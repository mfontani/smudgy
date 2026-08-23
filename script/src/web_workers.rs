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

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use deno_core::error::ModuleLoaderError;
use deno_core::{
    Extension, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleSource, ModuleSourceCode, ModuleSpecifier, OpMiddlewareContext, ResolutionKind,
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
/// This is per-isolate policy chosen by the isolate factory. Trusted and
/// sandboxed parents deliberately remain distinct because loading a worker's
/// source is embedder-owned authority, separate from the child's denied ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerMode {
    /// Worker-host ops are disabled: `new Worker(...)` (and the
    /// `node:worker_threads` path) throws a catchable error with no thread
    /// spawn and no panic.
    #[default]
    Disabled,
    /// Trusted user/local code may load local worker modules as well as inline
    /// and snapshotted package sources.
    TrustedComputeOnly,
    /// A consented sandbox package may load only inline and immutable
    /// `smudgy-pkg:` closure sources, never ambient host files.
    SandboxedComputeOnly,
}

impl WorkerMode {
    #[must_use]
    pub(crate) fn enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    #[must_use]
    fn allows_file_modules(self) -> bool {
        matches!(self, Self::TrustedComputeOnly)
    }

    #[must_use]
    pub(crate) fn max_live_workers(self, requested: Option<usize>) -> usize {
        if self.enabled() {
            requested.unwrap_or(WORKER_CAP).min(WORKER_CAP)
        } else {
            0
        }
    }
}

/// Per-parent live-worker ceiling, enforced natively by deno_runtime before
/// `op_create_worker` detaches transferables or spawns an OS thread.
pub(crate) const WORKER_CAP: usize = 128;

/// Worker-host ops disabled in isolates whose [`WorkerMode`] is `Disabled`.
///
/// `op_create_worker` is the single choke point both `new Worker(...)` and
/// `node:worker_threads` funnel through; the `op_host_*` companions are the
/// parent-side halves of the worker bridge and are inert without it, but are
/// disabled too so no half-constructed handle path stays reachable.
const DENIED_WORKER_HOST_OPS: &[&str] = &["op_create_worker"];
const DENIED_WORKER_HOST_OP_PREFIXES: &[&str] = &["op_host_", "op_node_worker_thread_"];

/// Process-wide controls are never appropriate for an embedded script realm,
/// including the trusted main realm: a script error must not terminate or
/// install signal handlers on the Smudgy host process.
const DENIED_EMBEDDED_PROCESS_OPS: &[&str] = &["op_exit", "op_kill", "op_set_exit_code"];
const DENIED_EMBEDDED_PROCESS_OP_PREFIXES: &[&str] = &["op_signal_"];

