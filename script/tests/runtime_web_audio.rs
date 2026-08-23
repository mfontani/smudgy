//! Audio-snapshot runtime selection, in its own test binary ON PURPOSE.
//!
//! V8's shared heap (string table included) is process-global: the first live
//! isolate's snapshot blob initializes it and every later `Isolate::New`
//! verifies against it. `runtime.rs` is full of base-blob isolates running in
//! parallel test threads, so a concurrently-live AUDIO-blob isolate there is
//! an intermittent process crash (access violation / `Check failed:
//! index < size()` depending on timing) — observed deterministically on
//! high-core machines while low-core CI scheduling masked it. Each cargo
//! integration test file is its own process; every isolate in this one boots
//! the audio blob.
#![cfg(feature = "web-audio")]

use std::path::Path;
use std::rc::Rc;

use anyhow::Result;
use deno_core::{FastString, serde_v8};
use smudgy_script::{ImportPolicy, ModulePolicy, ScriptRuntime, ScriptRuntimeOptions, WorkerMode};

fn tokio_runtime() -> Rc<tokio::runtime::Runtime> {
    Rc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    )
}

fn deferred_audio_extension() -> deno_core::Extension {
    let mut extension = deno_audio::deno_audio::init(deno_audio::AudioExtensionOptions::default());
    smudgy_script::prepare_deferred_web_audio_extension(&mut extension);
    extension
}

fn audio_runtime(data_dir: &Path) -> Result<(Rc<tokio::runtime::Runtime>, ScriptRuntime)> {
    let tokio = tokio_runtime();
    let runtime = ScriptRuntime::new(ScriptRuntimeOptions {
        extensions: vec![deferred_audio_extension()],
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
        workers: WorkerMode::Disabled,
        max_live_workers_override: None,
    })?;
    Ok((tokio, runtime))
}

fn eval_bool(
    tokio: &tokio::runtime::Runtime,
    rt: &mut ScriptRuntime,
    source: &str,
) -> Result<bool> {
    tokio.block_on(async {
        let value = rt
            .deno_runtime()
            .execute_script("<test>", FastString::from(source.to_string()))?;
        deno_core::scope!(scope, rt.deno_runtime());
        let local = deno_core::v8::Local::new(scope, value);
        Ok(serde_v8::from_v8(scope, local)?)
    })
}

/// A deno_audio extension at custom index 0 selects the audio startup snapshot
/// and its deferred entry point installs the Web Audio globals.
#[test]
fn exact_audio_extension_selects_audio_snapshot_and_installs_globals() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut runtime) = audio_runtime(temp.path())?;
    assert!(eval_bool(
        &tokio,
        &mut runtime,
        "typeof globalThis.AudioContext === 'function' && typeof globalThis.OfflineAudioContext === 'function'",
    )?);
    Ok(())
}
