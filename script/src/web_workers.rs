//! Web `Worker` support: off-thread compute realms with a message-only bridge.
//!
//! A smudgy worker is deliberately **not** a second scripting isolate. Every
//! session registry handed to `smudgy_ops` is `!Send` and lives on the session
//! thread, so a worker — which runs on its own OS thread inside deno_runtime's
//! worker host — carries no smudgy extension at all. Beyond that, the deno op
//! surface itself is reduced at registration time (see
//! [`worker_realm_guard_extension`]): the realm keeps `deno_webidl`,
//! `deno_web` (timers, streams, encoding, structured clone, MessagePort,
//! events, abort), and `deno_crypto` — pure compute — while the
//! net/fetch/websocket/http, fs, ffi/napi, process, kv/cron,
//! webgpu/canvas/image/cache, tty, os, and node op families are disabled so
//! authority is absent rather than merely denied. The worker also holds a
//! none-permissions container and an isolated BroadcastChannel backend;
//! `postMessage`/structured clone is its only I/O.
//!
//! Isolates without worker support get [`worker_host_denied_extension`]
//! instead: the worker-host ops themselves are disabled, so `new Worker(...)`
//! is a catchable error before any thread spawns — including through the
//! `node:worker_threads` shim, which reaches the same `op_create_worker`.
//! Without it, deno_runtime's default `create_web_worker_cb` is
//! `unimplemented!()`: every module-worker construction spawns an OS thread
//! that immediately panics.

use std::rc::Rc;
use std::sync::Arc;

use deno_core::error::ModuleLoaderError;
use deno_core::{
    Extension, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType, ResolutionKind,
};
use deno_error::JsErrorBox;
use deno_fs::RealFs;
use deno_permissions::{Permissions, PermissionsContainer, RuntimePermissionDescriptorParser};
use deno_resolver::npm::{DenoInNpmPackageChecker, NpmResolver};
use deno_runtime::deno_inspector_server::MainInspectorSessionChannel;
use deno_runtime::ops::worker_host::{CreateWebWorkerArgs, CreateWebWorkerCb};
use deno_runtime::web_worker::{WebWorker, WebWorkerOptions, WebWorkerServiceOptions};
use sys_traits::impls::RealSys;

use crate::transpiler::transpile;

/// Whether an isolate may construct Web `Worker`s, and in what shape.
///
/// This is per-isolate policy chosen by the isolate factory: the trusted main
/// isolate gets `ComputeOnly`; a sandboxed package isolate gets `ComputeOnly`
/// only with the consented `workers` capability, `Disabled` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerMode {
    /// Worker-host ops are disabled: `new Worker(...)` (and the
    /// `node:worker_threads` path) throws a catchable error with no thread
    /// spawn and no panic.
    #[default]
    Disabled,
    /// Workers are reduced-op compute realms with a message-only bridge.
    ComputeOnly,
}

/// Worker-host ops disabled in isolates whose [`WorkerMode`] is `Disabled`.
///
/// `op_create_worker` is the single choke point both `new Worker(...)` and
/// `node:worker_threads` funnel through; the `op_host_*` companions are the
/// parent-side halves of the worker bridge and are inert without it, but are
/// disabled too so no half-constructed handle path stays reachable.
const DENIED_WORKER_HOST_OPS: &[&str] = &["op_create_worker"];
const DENIED_WORKER_HOST_OP_PREFIXES: &[&str] = &["op_host_", "op_node_worker_thread_"];

