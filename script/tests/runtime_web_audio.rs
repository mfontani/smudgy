//! Tests Web Audio exposure and build-wide startup snapshot compatibility.
//!
//! Every isolate in this binary uses the Web Audio snapshot. This includes helper
//! runtimes, such as the declaration generator, that do not expose the Web Audio API.
#![cfg(feature = "web-audio")]

use std::collections::BTreeMap;
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

/// Verifies that an explicit deno_audio extension installs the Web Audio globals.
#[test]
fn exact_audio_extension_installs_globals_on_audio_snapshot() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (tokio, mut runtime) = audio_runtime(temp.path())?;
    assert!(eval_bool(
        &tokio,
        &mut runtime,
        "typeof globalThis.AudioContext === 'function' && typeof globalThis.OfflineAudioContext === 'function'",
    )?);
    Ok(())
}

/// Runs declaration generation on another thread while a Web Audio isolate remains live.
///
/// Before snapshot selection became build-wide, the generator used the base snapshot.
/// V8 then terminated with `index < size()` while deserializing its shared heap. The
/// separate thread preserves the publish-time execution path that exposed the defect.
#[test]
fn declaration_generator_uses_audio_snapshot_beside_live_audio_runtime() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (_tokio, _audio_runtime) = audio_runtime(temp.path())?;

    let generated = std::thread::spawn(|| {
        let sources = BTreeMap::from([(
            "index.ts".to_string(),
            "export const answer: number = 42;".to_string(),
        )]);
        smudgy_script::dts::generate_declarations(&sources, &BTreeMap::new())
    })
    .join()
    .expect("declaration generator thread panicked")?;

    assert_eq!(
        generated.files.get("index.d.ts").map(String::as_str),
        Some("export declare const answer: number;\n")
    );
    Ok(())
}