/// Core V8/runtime plumbing needed by the web/crypto compute surface. This is
/// an allowlist, not a denylist: generic resource I/O (`op_read`, `op_write`,
/// `op_close`), process stdout (`op_print`), resource enumeration, and the
/// intentional Rust panic op are absent.
const ALLOWED_WORKER_CORE_OPS: &[&str] = &[
    "op_wasm_streaming_feed",
    "op_wasm_streaming_set_url",
    "op_str_byte_length",
    "op_cancel_handle",
    "op_encode_binary_string",
    "op_import_sync",
    "op_import_sync_with_source",
    "op_is_any_array_buffer",
    "op_is_arguments_object",
    "op_is_array_buffer",
    "op_is_array_buffer_view",
    "op_is_async_function",
    "op_is_big_int_object",
    "op_is_boolean_object",
    "op_is_boxed_primitive",
    "op_is_data_view",
    "op_is_date",
    "op_is_generator_function",
    "op_is_generator_object",
    "op_is_map",
    "op_is_map_iterator",
    "op_is_module_namespace_object",
    "op_is_native_error",
    "op_is_number_object",
    "op_is_promise",
    "op_is_proxy",
    "op_is_reg_exp",
    "op_is_set",
    "op_is_set_iterator",
    "op_is_shared_array_buffer",
    "op_is_string_object",
    "op_is_symbol_object",
    "op_is_typed_array",
    "op_is_weak_map",
    "op_is_weak_set",
    "op_add_main_module_handler",
    "op_set_handled_promise_rejection_handler",
    "op_timer_schedule",
    "op_timer_track",
    "op_timer_untrack",
    "op_timer_now",
    "op_ref_op",
    "op_unref_op",
    "op_lazy_load_esm",
    "op_load_ext_script",
    "op_set_captured_bootstrap",
    "op_run_microtasks",
    "op_drain_pending_rejections",
    "op_compile_function",
    "op_eval_context",
    "op_encode",
    "op_decode",
    "op_serialize",
    "op_deserialize",
    "op_structured_clone",
    "op_set_promise_hooks",
    "op_get_promise_details",
    "op_get_proxy_details",
    "op_get_non_index_property_names",
    "op_get_constructor_name",
    "op_get_extras_binding_object",
    "op_memory_usage",
    "op_set_wasm_streaming_callback",
    "op_abort_wasm_streaming",
    "op_destructure_error",
    "op_dispatch_exception",
    "op_op_names",
    "op_current_user_call_site",
    "op_set_format_exception_callback",
    "op_event_loop_has_more_work",
    "op_immediate_check",
    "op_leak_tracing_enable",
    "op_leak_tracing_submit",
    "op_leak_tracing_get_all",
    "op_leak_tracing_get",
    "op_get_ext_import_meta_proto",
];

const ALLOWED_WORKER_RUNTIME_OPS: &[&str] = &["op_main_module"];
const ALLOWED_WORKER_PERMISSION_OPS: &[&str] = &[
    "op_query_permission",
    "op_revoke_permission",
    "op_request_permission",
];
const ALLOWED_WORKER_BOOTSTRAP_OPS: &[&str] = &[
    "op_bootstrap_args",
    "op_bootstrap_pid",
    "op_bootstrap_numcpus",
    "op_bootstrap_user_agent",
    "op_bootstrap_language",
    "op_bootstrap_log_level",
    "op_bootstrap_color_depth",
    "op_bootstrap_no_color",
    "op_bootstrap_stdout_no_color",
    "op_bootstrap_stderr_no_color",
    "op_bootstrap_unstable_args",
    "op_bootstrap_is_from_unconfigured_runtime",
    "op_proto_set_attempted",
    "op_proto_get_attempted",
    "op_snapshot_options",
];
const ALLOWED_WORKER_BRIDGE_OPS: &[&str] = &[
    "op_worker_post_message",
    "op_worker_post_message_raw",
    "op_worker_recv_message",
    "op_worker_recv_message_sync",
    "op_worker_maybe_wait_for_debugger",
    "op_worker_close",
    "op_worker_get_type",
];

/// Pure constants needed when web-platform modules lazily initialize a small
/// part of deno_node. Native object methods from deno_node remain disabled.
const ALLOWED_WORKER_NODE_OPS: &[&str] = &[
    "op_node_build_os",
    "op_node_fs_constants",
    "op_node_new_async_id",
];

fn is_denied(name: &str, exact: &[&str], prefixes: &[&str]) -> bool {
    exact.contains(&name) || prefixes.iter().any(|prefix| name.starts_with(prefix))
}