/// Op families absent from a worker realm (`ComputeOnly` reduction).
///
/// Deny-by-family over op names, applied at registration via extension
/// middleware, so the authority is absent from the realm rather than gated
/// behind a permission check. Kept: core, webidl, web (timers, streams,
/// encoding, structured clone, MessagePort, events, abort, blobs), crypto,
/// the worker's own bridge ops (`op_worker_*`, minus its classic-worker sync
/// fetch), bootstrap/runtime plumbing, and permission-query ops (which report
/// against the none-permissions container).
const DENIED_WORKER_REALM_OPS: &[&str] = &[
    // Classic-worker `importScripts` support performs network/file I/O.
    "op_worker_sync_fetch",
    // deno_os one-offs without a shared prefix.
    "op_delete_env",
    "op_env",
    "op_exec_path",
    "op_exit",
    "op_get_env",
    "op_get_env_no_permission_check",
    "op_gid",
    "op_hostname",
    "op_loadavg",
    "op_network_interfaces",
    "op_runtime_cpu_usage",
    "op_runtime_memory_usage",
    "op_set_env",
    "op_set_exit_code",
    "op_system_memory_info",
    "op_uid",
    // deno_tty (registered by deno_runtime).
    "op_set_raw",
    "op_console_size",
    "op_read_line_prompt",
    // deno_audio (registered in audio-snapshot workers purely for blob
    // compatibility — see create_compute_worker; workers get no audio).
    "op_decode_audio_data",
    "op_offline_start_rendering",
    "op_online_wait_events",
    // Nested workers: a worker may not construct workers in v1.
    "op_create_worker",
];
const DENIED_WORKER_REALM_OP_PREFIXES: &[&str] = &[
    "op_phase0_",
    "op_net_",
    "op_tls_",
    "op_ws_",
    "op_http_",
    "op_fetch",
    "op_fs_",
    "op_ffi_",
    "op_napi_",
    "op_spawn_",
    "op_kv_",
    "op_cron_",
    "op_webgpu_",
    "op_image_",
    "op_canvas_",
    "op_cache_",
    "op_webstorage_",
    "op_node_",
    "op_require_",
    "op_bundle_",
    "op_desktop_",
    "op_os_",
    "op_host_",
];

/// Authority-free ops exempted from the realm deny prefixes. Web-platform
/// paths lazily load deno_node internals inside a worker — `setInterval`
/// pulls in async_hooks → errors → uv → os, whose module scope calls
/// `op_node_build_os` — so the inert constants those module scopes need stay
/// enabled. Every entry here must be a pure constant/compute op; anything
/// that touches the system belongs in the deny list.
const WORKER_REALM_OP_EXCEPTIONS: &[&str] = &[
    "op_node_build_os",
    "op_node_fs_constants",
    "op_node_new_async_id",
];

fn is_denied(name: &str, exact: &[&str], prefixes: &[&str]) -> bool {
    if WORKER_REALM_OP_EXCEPTIONS.contains(&name) {
        return false;
    }
    exact.contains(&name) || prefixes.iter().any(|prefix| name.starts_with(prefix))
}

/// Middleware-only extension for isolates whose [`WorkerMode`] is `Disabled`:
/// turns every worker-host op into a catchable "op is disabled" error at
/// registration, before any JS runs.
pub fn worker_host_denied_extension() -> Extension {
    Extension {
        name: "smudgy_worker_host_denied",
        middleware_fn: Some(Box::new(|op| {
            if is_denied(
                op.name,
                DENIED_WORKER_HOST_OPS,
                DENIED_WORKER_HOST_OP_PREFIXES,
            ) {
                op.disable()
            } else {
                op
            }
        })),
        ..Default::default()
    }
}

/// Middleware-only extension appended to every worker realm: the op-surface
/// reduction described on [`DENIED_WORKER_REALM_OPS`].
fn worker_realm_guard_extension() -> Extension {
    Extension {
        name: "smudgy_worker_realm_guard",
        middleware_fn: Some(Box::new(|op| {
            if is_denied(
                op.name,
                DENIED_WORKER_REALM_OPS,
                DENIED_WORKER_REALM_OP_PREFIXES,
            ) {
                op.disable()
            } else {
                op
            }
        })),
        ..Default::default()
    }
}

