use std::rc::Rc;

use anyhow::Result;
use deno_core::{ModuleSpecifier, PollEventLoopOptions, serde_v8};
use smudgy_script::{ImportPolicy, ModulePolicy, ScriptRuntime, ScriptRuntimeOptions, WorkerMode};

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

#[ignore = "requires network"]
#[test]
fn luajs_esm_import_resolves_bare_node_builtin_and_loads_wasm() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let tokio = Rc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
    );
    let mut runtime = ScriptRuntime::new(ScriptRuntimeOptions {
        extensions: Vec::new(),
        data_dir: temp.path().to_path_buf(),
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
    let module_path = temp.path().join("luajs_test.mjs");
    std::fs::write(
        &module_path,
        r#"
        import { LuaJS } from "npm:@doridian/luajs@1.0.8";
        const state = await LuaJS.newState();
        const [value] = await state.run("return 42 + 69");
        state.close();
        export const ok = value === 111;
        "#,
    )?;
    let specifier = ModuleSpecifier::from_file_path(module_path).unwrap();

    let _entered = EnteredIsolate::enter(&mut runtime);
    let ok = tokio.block_on(async {
        let module_id = runtime
            .deno_runtime()
            .load_main_es_module(&specifier)
            .await?;
        let receiver = runtime.deno_runtime().mod_evaluate(module_id);
        runtime
            .deno_runtime()
            .run_event_loop(PollEventLoopOptions::default())
            .await?;
        receiver.await?;

        let namespace = runtime.deno_runtime().get_module_namespace(module_id)?;
        deno_core::scope!(scope, runtime.deno_runtime());
        let namespace = namespace.open(scope);
        let key = deno_core::v8::String::new(scope, "ok").unwrap();
        let value = namespace.get(scope, key.into()).unwrap();
        Ok::<bool, anyhow::Error>(serde_v8::from_v8(scope, value)?)
    })?;
    assert!(ok);
    Ok(())
}
