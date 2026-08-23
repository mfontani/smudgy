//! Web `Worker` support: reduced-op compute realms with a message-only bridge.
//!
//! Workers run on their own OS threads inside deno_runtime's worker host, so
//! these tests exercise real cross-thread construction, messaging, reduction,
//! and termination against `WorkerMode::ComputeOnly`, plus the `Disabled`
//! denial path (which must be a catchable error, never a panic or thread
//! spawn — deno_runtime's default callback panics a fresh thread per call).

use std::path::Path;
use std::rc::Rc;

use anyhow::Result;
use deno_core::{FastString, PollEventLoopOptions, serde_v8};
use smudgy_script::{
    ImportPolicy, ModulePolicy, ScriptRuntime, ScriptRuntimeOptions, WorkerMode,
};

fn tokio_runtime() -> Rc<tokio::runtime::Runtime> {
    Rc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    )
}

/// Multi-runtime tests share one thread, so each V8 operation must temporarily
/// make its runtime's isolate current (the production ScriptEngine does the same).
struct EnteredIsolate(*mut deno_core::v8::OwnedIsolate);

impl EnteredIsolate {
    fn enter(runtime: &mut ScriptRuntime) -> Self {
        let isolate = runtime.deno_runtime().v8_isolate();
        // SAFETY: the runtime owns this live isolate; Drop balances this enter.
        unsafe { (*isolate).enter() };
        Self(isolate)
    }
}

impl Drop for EnteredIsolate {
    fn drop(&mut self) {
        // SAFETY: balanced with enter; no other isolate operation is interleaved.
        unsafe { (*self.0).exit() };
    }
}

fn script_runtime(
    data_dir: &Path,
    workers: WorkerMode,
) -> Result<(Rc<tokio::runtime::Runtime>, ScriptRuntime)> {
    let tokio = tokio_runtime();
    let runtime = ScriptRuntime::new(ScriptRuntimeOptions {
        extensions: Vec::new(),
        data_dir: data_dir.to_path_buf(),
        webstorage_dir: None,
        module_policy: ModulePolicy {
            allow_https: true,
            import_policy: ImportPolicy::Any,
        },
        inspector: None,
        tokio: tokio.clone(),
        package_provider: None,
        permissions: None,
        broadcast_channel: None,
        workers,
    })?;
    Ok((tokio, runtime))
}

fn eval_async_bool(
    tokio: &tokio::runtime::Runtime,
    rt: &mut ScriptRuntime,
    source: &str,
) -> Result<bool> {
    let _entered = EnteredIsolate::enter(rt);
    tokio.block_on(async {
        let value = rt
            .deno_runtime()
            .execute_script("<test>", FastString::from(source.to_string()))?;
        let promise = rt.deno_runtime().resolve(value);
        let value = rt
            .deno_runtime()
            .with_event_loop_future(promise, PollEventLoopOptions::default())
            .await?;
        deno_core::scope!(scope, rt.deno_runtime());
        let local = deno_core::v8::Local::new(scope, value);
        Ok(serde_v8::from_v8(scope, local)?)
    })
}

/// Wrap a worker-module body into a `data:` URL Worker construction whose
/// parent-side promise resolves with the worker's first message and rejects on
/// worker error or a 15s timeout.
fn first_message_source(worker_body: &str, post: &str) -> String {
    format!(
        r#"(async () => {{
            const body = {worker_body:?};
            const worker = new Worker(
                "data:text/javascript," + encodeURIComponent(body),
                {{ type: "module" }},
            );
            try {{
                const result = await new Promise((resolve, reject) => {{
                    worker.onmessage = (e) => resolve(e.data);
                    worker.onerror = (e) => {{
                        e.preventDefault();
                        reject(new Error("worker error: " + e.message));
                    }};
                    {post}
                    setTimeout(() => reject(new Error("worker timed out")), 15000);
                }});
                return result;
            }} finally {{
                worker.terminate();
            }}
        }})()"#
    )
}

/// A round trip through a real worker: the parent posts a value, worker-side
/// `onmessage` doubles it, and the parent observes the reply. Proves thread
/// spawn, snapshot boot, the data-URL loader, and both bridge directions.
#[test]
fn worker_echo_round_trip() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut rt) = script_runtime(temp.path(), WorkerMode::ComputeOnly)?;
    let source = first_message_source(
        "onmessage = (e) => { postMessage(e.data * 2); };",
        "worker.postMessage(21);",
    );
    let ok = eval_async_bool(&tokio, &mut rt, &format!("{source}.then((v) => v === 42)"))?;
    assert!(ok, "worker echo round trip should double the value");
    Ok(())
}