/// Module loader for worker realms: `file:` modules (transpiled like the main
/// loader's file path) and `data:` JavaScript modules only. No npm/jsr/https,
/// no `smudgy:*` schemes — a worker imports no registry code and no smudgy
/// API. `data:` bodies are percent-decoded plain JavaScript (the common
/// inline-worker form); base64 `data:` URLs and `blob:` workers are not
/// supported in v1.
struct WorkerModuleLoader;

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = if bytes[i] == b'%' && i + 2 < bytes.len() {
            match (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                (Some(hi), Some(lo)) => Some((hi * 16 + lo) as u8),
                _ => None,
            }
        } else {
            None
        };
        if let Some(byte) = decoded {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn loader_error(message: String) -> ModuleLoaderError {
    JsErrorBox::generic(message).into()
}

impl WorkerModuleLoader {
    fn load_source(specifier: &ModuleSpecifier) -> Result<ModuleSource, ModuleLoaderError> {
        let source = match specifier.scheme() {
            "file" => {
                let path = specifier
                    .to_file_path()
                    .map_err(|_| loader_error(format!("{specifier} is not a file URL")))?;
                std::fs::read_to_string(&path).map_err(|e| {
                    loader_error(format!(
                        "failed to read worker module {}: {e}",
                        path.display()
                    ))
                })?
            }
            "data" => {
                let body = specifier.path();
                let (media_type, payload) = body.split_once(',').ok_or_else(|| {
                    loader_error(format!("malformed data URL worker module {specifier}"))
                })?;
                if media_type.ends_with(";base64") {
                    return Err(loader_error(
                        "base64 data URL worker modules are not supported; use a plain \
                         percent-encoded body or a file module"
                            .to_string(),
                    ));
                }
                percent_decode(payload)
            }
            scheme => {
                return Err(loader_error(format!(
                    "worker modules may only load file: or data: sources; \
                     {scheme}: is not available inside a worker ({specifier})"
                )));
            }
        };

        let (code, _source_map) = transpile(specifier, &source).map_err(JsErrorBox::from_err)?;
        Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(code.into()),
            specifier,
            None,
        ))
    }
}

impl ModuleLoader for WorkerModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        deno_core::resolve_import(specifier, referrer).map_err(|e| JsErrorBox::from_err(e).into())
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        ModuleLoadResponse::Sync(Self::load_source(specifier))
    }
}

/// The callback [`op_create_worker`] invokes **on the worker's own OS
/// thread**, inside the current-thread tokio runtime deno_runtime creates for
/// it, to construct the worker realm. Everything here is built fresh on that
/// thread; nothing session-bound (and nothing `!Send`) is captured.
///
/// `web_audio` records which snapshot the PARENT runtime booted. V8's shared
/// heap (string table included) is process-global: the first live isolate's
/// blob initializes it and every later `Isolate::New` verifies against it, so
/// a worker booting a different blob than its concurrently-live parent is a
/// fatal `Check failed: index < size()` inside snapshot deserialization. The
/// worker therefore always boots the parent's snapshot; on the audio blob it
/// registers a matching state-free deno_audio extension (options-default,
/// exactly like the snapshot build) whose ops the realm guard disables.
pub(crate) fn create_web_worker_callback(web_audio: bool) -> Arc<CreateWebWorkerCb> {
    Arc::new(move |args: CreateWebWorkerArgs| create_compute_worker(args, web_audio))
}