fn is_worker_realm_op_allowed(context: OpMiddlewareContext, name: &str) -> bool {
    // Native object names are not globally unique (`open`, `close`, and
    // `constructor` occur across safe and authority-bearing APIs). Provenance
    // comes from deno_core's context-aware middleware hook.
    if context.method_kind.is_some() {
        return matches!(context.extension_name, "deno_web" | "deno_crypto");
    }

    match context.extension_name {
        "deno_core" => ALLOWED_WORKER_CORE_OPS.contains(&name),
        // These extensions are the intentionally retained Web Platform
        // compute surface. Their backends are per-worker/in-memory.
        "deno_web" | "deno_crypto" => true,
        "deno_runtime" => ALLOWED_WORKER_RUNTIME_OPS.contains(&name),
        "deno_permissions" => ALLOWED_WORKER_PERMISSION_OPS.contains(&name),
        "deno_bootstrap" => ALLOWED_WORKER_BOOTSTRAP_OPS.contains(&name),
        "deno_web_worker" => ALLOWED_WORKER_BRIDGE_OPS.contains(&name),
        "deno_node" => ALLOWED_WORKER_NODE_OPS.contains(&name),
        // Every unrecognized extension/op is disabled, including future Deno
        // additions until they receive an explicit security review.
        _ => false,
    }
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

/// Baseline process-integrity guard for every embedded Smudgy runtime. This is
/// separate from filesystem/network permissions because `op_kill` explicitly
/// permits killing the current process without a run grant in upstream Deno.
pub(crate) fn embedded_process_guard_extension() -> Extension {
    Extension {
        name: "smudgy_embedded_process_guard",
        middleware_fn: Some(Box::new(|op| {
            if is_denied(
                op.name,
                DENIED_EMBEDDED_PROCESS_OPS,
                DENIED_EMBEDDED_PROCESS_OP_PREFIXES,
            ) {
                op.disable()
            } else {
                op
            }
        })),
        ..Default::default()
    }
}

/// Middleware-only extension appended to every worker realm: a fail-closed,
/// provenance-aware op allowlist covering regular ops and native object
/// constructors/methods/static methods.
fn worker_realm_guard_extension() -> Extension {
    Extension {
        name: "smudgy_worker_realm_guard",
        context_middleware_fn: Some(Box::new(|context, op| {
            if is_worker_realm_op_allowed(context, op.name) {
                op
            } else {
                op.disable()
            }
        })),
        ..Default::default()
    }
}

/// Immutable package sources published by a parent isolate after its initial module graph
/// has loaded. The provider that supplied them is `!Send`; this owned map is the intentionally
/// narrow, `Send` channel into off-thread worker realms.
pub(crate) type WorkerModuleSourceChannel = Arc<OnceLock<HashMap<String, String>>>;

/// Module loader for worker realms: `file:` modules (transpiled like the main loader's file
/// path), `data:` JavaScript modules, and canonical `smudgy-pkg:` sources snapshotted from the
/// parent isolate. No npm/jsr/https or smudgy virtual/API schemes. `data:` bodies are
/// percent-decoded plain JavaScript (the common inline-worker form); base64 `data:` URLs and
/// `blob:` workers are not supported in v1.
struct WorkerModuleLoader {
    package_sources: WorkerModuleSourceChannel,
    allow_file_modules: bool,
}

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
    JsErrorBox::generic(message)
}

impl WorkerModuleLoader {
    fn ensure_scheme_allowed(&self, specifier: &ModuleSpecifier) -> Result<(), ModuleLoaderError> {
        if specifier.scheme() == "file" {
            if !self.allow_file_modules {
                return Err(loader_error(format!(
                    "sandboxed worker modules may not load host files: {specifier}"
                )));
            }
            if specifier
                .host_str()
                .is_some_and(|host| !host.is_empty() && !host.eq_ignore_ascii_case("localhost"))
            {
                return Err(loader_error(format!(
                    "non-local file worker modules are not supported: {specifier}"
                )));
            }
        }
        Ok(())
    }

