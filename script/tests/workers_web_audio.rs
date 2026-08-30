//! Regression tests for workers beside a live Web Audio parent.
//!
//! Before snapshot selection became build-wide, a worker could use the base snapshot
//! while its parent used the Web Audio snapshot. V8 then terminated with `index < size()`
//! during shared-heap deserialization. Every isolate in this binary now uses the Web
//! Audio snapshot. Workers register deno_audio for compatibility but do not expose its API.
#![cfg(feature = "web-audio")]

use std::path::Path;
use std::rc::Rc;

use anyhow::Result;
use deno_core::{FastString, PollEventLoopOptions, serde_v8};
use smudgy_script::{ImportPolicy, ModulePolicy, ScriptRuntime, ScriptRuntimeOptions, WorkerMode};

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

/// Creates the runtime shape used by a session with Web Audio and trusted workers.
fn audio_script_runtime(data_dir: &Path) -> Result<(Rc<tokio::runtime::Runtime>, ScriptRuntime)> {
    let mut audio = deno_audio::deno_audio::init(deno_audio::AudioExtensionOptions::default());
    smudgy_script::prepare_deferred_web_audio_extension(&mut audio);
    let tokio = tokio_runtime();
    let runtime = ScriptRuntime::new(ScriptRuntimeOptions {
        extensions: vec![audio],
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
        workers: WorkerMode::TrustedComputeOnly,
        max_live_workers_override: None,
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

/// Keeps the Web Audio parent live while a worker starts, exchanges a message, and stops.
#[test]
fn worker_boots_beside_a_live_audio_snapshot_parent() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut rt) = audio_script_runtime(temp.path())?;
    let source = r#"(async () => {
        const worker = new Worker(
            "data:text/javascript," +
                encodeURIComponent("onmessage = (e) => { postMessage(e.data * 2); };"),
            { type: "module" },
        );
        try {
            return await new Promise((resolve, reject) => {
                worker.onmessage = (e) => resolve(e.data === 42);
                worker.onerror = (e) => {
                    e.preventDefault();
                    reject(new Error("worker error: " + e.message));
                };
                worker.postMessage(21);
                setTimeout(() => reject(new Error("worker timed out")), 15000);
            });
        } finally {
            worker.terminate();
        }
    })()"#;
    let ok = eval_async_bool(&tokio, &mut rt, source)?;
    assert!(
        ok,
        "a worker beside an audio-snapshot parent echoes normally"
    );
    Ok(())
}

/// Verifies that snapshot compatibility does not expose Web Audio in the worker.
///
/// The worker registers deno_audio to match the snapshot sequence. It does not evaluate
/// the ESM entry point, and the realm guard disables the native audio operations.
#[test]
fn audio_blob_worker_has_no_audio_surface() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut rt) = audio_script_runtime(temp.path())?;
    let worker_body = r#"
        onmessage = () => {
            postMessage({
                contextAbsent: typeof globalThis.AudioContext === "undefined",
                offlineAbsent: typeof globalThis.OfflineAudioContext === "undefined",
                bufferAbsent: typeof globalThis.AudioBuffer === "undefined",
            });
        };
    "#;
    let source = format!(
        r#"(async () => {{
            const worker = new Worker(
                "data:text/javascript," + encodeURIComponent({worker_body:?}),
                {{ type: "module" }},
            );
            try {{
                const p = await new Promise((resolve, reject) => {{
                    worker.onmessage = (e) => resolve(e.data);
                    worker.onerror = (e) => {{
                        e.preventDefault();
                        reject(new Error("worker error: " + e.message));
                    }};
                    worker.postMessage(null);
                    setTimeout(() => reject(new Error("worker timed out")), 15000);
                }});
                return p.contextAbsent && p.offlineAbsent && p.bufferAbsent;
            }} finally {{
                worker.terminate();
            }}
        }})()"#
    );
    let ok = eval_async_bool(&tokio, &mut rt, &source)?;
    assert!(ok, "no Web Audio globals inside an audio-blob worker");
    Ok(())
}