fn create_compute_worker(
    args: CreateWebWorkerArgs,
    web_audio: bool,
) -> (WebWorker, deno_runtime::web_worker::SendableWebWorkerHandle) {
    let parser = Arc::new(RuntimePermissionDescriptorParser::new(RealSys));
    // No authority, not inherited authority: the spawning isolate's forwarded
    // container (`args.permissions`) is deliberately unused — even the trusted
    // main isolate's workers hold a none-permissions container, because the
    // realm's contract is compute plus the message bridge, nothing else.
    let permissions = PermissionsContainer::new(parser, Permissions::none_without_prompt());

    let services = WebWorkerServiceOptions::<DenoInNpmPackageChecker, NpmResolver<RealSys>, RealSys> {
        blob_store: Arc::new(deno_web::BlobStore::default()),
        broadcast_channel: deno_web::InMemoryBroadcastChannel::default(),
        deno_rt_native_addon_loader: None,
        compiled_wasm_module_store: None,
        feature_checker: crate::quiet_feature_checker(),
        fs: Arc::new(RealFs),
        main_inspector_session_tx: MainInspectorSessionChannel::new(),
        module_loader: Rc::new(WorkerModuleLoader),
        node_services: None,
        npm_process_state_provider: None,
        permissions,
        root_cert_store_provider: None,
        shared_array_buffer_store: None,
        bundle_provider: None,
    };

    #[cfg(feature = "web-audio")]
    let (startup_snapshot, residual_lazy_js_sources, residual_lazy_esm_sources) = if web_audio {
        (
            crate::WEB_AUDIO_STARTUP_SNAPSHOT,
            crate::RESIDUAL_LAZY_AUDIO_JS_SOURCES,
            crate::RESIDUAL_LAZY_AUDIO_ESM_SOURCES,
        )
    } else {
        (
            crate::STARTUP_SNAPSHOT,
            crate::RESIDUAL_LAZY_JS_SOURCES,
            crate::RESIDUAL_LAZY_ESM_SOURCES,
        )
    };
    #[cfg(not(feature = "web-audio"))]
    let (startup_snapshot, residual_lazy_js_sources, residual_lazy_esm_sources) = {
        debug_assert!(!web_audio, "audio-snapshot parents require the web-audio feature");
        (
            crate::STARTUP_SNAPSHOT,
            crate::RESIDUAL_LAZY_JS_SOURCES,
            crate::RESIDUAL_LAZY_ESM_SOURCES,
        )
    };

    // On the audio blob the snapshot's frozen extension prefix ends with
    // deno_audio, so the worker registers a matching instance first — deferred
    // ESM never imported (no bootstrap side-module runs in workers), ops
    // disabled by the realm guard, default (state-free) options exactly like
    // the snapshot build's instance.
    let mut extensions = Vec::new();
    #[cfg(feature = "web-audio")]
    if web_audio {
        let mut audio =
            deno_audio::deno_audio::init(deno_audio::AudioExtensionOptions::default());
        crate::prepare_deferred_web_audio_extension(&mut audio);
        extensions.push(audio);
    }
    extensions.push(worker_realm_guard_extension());

    let mut options = WebWorkerOptions {
        name: args.name,
        main_module: args.main_module.clone(),
        worker_id: args.worker_id,
        bootstrap: Default::default(),
        extensions,
        startup_snapshot: Some(startup_snapshot),
        residual_lazy_js_sources,
        residual_lazy_esm_sources,
        unsafely_ignore_certificate_errors: None,
        create_params: None,
        seed: None,
        // Unreachable: the realm guard disables `op_create_worker`, so a
        // nested construction throws before this callback could be invoked.
        create_web_worker_cb: Arc::new(|_| {
            unreachable!("op_create_worker is disabled inside smudgy worker realms")
        }),
        format_js_error_fn: None,
        worker_type: args.worker_type,
        cache_storage_dir: None,
        stdio: Default::default(),
        trace_ops: None,
        close_on_idle: args.close_on_idle,
        maybe_worker_metadata: args.maybe_worker_metadata,
        maybe_main_module_blob: args.maybe_main_module_blob,
        maybe_coverage_dir: None,
        maybe_cpu_prof_config: None,
        enable_raw_imports: false,
        enable_stack_trace_arg_in_ops: false,
        wait_for_debugger_on_start: args.wait_for_debugger_on_start,
        wait_for_page_wait_for_debugger: args.wait_for_page_wait_for_debugger,
    };
    options.bootstrap.location = Some(args.main_module);
    options.bootstrap.has_node_modules_dir = false;

    WebWorker::bootstrap_from_options(services, options)
}