    fn load_source(&self, specifier: &ModuleSpecifier) -> Result<ModuleSource, ModuleLoaderError> {
        self.ensure_scheme_allowed(specifier)?;
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
            crate::package_resolver::CANONICAL_SCHEME => self
                .package_sources
                .get()
                .and_then(|sources| sources.get(specifier.as_str()))
                .cloned()
                .ok_or_else(|| {
                    loader_error(format!(
                        "package worker module was not present in the parent isolate's source snapshot: {specifier}"
                    ))
                })?,
            scheme => {
                return Err(loader_error(format!(
                    "worker modules may only load file:, data:, or snapshotted smudgy-pkg: sources; \
                     {scheme}: is not available inside a worker ({specifier})"
                )));
            }
        };

        let (code, _source_map) = transpile(specifier, &source).map_err(JsErrorBox::from_err)?;
        Ok(ModuleSource::new(
            crate::package_resolver::module_type_for(specifier.path()),
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
        let resolved =
            deno_core::resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)?;
        self.ensure_scheme_allowed(&resolved)?;
        Ok(resolved)
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        ModuleLoadResponse::Sync(self.load_source(specifier))
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
pub(crate) fn create_web_worker_callback(
    web_audio: bool,
    package_sources: WorkerModuleSourceChannel,
    mode: WorkerMode,
) -> Arc<CreateWebWorkerCb> {
    debug_assert!(mode.enabled());
    Arc::new(move |args: CreateWebWorkerArgs| {
        create_compute_worker(args, web_audio, Arc::clone(&package_sources), mode)
    })
}

fn create_compute_worker(
    args: CreateWebWorkerArgs,
    web_audio: bool,
    package_sources: WorkerModuleSourceChannel,
    mode: WorkerMode,
) -> (WebWorker, deno_runtime::web_worker::SendableWebWorkerHandle) {
    let parser = Arc::new(RuntimePermissionDescriptorParser::new(RealSys));
    // No authority, not inherited authority: the spawning isolate's forwarded
    // container (`args.permissions`) is deliberately unused — even the trusted
    // main isolate's workers hold a none-permissions container, because the
    // realm's contract is compute plus the message bridge, nothing else.
    let permissions = PermissionsContainer::new(parser, Permissions::none_without_prompt());

    let services =
        WebWorkerServiceOptions::<DenoInNpmPackageChecker, NpmResolver<RealSys>, RealSys> {
            blob_store: Arc::new(deno_web::BlobStore::default()),
            broadcast_channel: deno_web::InMemoryBroadcastChannel::default(),
            deno_rt_native_addon_loader: None,
            compiled_wasm_module_store: None,
            feature_checker: crate::quiet_feature_checker(),
            fs: Arc::new(RealFs),
            main_inspector_session_tx: MainInspectorSessionChannel::new(),
            module_loader: Rc::new(WorkerModuleLoader {
                package_sources,
                allow_file_modules: mode.allows_file_modules(),
            }),
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
        debug_assert!(
            !web_audio,
            "audio-snapshot parents require the web-audio feature"
        );
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
    let extensions = vec![worker_realm_guard_extension()];
    #[cfg(feature = "web-audio")]
    let extensions = if web_audio {
        let mut audio = deno_audio::deno_audio::init(deno_audio::AudioExtensionOptions::default());
        crate::prepare_deferred_web_audio_extension(&mut audio);
        let mut extensions = extensions;
        extensions.insert(0, audio);
        extensions
    } else {
        extensions
    };

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
        max_live_workers: Some(0),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_and_unc_file_urls_are_rejected_before_path_conversion() {
        let sandboxed = WorkerModuleLoader {
            package_sources: Arc::new(OnceLock::new()),
            allow_file_modules: false,
        };
        let local = ModuleSpecifier::parse("file:///host/secret.json").unwrap();
        assert!(sandboxed.ensure_scheme_allowed(&local).is_err());

        let trusted = WorkerModuleLoader {
            package_sources: Arc::new(OnceLock::new()),
            allow_file_modules: true,
        };
        let unc = ModuleSpecifier::parse("file://unreachable.invalid/share/worker.js").unwrap();
        assert!(trusted.ensure_scheme_allowed(&unc).is_err());
    }
}