/// TypeScript worker modules load through the same transpile path as the main
/// loader's file modules.
#[test]
fn worker_file_module_transpiles_typescript() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let module_path = temp.path().join("worker.ts");
    std::fs::write(
        &module_path,
        "const double = (n: number): number => n * 2;\nonmessage = (e) => { postMessage(double(e.data)); };\n",
    )?;
    let module_url = deno_core::url::Url::from_file_path(&module_path)
        .expect("temp path converts to a file URL");
    let (tokio, mut rt) = script_runtime(temp.path(), WorkerMode::ComputeOnly)?;
    let source = format!(
        r#"(async () => {{
            const worker = new Worker({:?}, {{ type: "module" }});
            try {{
                return await new Promise((resolve, reject) => {{
                    worker.onmessage = (e) => resolve(e.data === 34);
                    worker.onerror = (e) => {{
                        e.preventDefault();
                        reject(new Error("worker error: " + e.message));
                    }};
                    worker.postMessage(17);
                    setTimeout(() => reject(new Error("worker timed out")), 15000);
                }});
            }} finally {{
                worker.terminate();
            }}
        }})()"#,
        module_url.as_str(),
    );
    let ok = eval_async_bool(&tokio, &mut rt, &source)?;
    assert!(ok, "a TypeScript file worker should transpile and run");
    Ok(())
}

/// The reduced realm: fetch, fs, env/os, and nested worker construction are
/// absent (disabled at op registration), while pure-compute web platform
/// pieces — structuredClone and crypto — work. The worker probes its own
/// realm and posts the observations back.
#[test]
fn worker_realm_is_reduced_to_compute() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut rt) = script_runtime(temp.path(), WorkerMode::ComputeOnly)?;
    let worker_body = r#"
        onmessage = async () => {
            const denied = async (fn) => {
                try { await fn(); return false; } catch { return true; }
            };
            const probes = {
                fetchDenied: await denied(() => fetch("https://example.invalid/")),
                fsDenied: await denied(() => Deno.readTextFile("does-not-matter.txt")),
                envDenied: await denied(() => Deno.env.get("PATH")),
                nestedWorkerDenied: await denied(() => {
                    new Worker("data:text/javascript,", { type: "module" });
                }),
                structuredCloneWorks: structuredClone({ n: 9 }).n === 9,
                cryptoWorks: crypto.getRandomValues(new Uint8Array(8)).length === 8,
            };
            postMessage(probes);
        };
    "#;
    let source = first_message_source(worker_body, "worker.postMessage(null);");
    let ok = eval_async_bool(
        &tokio,
        &mut rt,
        &format!(
            "{source}.then((p) => p.fetchDenied && p.fsDenied && p.envDenied \
             && p.nestedWorkerDenied && p.structuredCloneWorks && p.cryptoWorks)"
        ),
    )?;
    assert!(ok, "the worker realm should be compute + messaging only");
    Ok(())
}

/// `terminate()` retires a live worker and the parent event loop settles —
/// i.e. no worker-host op or thread keeps the runtime alive afterwards.
#[test]
fn worker_terminate_settles() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut rt) = script_runtime(temp.path(), WorkerMode::ComputeOnly)?;
    let source = r#"(async () => {
        const worker = new Worker(
            "data:text/javascript," +
                encodeURIComponent("onmessage = () => { postMessage(1); };"),
            { type: "module" },
        );
        const first = await new Promise((resolve, reject) => {
            worker.onmessage = (e) => resolve(e.data);
            worker.onerror = (e) => {
                e.preventDefault();
                reject(new Error("worker error: " + e.message));
            };
            worker.postMessage(null);
            setTimeout(() => reject(new Error("worker timed out")), 15000);
        });
        worker.terminate();
        return first === 1;
    })()"#;
    let ok = eval_async_bool(&tokio, &mut rt, source)?;
    assert!(ok, "terminate should retire the worker cleanly");
    Ok(())
}

/// `WorkerMode::Disabled`: construction is a catchable error (the worker-host
/// ops are disabled at registration) — no panic, no thread spawn. This is the
/// sandboxed-package posture until the W2 `workers` capability.
#[test]
fn disabled_mode_makes_worker_a_catchable_error() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut rt) = script_runtime(temp.path(), WorkerMode::Disabled)?;
    let source = r#"(async () => {
        try {
            new Worker("data:text/javascript,", { type: "module" });
            return false;
        } catch {
            return true;
        }
    })()"#;
    let ok = eval_async_bool(&tokio, &mut rt, source)?;
    assert!(ok, "a disabled isolate's Worker construction should throw");
    Ok(())
}

/// Classic workers stay rejected in worker-enabled isolates (matches Deno).
#[test]
fn classic_workers_stay_rejected() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut rt) = script_runtime(temp.path(), WorkerMode::ComputeOnly)?;
    let source = r#"(async () => {
        try {
            new Worker("data:text/javascript,");
            return false;
        } catch (e) {
            return e instanceof DOMException && e.name === "NotSupportedError";
        }
    })()"#;
    let ok = eval_async_bool(&tokio, &mut rt, source)?;
    assert!(ok, "classic workers should reject with NotSupportedError");
    Ok(())
}
