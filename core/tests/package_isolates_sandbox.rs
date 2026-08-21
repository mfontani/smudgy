//! A session spawns a **real** sandboxed isolate per installed-untrusted
//! package, and firing routes into the owning isolate (`script/PACKAGE-ISOLATES-SANDBOX.md`).
//! Unlike `command_ordering.rs` (which proves the ordering invariant with a *synthetic*
//! second isolate key and plaintext aliases), these drive **JS-function aliases**, so a match
//! actually calls `call_javascript_function` into the sandboxed isolate's own heap + registry.
//!
//! Packages are injected via an in-memory [`PackageProvider`] (the `spawn_with_package_provider`
//! seam) so a real second isolate can be exercised without the cloud backend. The lockfile marks
//! each as untrusted (the default), so the engine gives it its own isolate.
//!
//! Covers boundary/coexistence, cross-isolate depth-first ordering, and function isolation across
//! isolates, plus singleton dedupe across isolates and versions.

use std::pin::Pin;
use std::rc::Rc;
#[cfg(feature = "web-audio")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(feature = "web-audio")]
use std::thread;
use std::time::Duration;

use futures::{Stream, StreamExt};
use smudgy_core::models::shared_packages::{self, UpdateMode};
use smudgy_core::session::runtime::{RuntimeAction, RuntimeThreadJoinOutcome, join_runtime_thread};
#[cfg(feature = "web-audio")]
use smudgy_core::session::spawn_with_package_provider_and_audio;
use smudgy_core::session::{
    BufferUpdate, PackageProviderFactory, SessionEvent, SessionId, SessionParams,
    TaggedSessionEvent, spawn_with_package_provider,
};
use smudgy_script::{
    InMemoryPackageProvider, PackageKey, PackageManifest, PackageModuleSource, PackagePermissions,
    PackageProvider, ResolvedPackage, SmudgyCapabilities,
};

/// Time the collector waits for the next buffer event before declaring the session idle.
const QUIET_PERIOD: Duration = Duration::from_millis(900);

// V8 snapshot deserialization is not safe when this Windows test binary starts several session
// runtimes concurrently. Every runtime helper holds this through stream teardown and the runtime
// thread join, so no isolate destruction can overlap the next snapshot deserialize.
static PACKAGE_ISOLATE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(feature = "web-audio")]
fn retained_online_context_module(label: &str, count: usize) -> String {
    r#"
        import { echo } from "smudgy:core";

        if (Object.prototype.hasOwnProperty.call(globalThis, "__full_reload_heap_brand")) {
          throw new Error("an old engine heap survived full-session reload");
        }
        globalThis.__full_reload_heap_brand = Symbol("__LABEL__-heap");

        const held = [];
        for (let index = 0; index < __COUNT__; index += 1) {
          const context = new AudioContext({ sampleRate: 48_000, sinkId: "" });
          const gain = context.createGain();
          const oscillator = context.createOscillator();
          if (!(context instanceof AudioContext)
              || Object.getPrototypeOf(context) !== AudioContext.prototype
              || !(gain instanceof GainNode)
              || Object.getPrototypeOf(gain) !== GainNode.prototype
              || !(oscillator instanceof OscillatorNode)
              || Object.getPrototypeOf(oscillator) !== OscillatorNode.prototype) {
            throw new Error("replacement Web Audio objects have stale or foreign brands");
          }
          gain.gain.value = 0;
          oscillator.connect(gain);
          gain.connect(context.destination);
          oscillator.start();
          held.push({ context, gain, oscillator, graphBrand: Object.freeze({ index }) });
        }
        globalThis.__full_reload_retained_online_contexts = held;

        const cycleKey = "smudgy-full-reload-__LABEL__-cycle";
        const previous = Number(localStorage.getItem(cycleKey) ?? "0");
        const cycle = previous + 1;
        localStorage.setItem(cycleKey, String(cycle));
        echo("FULL_RELOAD___LABEL___READY:" + cycle);
    "#
    .replace("__LABEL__", label)
    .replace("__COUNT__", &count.to_string())
}

#[cfg(feature = "web-audio")]
fn prove_full_script_bus_reuse(bus: &smudgy_audio::MixerScriptBusHandle) {
    let reservations = (0..smudgy_audio::INPUTS_PER_BUS)
        .map(|_| {
            bus.try_reserve_input()
                .expect("every Script slot is reusable at the engine boundary")
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        bus.try_reserve_input(),
        Err(smudgy_audio::MixerControlError::InputCapacity)
    ));
    for reservation in reservations {
        let retirement = futures::executor::block_on(reservation.abort())
            .expect("boundary Script reservation retires exactly");
        assert!(retirement.is_clean());
    }
}

#[cfg(feature = "web-audio")]
async fn wait_for_quiescent_audio_usage(
    application: &smudgy_audio_web::ApplicationAudioOwner,
    online_contexts: usize,
    description: &str,
) -> deno_audio::AudioHostUsage {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut previous = None;
    let mut stable_samples = 0;
    loop {
        let usage = application.usage();
        let quiescent = usage.online_contexts() == online_contexts
            && usage.queued_control_commands() == 0
            && usage.queued_events() == 0;
        if quiescent && previous == Some(usage) {
            stable_samples += 1;
            if stable_samples >= 3 {
                return usage;
            }
        } else {
            previous = Some(usage);
            stable_samples = 0;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {description}; last usage: {usage:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(feature = "web-audio")]
async fn wait_for_exact_audio_usage(
    application: &smudgy_audio_web::ApplicationAudioOwner,
    expected: deno_audio::AudioHostUsage,
    description: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let usage = application.usage();
        if usage == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {description}; expected {expected:?}, got {usage:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One `smudgy://owner/name` test package: its version and module sources (entry is `index.js`).
struct TestPackage {
    owner: &'static str,
    name: &'static str,
    version: &'static str,
    modules: Vec<(&'static str, String)>,
    /// Whether the lock entry is installed enabled. A disabled install is still *resolvable* (it
    /// stays in the in-memory provider) but the engine must skip it when building the isolate set.
    enabled: bool,
}

impl TestPackage {
    fn new(owner: &'static str, name: &'static str, version: &'static str, entry: &str) -> Self {
        Self {
            owner,
            name,
            version,
            modules: vec![("index.js", entry.to_string())],
            enabled: true,
        }
    }

    /// Mark this package installed-but-disabled (the user's "install, don't enable" choice).
    fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// Spin up a headless session whose `smudgy://` packages resolve from an in-memory provider,
/// each installed untrusted (so the engine spawns it a sandboxed isolate). Wait until the `gate`
/// sentinel line has been observed `gate_count` times — module/package automations register
/// through the FIFO action queue, so a sentinel echoed *after* a `createAlias` proves that alias
/// is live — then send `input` and collect every appended line until the session goes quiet.
#[allow(clippy::too_many_lines)]
async fn run_scenario(
    session_id: u32,
    server: &str,
    local_modules: &[(&str, &str)],
    packages: Vec<TestPackage>,
    gate: &str,
    gate_count: usize,
    input: &str,
) -> Vec<String> {
    run_scenario_inner(
        session_id,
        server,
        local_modules,
        packages,
        gate,
        gate_count,
        input,
        None,
    )
    .await
}

#[cfg(feature = "web-audio")]
#[allow(clippy::too_many_arguments)]
async fn run_scenario_with_audio(
    session_id: u32,
    server: &str,
    local_modules: &[(&str, &str)],
    packages: Vec<TestPackage>,
    gate: &str,
    gate_count: usize,
    input: &str,
    audio_scope: smudgy_audio_web::SessionAudioScope,
) -> Vec<String> {
    run_scenario_inner(
        session_id,
        server,
        local_modules,
        packages,
        gate,
        gate_count,
        input,
        Some(audio_scope),
    )
    .await
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_scenario_inner(
    session_id: u32,
    server: &str,
    local_modules: &[(&str, &str)],
    packages: Vec<TestPackage>,
    gate: &str,
    gate_count: usize,
    input: &str,
    #[cfg(feature = "web-audio")] audio_scope: Option<smudgy_audio_web::SessionAudioScope>,
    #[cfg(not(feature = "web-audio"))] _audio_scope: Option<()>,
) -> Vec<String> {
    let _test_guard = PACKAGE_ISOLATE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let session_id = SessionId::from(session_id);

    // The smudgy home override is a process-global `OnceLock` (first setter in the binary wins),
    // so re-read it after setting and scope everything under a unique server name per test.
    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home = smudgy_core::get_smudgy_home().expect("smudgy home");
    let server_dir = home.join(server);
    let modules_dir = server_dir.join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(server_dir.join("logs")).unwrap();
    for (name, source) in local_modules {
        std::fs::write(modules_dir.join(name), source).unwrap();
    }
    // Install each package untrusted (the default) → its own sandboxed isolate, honoring its
    // `enabled` flag so a disabled package is installed-but-skipped by the engine.
    for pkg in &packages {
        let spec = format!("smudgy://{}/{}", pkg.owner, pkg.name);
        shared_packages::install_package(server, &spec, UpdateMode::Auto, pkg.enabled).unwrap();
        // The smudgy ops these isolate-boundary/ordering tests rely on (`createAlias` /
        // `createTriggers` / `echo` / `send`) are capability-gated. They don't exercise capability gating (that's
        // `package_isolates_enforcement.rs`), so grant the full smudgy capability set at install —
        // without a consent record a sandboxed package would be denied every smudgy op and these
        // tests couldn't run. Deno perms stay empty (none of these packages touch net/fs).
        shared_packages::record_consent(
            server,
            &spec,
            &PackagePermissions {
                smudgy: SmudgyCapabilities::all(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    // Inject the in-memory resolver in place of a cloud client. Rebuilt per engine construction
    // (incl. reload); each isolate's own loader compiles the same source into its own heap.
    let factory: PackageProviderFactory = Arc::new(move || {
        let mut provider = InMemoryPackageProvider::new();
        for pkg in &packages {
            provider.insert(ResolvedPackage {
                key: PackageKey {
                    owner: pkg.owner.to_string(),
                    name: pkg.name.to_string(),
                },
                resolved_version: pkg.version.to_string(),
                manifest: PackageManifest::parse(&format!(
                    "{{ \"name\": \"{}\", \"version\": \"{}\" }}",
                    pkg.name, pkg.version
                ))
                .expect("valid manifest"),
                integrity: format!("test-{}-{}", pkg.name, pkg.version),
                modules: pkg
                    .modules
                    .iter()
                    .map(|(subpath, text)| PackageModuleSource {
                        subpath: (*subpath).to_string(),
                        text: text.clone(),
                    })
                    .collect(),
            });
        }
        let provider: Rc<dyn PackageProvider> = Rc::new(provider);
        provider
    });

    let params = Arc::new(SessionParams {
        session_id,
        server_name: Arc::new(server.to_string()),
        profile_name: Arc::new("test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });

    let mut events: Pin<Box<dyn Stream<Item = TaggedSessionEvent>>> = {
        #[cfg(feature = "web-audio")]
        if let Some(audio_scope) = audio_scope {
            Box::pin(
                spawn_with_package_provider_and_audio(params, factory, audio_scope)
                    .expect("audio scope matches package session"),
            )
        } else {
            Box::pin(spawn_with_package_provider(params, factory))
        }
        #[cfg(not(feature = "web-audio"))]
        {
            Box::pin(spawn_with_package_provider(params, factory))
        }
    };
    // Collect from the very first event: engine notices (e.g. "[package] X failed to load") are
    // emitted during construction, before `RuntimeReady`, so they'd otherwise be consumed by the
    // wait loop and lost.
    let mut lines: Vec<String> = Vec::new();
    let tx = loop {
        let event = tokio::time::timeout(Duration::from_mins(1), events.next())
            .await
            .expect("timed out waiting for RuntimeReady")
            .expect("event stream ended before RuntimeReady");
        match event.event {
            SessionEvent::RuntimeReady(tx) => break tx,
            SessionEvent::UpdateBuffer(updates) => {
                for update in updates.iter() {
                    if let BufferUpdate::Append(line) = update {
                        lines.push(line.text.clone());
                    }
                }
            }
            _ => {}
        }
    };

    // Pin the command separator so the test is environment-independent.
    tx.send(RuntimeAction::ApplySettings {
        command_separator: Arc::new(";".to_string()),
        raw_line_prefix: Arc::new("\\".to_string()),
        log_enabled: true,
        script_settings: Box::new(smudgy_core::models::settings::ScriptSettings::default()),
    })
    .unwrap();

    let mut seen_gate = 0usize;
    let mut sent = false;
    while let Ok(Some(event)) = tokio::time::timeout(QUIET_PERIOD, events.next()).await {
        if let SessionEvent::UpdateBuffer(updates) = event.event {
            for update in updates.iter() {
                if let BufferUpdate::Append(line) = update {
                    lines.push(line.text.clone());
                    if !sent && line.text == gate {
                        seen_gate += 1;
                        if seen_gate >= gate_count {
                            tx.send(RuntimeAction::Send(Arc::new(input.to_string())))
                                .unwrap();
                            sent = true;
                        }
                    }
                }
            }
        }
    }
    tx.send(RuntimeAction::Shutdown)
        .expect("runtime accepts shutdown");
    drop(tx);
    drop(events);
    let joined = tokio::task::spawn_blocking(move || join_runtime_thread(session_id))
        .await
        .expect("runtime join task does not panic");
    assert_eq!(joined, RuntimeThreadJoinOutcome::Clean { session_id });

    assert!(
        sent,
        "gate sentinel {gate:?} (x{gate_count}) was never observed; transcript:\n{lines:#?}"
    );
    lines
}

/// Boundary/coexistence with a real second isolate: a local module (main isolate) and a
/// sandboxed package each register a same-named JS alias; one input matches both and **both
/// fire**, because the trigger Manager keys by `(IsolateId, Origin, name)`. Each handler runs in
/// its own isolate's heap (`call_javascript_function` routes by id). To make the *isolate boundary*
/// load-bearing (the two also have distinct `Origin`s, so coexistence alone wouldn't require a
/// second isolate), the package reports `from_pkg` only if it CANNOT see a `globalThis` marker the
/// main module set in main's heap — so a no-op sandbox (one shared heap) would report `from_pkg_LEAK`
/// and fail this test.
#[tokio::test]
async fn coexists_across_main_and_sandboxed_isolate() {
    // Marker lives in main's heap only; the package seeing it would mean a shared isolate.
    let main_mod = r#"
        import { createAlias, echo } from "smudgy:core";
        globalThis.__leak_marker = "MAIN";
        createAlias("^dup$", () => { echo("from_main"); });
    "#;
    let pkg = TestPackage::new(
        "wbk",
        "inc",
        "1.0.0",
        r#"
        import { createAlias, echo } from "smudgy:core";
        createAlias("^dup$", () => {
            echo(globalThis.__leak_marker ? "from_pkg_LEAK" : "from_pkg");
        });
        echo("PKG_READY");
        "#,
    );

    let lines = run_scenario(
        9201,
        "pi_sandbox_boundary",
        &[("main_mod.ts", main_mod)],
        vec![pkg],
        "PKG_READY",
        1,
        "dup",
    )
    .await;

    assert!(
        lines.iter().any(|l| l == "from_main"),
        "main-isolate alias must fire; transcript:\n{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l == "from_pkg"),
        "sandboxed-package alias must fire AND be isolated (no `from_pkg_LEAK`) — coexistence across a real isolate boundary; transcript:\n{lines:#?}"
    );
}

/// Accessibility packages run in sandboxed isolates, so Web Audio is installed there as a
/// first-class web API rather than being limited to the trusted main isolate. Both contexts use
/// their scoped default Script route into one fake physical mixer, without CI audio hardware.
#[cfg(feature = "web-audio")]
#[tokio::test]
async fn sandboxed_package_can_render_web_audio() {
    let pkg = TestPackage::new(
        "a11y",
        "earcon",
        "1.0.0",
        r#"
        import { echo } from "smudgy:core";

        const context = new AudioContext({ sampleRate: 48_000, sinkId: "" });
        if (globalThis.__main_audio_context !== undefined) {
          echo("SANDBOX_AUDIO_CONTEXT_LEAK");
        }
        const oscillator = context.createOscillator();
        const gain = context.createGain();
        gain.gain.value = 0.05;
        oscillator.connect(gain);
        gain.connect(context.destination);
        oscillator.onended = async () => {
            await context.close();
            echo("SANDBOX_WEB_AUDIO_OK");
        };
        oscillator.start();
        oscillator.stop(context.currentTime + 0.02);
        "#,
    );

    let session_id = 9_211;
    let (service, probe) = smudgy_audio::test_support::start_test_mixer(
        48_000,
        smudgy_audio::test_support::TestDriverConfig::default(),
    )
    .expect("headless process mixer starts");
    let mixer_owner = service
        .add_session(smudgy_audio::AudioSessionId(u64::from(session_id)))
        .expect("package session joins process mixer");
    let application = smudgy_audio_web::ApplicationAudioOwner::new(
        deno_audio::AudioHostLimits::unlimited()
            .max_online_contexts(Some(4))
            .max_live_audio_bytes(Some(32 * 1024 * 1024))
            .max_graph_nodes(Some(512))
            .max_graph_connections(Some(512))
            .max_scheduled_sources(Some(256))
            .max_automation_events(Some(1_024))
            .max_queued_control_commands(Some(256))
            .max_queued_events(Some(256))
            .max_decode_jobs(Some(2))
            .max_offline_render_jobs(Some(2)),
    );
    let registration = application
        .registrar()
        .register_session(mixer_owner)
        .expect("package session audio registration succeeds");
    let render_done = Arc::new(AtomicBool::new(false));
    let render_thread = {
        let render_done = Arc::clone(&render_done);
        let probe = probe.clone();
        thread::spawn(move || {
            let mut output = [0.0; 256];
            while !render_done.load(Ordering::Acquire) {
                let _ = probe.render(&mut output, 2);
                thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let lines = run_scenario_with_audio(
        session_id,
        "pi_sandbox_web_audio",
        &[(
            "trusted-web-audio.ts",
            r#"
            import { echo } from "smudgy:core";
            const context = new AudioContext({ sampleRate: 48_000, sinkId: "" });
            globalThis.__main_audio_context = context;
            const oscillator = context.createOscillator();
            oscillator.onended = async () => {
              await context.close();
              echo("MAIN_WEB_AUDIO_OK");
            };
            oscillator.connect(context.destination);
            oscillator.start();
            oscillator.stop(context.currentTime + 0.02);
            "#,
        )],
        vec![pkg],
        "SANDBOX_WEB_AUDIO_OK",
        1,
        "noop",
        registration.scope(),
    )
    .await;
    render_done.store(true, Ordering::Release);
    render_thread.join().expect("fake physical renderer joins");

    assert!(
        lines.iter().any(|line| line == "SANDBOX_WEB_AUDIO_OK"),
        "sandboxed accessibility package must render and close Web Audio; transcript:\n{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line == "MAIN_WEB_AUDIO_OK"),
        "trusted and sandboxed isolates must receive distinct contexts from the same scope; transcript:\n{lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| line == "SANDBOX_AUDIO_CONTEXT_LEAK"),
        "a sandboxed context must not share its graph brand or heap with main; transcript:\n{lines:#?}"
    );
    assert_eq!(probe.start_count(), 1);
    assert_eq!(probe.play_count(), 1);
    drop(registration);
    assert!(service.shutdown().clean);
}

/// A live reload replaces the complete script engine: trusted main and every sandboxed package
/// isolate. This stress keeps every old online context intentionally open and makes the runtime's
/// generation retirement, rather than script-authored `close()`, return the exact application
/// permits and Script slots before the replacement can construct anything.
#[cfg(feature = "web-audio")]
#[tokio::test]
#[allow(clippy::await_holding_lock, clippy::too_many_lines)]
async fn repeated_full_session_reload_returns_exact_audio_baseline() {
    let _test_guard = PACKAGE_ISOLATE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home = smudgy_core::get_smudgy_home().expect("smudgy home");
    let reload_server = "pi_full_session_audio_reload";
    let sibling_server = "pi_full_reload_sibling";
    for server in [reload_server, sibling_server] {
        std::fs::create_dir_all(home.join(server).join("modules"))
            .expect("create module directory");
        std::fs::create_dir_all(home.join(server).join("logs")).expect("create log directory");
    }

    std::fs::write(
        home.join(sibling_server).join("modules/sibling.ts"),
        r#"
        import { createAlias, echo } from "smudgy:core";
        const context = new AudioContext({ sampleRate: 48_000, sinkId: "" });
        const gain = context.createGain();
        const oscillator = context.createOscillator();
        gain.gain.value = 0.025;
        oscillator.connect(gain);
        gain.connect(context.destination);
        oscillator.start();
        globalThis.__full_reload_sibling_audio = { context, gain, oscillator };
        createAlias("^full_reload_ping$", () => echo("FULL_RELOAD_SIBLING_PONG"));
        echo("FULL_RELOAD_SIBLING_READY");
        "#,
    )
    .expect("write sibling module");
    std::fs::write(
        home.join(reload_server).join("modules/main.ts"),
        retained_online_context_module("MAIN", 5),
    )
    .expect("write reloader main module");

    let packages = vec![
        TestPackage::new(
            "reload",
            "alpha",
            "1.0.0",
            &retained_online_context_module("ALPHA", 5),
        ),
        TestPackage::new(
            "reload",
            "bravo",
            "1.0.0",
            &retained_online_context_module("BRAVO", 5),
        ),
        TestPackage::new(
            "reload",
            "charlie",
            "1.0.0",
            &retained_online_context_module("CHARLIE", 5),
        ),
        TestPackage::new(
            "reload",
            "delta",
            "1.0.0",
            &retained_online_context_module("DELTA", 5),
        ),
        TestPackage::new(
            "reload",
            "echo",
            "1.0.0",
            &retained_online_context_module("ECHO", 5),
        ),
        TestPackage::new(
            "reload",
            "zulu",
            "1.0.0",
            &retained_online_context_module("ZULU", 2),
        ),
    ];
    for package in &packages {
        let specifier = format!("smudgy://{}/{}", package.owner, package.name);
        shared_packages::install_package(reload_server, &specifier, UpdateMode::Auto, true)
            .expect("install reload package");
        shared_packages::record_consent(
            reload_server,
            &specifier,
            &PackagePermissions {
                smudgy: SmudgyCapabilities::all(),
                ..Default::default()
            },
        )
        .expect("grant package echo capability");
    }
    let package_factory: PackageProviderFactory = Arc::new(move || {
        let mut provider = InMemoryPackageProvider::new();
        for package in &packages {
            provider.insert(ResolvedPackage {
                key: PackageKey {
                    owner: package.owner.to_string(),
                    name: package.name.to_string(),
                },
                resolved_version: package.version.to_string(),
                manifest: PackageManifest::parse(&format!(
                    "{{ \"name\": \"{}\", \"version\": \"{}\" }}",
                    package.name, package.version
                ))
                .expect("valid reload package manifest"),
                integrity: format!("reload-{}-{}", package.name, package.version),
                modules: package
                    .modules
                    .iter()
                    .map(|(subpath, text)| PackageModuleSource {
                        subpath: (*subpath).to_string(),
                        text: text.clone(),
                    })
                    .collect(),
            });
        }
        Rc::new(provider) as Rc<dyn PackageProvider>
    });

    let (service, probe) = smudgy_audio::test_support::start_test_mixer(
        48_000,
        smudgy_audio::test_support::TestDriverConfig::default(),
    )
    .expect("headless process mixer starts once");
    let render_done = Arc::new(AtomicBool::new(false));
    let rendered_quanta = Arc::new(AtomicUsize::new(0));
    let non_silent_quanta = Arc::new(AtomicUsize::new(0));
    let render_thread = {
        let render_done = Arc::clone(&render_done);
        let rendered_quanta = Arc::clone(&rendered_quanta);
        let non_silent_quanta = Arc::clone(&non_silent_quanta);
        let probe = probe.clone();
        thread::spawn(move || {
            let mut output = [0.0; 256];
            while !render_done.load(Ordering::Acquire) {
                if probe.render(&mut output, 2).is_ok() {
                    rendered_quanta.fetch_add(1, Ordering::AcqRel);
                    if output.iter().any(|sample| sample.abs() > f32::EPSILON) {
                        non_silent_quanta.fetch_add(1, Ordering::AcqRel);
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        })
    };

    let mut application = Arc::new(smudgy_audio_web::ApplicationAudioOwner::new(
        deno_audio::AudioHostLimits::unlimited()
            .max_online_contexts(Some(33))
            .max_live_audio_bytes(Some(32 * 1024 * 1024))
            .max_graph_nodes(Some(1_024))
            .max_graph_connections(Some(1_024))
            .max_scheduled_sources(Some(512))
            .max_automation_events(Some(1_024))
            .max_queued_control_commands(Some(2_048))
            .max_queued_events(Some(2_048))
            .max_decode_jobs(Some(2))
            .max_offline_render_jobs(Some(2)),
    ));
    let empty_baseline = application.usage();
    assert_eq!(empty_baseline, deno_audio::AudioHostUsage::default());

    let sibling_numeric_id = 9_220;
    let sibling_id = SessionId::from(sibling_numeric_id);
    let sibling_owner = service
        .add_session(smudgy_audio::AudioSessionId(u64::from(sibling_numeric_id)))
        .expect("sibling joins process mixer");
    let sibling_registration = application
        .registrar()
        .register_session(sibling_owner)
        .expect("sibling audio registration succeeds");
    let sibling_params = Arc::new(SessionParams {
        session_id: sibling_id,
        server_name: Arc::new(sibling_server.to_string()),
        profile_name: Arc::new("test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });
    let mut sibling_events = Box::pin(
        smudgy_core::session::spawn_with_audio(sibling_params, sibling_registration.scope())
            .expect("sibling audio scope matches"),
    );
    let mut sibling_tx = None;
    tokio::time::timeout(Duration::from_mins(1), async {
        let mut ready = false;
        while sibling_tx.is_none() || !ready {
            let event = sibling_events.next().await.expect("sibling remains live");
            match event.event {
                SessionEvent::RuntimeReady(tx) => sibling_tx = Some(tx),
                SessionEvent::UpdateBuffer(updates) => {
                    ready |= updates.iter().any(|update| {
                        matches!(update, BufferUpdate::Append(line) if line.text == "FULL_RELOAD_SIBLING_READY")
                    });
                }
                _ => {}
            }
        }
    })
    .await
    .expect("sibling starts");
    let sibling_baseline =
        wait_for_quiescent_audio_usage(&application, 1, "sibling-only host baseline").await;
    assert_eq!(sibling_baseline.graph_nodes(), 5);
    assert_eq!(sibling_baseline.graph_connections(), 2);
    assert_eq!(sibling_baseline.scheduled_sources(), 1);

    let reload_numeric_id = 9_221;
    let reload_id = SessionId::from(reload_numeric_id);
    let reload_owner = service
        .add_session(smudgy_audio::AudioSessionId(u64::from(reload_numeric_id)))
        .expect("reloader joins the same process mixer");
    let reload_script_bus = reload_owner.script_bus();
    let reload_native_bus = reload_owner.native_bus();
    let reload_speech_bus = reload_owner.speech_bus();
    let reload_registration = application
        .registrar()
        .register_session(reload_owner)
        .expect("reloader audio registration succeeds");
    let reload_scope = reload_registration.scope();
    let scope_debug = format!("{reload_scope:?}");
    let rebuild_count = Arc::new(AtomicUsize::new(0));
    let on_engine_rebuild: Arc<dyn Fn() + Send + Sync> = {
        let application = Arc::clone(&application);
        let reload_script_bus = reload_script_bus.clone();
        let rebuild_count = Arc::clone(&rebuild_count);
        Arc::new(move || {
            assert_eq!(
                application.usage(),
                sibling_baseline,
                "old main/package contexts must drain before replacement construction"
            );
            prove_full_script_bus_reuse(&reload_script_bus);
            assert_eq!(application.usage(), sibling_baseline);
            rebuild_count.fetch_add(1, Ordering::AcqRel);
        })
    };
    let reload_params = Arc::new(SessionParams {
        session_id: reload_id,
        server_name: Arc::new(reload_server.to_string()),
        profile_name: Arc::new("test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: Some(Arc::clone(&on_engine_rebuild)),
    });
    let mut reload_events = Box::pin(
        spawn_with_package_provider_and_audio(reload_params, package_factory, reload_scope.clone())
            .expect("reloader audio scope matches"),
    );
    let mut reload_tx = None;
    let labels = ["MAIN", "ALPHA", "BRAVO", "CHARLIE", "DELTA", "ECHO", "ZULU"];
    let mut transcript = Vec::new();
    let mut loaded_baseline = None;
    let mut cross_reload_render_threshold = None;

    for cycle in 1..=4 {
        let expected = labels
            .iter()
            .map(|label| format!("FULL_RELOAD_{label}_READY:{cycle}"))
            .collect::<std::collections::BTreeSet<_>>();
        let mut observed = std::collections::BTreeSet::new();
        tokio::time::timeout(Duration::from_secs(90), async {
            while observed != expected {
                let event = reload_events.next().await.expect("reloader remains live");
                match event.event {
                    SessionEvent::RuntimeReady(tx) => reload_tx = Some(tx),
                    SessionEvent::UpdateBuffer(updates) => {
                        for update in updates.iter() {
                            if let BufferUpdate::Append(line) = update {
                                transcript.push(line.text.clone());
                                if expected.contains(&line.text) {
                                    observed.insert(line.text.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "generation {cycle} did not reconstruct every isolate; observed {observed:?}; transcript: {transcript:#?}"
            )
        });
        if let Some((rendered_before_reload, non_silent_before_reload)) =
            cross_reload_render_threshold.take()
        {
            assert!(
                rendered_quanta.load(Ordering::Acquire) > rendered_before_reload
                    && non_silent_quanta.load(Ordering::Acquire) > non_silent_before_reload,
                "sibling physical output did not advance across reload cycle {cycle}"
            );
        }
        assert_eq!(rebuild_count.load(Ordering::Acquire), cycle);
        assert_eq!(format!("{reload_scope:?}"), scope_debug);
        assert_eq!(reload_scope.session_id(), u64::from(reload_numeric_id));

        let loaded = wait_for_quiescent_audio_usage(
            &application,
            33,
            &format!("loaded Web Audio generation {cycle}"),
        )
        .await;
        assert!(matches!(
            reload_script_bus.try_reserve_input(),
            Err(smudgy_audio::MixerControlError::InputCapacity)
        ));
        if let Some(expected_loaded) = loaded_baseline {
            assert_eq!(loaded, expected_loaded);
        } else {
            assert_eq!(
                loaded.online_contexts() - sibling_baseline.online_contexts(),
                32
            );
            assert_eq!(loaded.graph_nodes() - sibling_baseline.graph_nodes(), 160);
            assert_eq!(
                loaded.graph_connections() - sibling_baseline.graph_connections(),
                64
            );
            assert_eq!(
                loaded.scheduled_sources() - sibling_baseline.scheduled_sources(),
                32
            );
            assert_eq!(
                loaded.live_audio_bytes(),
                sibling_baseline.live_audio_bytes()
            );
            assert_eq!(
                loaded.automation_events(),
                sibling_baseline.automation_events()
            );
            assert_eq!(
                loaded.queued_control_commands(),
                sibling_baseline.queued_control_commands()
            );
            assert_eq!(loaded.queued_events(), sibling_baseline.queued_events());
            assert_eq!(loaded.decode_jobs(), sibling_baseline.decode_jobs());
            assert_eq!(
                loaded.offline_render_jobs(),
                sibling_baseline.offline_render_jobs()
            );
            loaded_baseline = Some(loaded);
        }

        let native_gains = [0.55, 0.60, 0.65, 0.70];
        let speech_gains = [0.70, 0.65, 0.60, 0.55];
        reload_native_bus
            .set_gain(native_gains[cycle - 1])
            .expect("the exact Native bus survives reload");
        reload_speech_bus
            .set_gain(speech_gains[cycle - 1])
            .expect("the exact Speech bus survives reload");
        let rendered_before = rendered_quanta.load(Ordering::Acquire);
        let non_silent_before = non_silent_quanta.load(Ordering::Acquire);
        let render_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while rendered_quanta.load(Ordering::Acquire) == rendered_before
            || non_silent_quanta.load(Ordering::Acquire) == non_silent_before
        {
            assert!(
                tokio::time::Instant::now() < render_deadline,
                "sibling rendering stopped during reload cycle {cycle}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        sibling_tx
            .as_ref()
            .expect("sibling published its runtime")
            .send(RuntimeAction::Send(Arc::new(
                "full_reload_ping".to_string(),
            )))
            .expect("sibling accepts a per-cycle action");
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = sibling_events.next().await.expect("sibling remains responsive");
                if let SessionEvent::UpdateBuffer(updates) = event.event
                    && updates.iter().any(|update| {
                        matches!(update, BufferUpdate::Append(line) if line.text == "FULL_RELOAD_SIBLING_PONG")
                    })
                {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("sibling stopped responding during reload cycle {cycle}"));
        assert_eq!(application.usage(), loaded);
        assert_eq!(probe.start_count(), 1);
        assert_eq!(probe.play_count(), 1);

        if cycle < 4 {
            cross_reload_render_threshold = Some((
                rendered_quanta.load(Ordering::Acquire),
                non_silent_quanta.load(Ordering::Acquire),
            ));
            reload_tx
                .as_ref()
                .expect("reloader published its runtime")
                .send(RuntimeAction::Reload)
                .expect("reloader accepts full-session reload");
        }
    }

    reload_tx
        .take()
        .expect("reloader published its runtime")
        .send(RuntimeAction::Shutdown)
        .expect("reloader accepts shutdown");
    drop(reload_events);
    let reload_join = tokio::task::spawn_blocking(move || join_runtime_thread(reload_id))
        .await
        .expect("reloader join task does not panic");
    assert_eq!(
        reload_join,
        RuntimeThreadJoinOutcome::Clean {
            session_id: reload_id
        }
    );
    wait_for_exact_audio_usage(
        &application,
        sibling_baseline,
        "final reloader generation retirement",
    )
    .await;
    prove_full_script_bus_reuse(&reload_script_bus);
    reload_registration
        .retire()
        .await
        .expect("reloader mixer session retires exactly");

    sibling_tx
        .take()
        .expect("sibling published its runtime")
        .send(RuntimeAction::Shutdown)
        .expect("sibling accepts shutdown");
    drop(sibling_events);
    let sibling_join = tokio::task::spawn_blocking(move || join_runtime_thread(sibling_id))
        .await
        .expect("sibling join task does not panic");
    assert_eq!(
        sibling_join,
        RuntimeThreadJoinOutcome::Clean {
            session_id: sibling_id
        }
    );
    wait_for_exact_audio_usage(
        &application,
        empty_baseline,
        "empty application host baseline",
    )
    .await;
    sibling_registration
        .retire()
        .await
        .expect("sibling mixer session retires exactly");

    drop(reload_scope);
    drop(reload_script_bus);
    drop(reload_native_bus);
    drop(reload_speech_bus);
    drop(on_engine_rebuild);
    assert!(
        Arc::get_mut(&mut application)
            .expect("all scoped application authorities are retired")
            .seal()
    );
    assert_eq!(application.usage(), empty_baseline);

    render_done.store(true, Ordering::Release);
    render_thread.join().expect("fake physical renderer joins");
    let shutdown = service.shutdown();
    assert!(shutdown.clean);
    assert_eq!(probe.start_count(), 1);
    assert_eq!(probe.play_count(), 1);
    assert_eq!(probe.close_count(), 1);
}

/// Cross-isolate depth-first ordering with a real second isolate: a main-isolate alias
/// `send`s a command that a sandboxed package's alias matches and expands. Depth-first order must
/// hold across the boundary (the package's expansion completes before the main alias's sibling
/// command), exactly as `command_ordering.rs` asserts for synthetic isolates — but here the package
/// handler genuinely executes in the second isolate via `call_javascript_function`. The package's
/// handler only emits `deep_a`/`deep_b` when it CANNOT see main's `globalThis` marker, so a no-op
/// sandbox (one shared heap) would emit `LEAK` instead and break the ordering assertion — making the
/// real-second-isolate requirement load-bearing rather than incidental.
#[tokio::test]
async fn depth_first_ordering_holds_across_real_isolate() {
    let main_mod = r#"
        import { createAlias, send } from "smudgy:core";
        globalThis.__leak_marker = "MAIN";
        createAlias("^outer$", () => { send("deep"); send("tail"); });
    "#;
    let pkg = TestPackage::new(
        "wbk",
        "inc",
        "1.0.0",
        r#"
        import { createAlias, send, echo } from "smudgy:core";
        createAlias("^deep$", () => {
            if (globalThis.__leak_marker) { send("LEAK"); }
            else { send("deep_a"); send("deep_b"); }
        });
        echo("PKG_READY");
        "#,
    );

    let lines = run_scenario(
        9202,
        "pi_sandbox_ordering",
        &[("main_mod.ts", main_mod)],
        vec![pkg],
        "PKG_READY",
        1,
        "outer",
    )
    .await;

    let order: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| matches!(*l, "deep_a" | "deep_b" | "tail"))
        .collect();
    assert_eq!(
        order,
        vec!["deep_a", "deep_b", "tail"],
        "the sandboxed package's expansion must finish before the main alias's sibling command; transcript:\n{lines:#?}"
    );
}

/// A **disabled** install is excluded from the rebuilt isolate set (`build_isolate_plan` skips
/// `!enabled` roots — `PACKAGE-ISOLATES-CONSENT-TRUST.md`): the package is
/// still resolvable (present in the provider), but the engine must neither evaluate its modules nor
/// register its automations. An *enabled* sibling supplies the gate; the disabled one shares the
/// alias name `ping`, so if it had loaded its `DEAD` handler would also fire. Asserting `DEAD_READY`
/// (its top-level echo) and `DEAD` (its alias) are absent proves the disabled isolate never built.
#[tokio::test]
async fn disabled_package_is_excluded_from_the_isolate_set() {
    let live = TestPackage::new(
        "wbk",
        "live",
        "1.0.0",
        r#"
        import { createAlias, echo } from "smudgy:core";
        createAlias("^ping$", () => { echo("PONG"); });
        echo("LIVE_READY");
        "#,
    );
    let dead = TestPackage::new(
        "wbk",
        "dead",
        "1.0.0",
        r#"
        import { createAlias, echo } from "smudgy:core";
        createAlias("^ping$", () => { echo("DEAD"); });
        echo("DEAD_READY");
        "#,
    )
    .disabled();

    let lines = run_scenario(
        9203,
        "pi_sandbox_disabled",
        &[],
        vec![live, dead],
        "LIVE_READY",
        1,
        "ping",
    )
    .await;

    assert!(
        lines.iter().any(|l| l == "PONG"),
        "the enabled package's alias must fire; transcript:\n{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l == "DEAD_READY"),
        "the disabled package's module must never evaluate; transcript:\n{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l == "DEAD"),
        "the disabled package's alias must never register or fire; transcript:\n{lines:#?}"
    );
}

/// Function isolation: the *same* package runs in two isolates (the sandboxed install,
/// and a copy a local module pulled into main by importing it), each with its own module-global.
/// Bumping the main copy's counter must not be visible to the sandboxed copy — proving the
/// coexistence is real heap isolation, not a shared instance.
#[tokio::test]
async fn module_global_is_isolated_between_copies() {
    // The local module imports the package (→ a copy of it runs in main), bumps that copy's
    // counter, and exposes the bump under its own alias name.
    let main_mod = r#"
        import { createAlias, echo } from "smudgy:core";
        import { bump } from "smudgy://wbk/inc";
        createAlias("^main_bump$", () => { bump(); echo("bumped"); });
    "#;
    // The package keeps a module-global counter and reports it under a coexisting alias name.
    let pkg = TestPackage::new(
        "wbk",
        "inc",
        "1.0.0",
        r#"
        import { createAlias, echo } from "smudgy:core";
        let n = 0;
        export function bump() { n += 1; return n; }
        createAlias("^inc_report$", () => { echo("inc=" + n); });
        echo("INC_READY");
        "#,
    );

    // `inc` loads into two isolates (main, via the import, and its own sandbox), so it echoes
    // INC_READY twice; wait for both so all three aliases are registered before driving.
    let lines = run_scenario(
        9203,
        "pi_sandbox_isolation",
        &[("main_mod.ts", main_mod)],
        vec![pkg],
        "INC_READY",
        2,
        "main_bump;inc_report",
    )
    .await;

    assert!(
        lines.iter().any(|l| l == "inc=1"),
        "the main copy's counter must read 1 after its bump; transcript:\n{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l == "inc=0"),
        "the sandboxed copy's counter must still read 0 (separate heap); transcript:\n{lines:#?}"
    );
}

/// Regression — a sandboxed package that throws at module-eval must be **skipped** (its isolate
/// dropped, a notice emitted) and the session must keep running. The isolate is left "exited"
/// between ops (Model B), so dropping it on the load-failure path without first making it the
/// thread's current isolate would trip `rusty_v8`'s `OwnedIsolate::Drop` assert and **abort the whole
/// process** — which here would crash the test binary rather than fail gracefully. So a clean run
/// (main's `ping`→`pong` still fires, plus the failure notice) is the proof the failure path is
/// safe. (`core/src/session/runtime/script_engine.rs` enters the isolate before dropping it.)
#[tokio::test]
async fn failing_sandboxed_package_is_skipped_without_aborting() {
    let main_mod = r#"
        import { createAlias, echo } from "smudgy:core";
        createAlias("^ping$", () => { echo("pong"); });
        echo("MAIN_READY");
    "#;
    // A static import of a package the provider doesn't have → graph load fails synchronously, so
    // `load_modules` returns Err during construction and the engine drops this isolate in place
    // (the path that aborts without the enter-before-drop fix). A *runtime* top-level throw instead
    // surfaces as an async rejection during pumping (a separate, non-aborting path).
    let broken = TestPackage::new(
        "wbk",
        "broken",
        "1.0.0",
        r#"import "smudgy://wbk/no_such_dependency";"#,
    );

    let lines = run_scenario(
        9204,
        "pi_sandbox_failload",
        &[("main_mod.ts", main_mod)],
        vec![broken],
        "MAIN_READY",
        1,
        "ping",
    )
    .await;

    assert!(
        lines.iter().any(|l| l == "pong"),
        "the session must survive a failing sandboxed package and keep firing main aliases; transcript:\n{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("[package] broken failed to load")),
        "the failing package must be reported (skipped, not silently ignored); transcript:\n{lines:#?}"
    );
}

/// `singleton` dedupe across isolates: the SAME package runs in two isolates
/// (its sandboxed install, and a copy a local module pulled into main by importing it) and each
/// copy calls `createAlias("^dup$", …, { singleton: true })`. The singleton identity (here the
/// derived name, i.e. the pattern source) drops the
/// isolate dimension and the version (`PACKAGE-ISOLATES.md`), so exactly ONE `dup` registers
/// session-wide: the first copy's op returns `created === true`, the second returns `false` and
/// no-ops. Firing `dup` then echoes once. Without the flag (second scenario) the two copies
/// coexist and both fire — the default behavior.
#[tokio::test]
async fn singleton_dedupes_same_package_across_isolates() {
    // --- With `{ singleton: true }`: exactly one copy registers; the other reports it existed. ---
    let pkg = TestPackage::new(
        "wbk",
        "widget",
        "1.0.0",
        r#"
        import { createAlias, echo } from "smudgy:core";
        const a = createAlias("^dup$", () => { echo("fired_dup"); }, { singleton: true });
        echo(a.created ? "created_true" : "created_false");
        echo("PKG_READY");
        "#,
    );
    let lines = run_scenario(
        9211,
        "pi_singleton_dedupe",
        // Importing the package pulls a second copy of it into the main isolate.
        &[("main_mod.ts", r#"import "smudgy://wbk/widget";"#)],
        vec![pkg],
        "PKG_READY",
        2,
        "dup",
    )
    .await;

    assert_eq!(
        lines.iter().filter(|l| *l == "created_true").count(),
        1,
        "exactly one copy must win the singleton reservation; transcript:\n{lines:#?}"
    );
    assert_eq!(
        lines.iter().filter(|l| *l == "created_false").count(),
        1,
        "the second copy's singleton create must report it already existed; transcript:\n{lines:#?}"
    );
    assert_eq!(
        lines.iter().filter(|l| *l == "fired_dup").count(),
        1,
        "only the one registered `dup` alias may fire; transcript:\n{lines:#?}"
    );

    // --- Without the flag: the two copies coexist and BOTH fire (the default). ---
    let pkg = TestPackage::new(
        "wbk",
        "gadget",
        "1.0.0",
        r#"
        import { createAlias, echo } from "smudgy:core";
        createAlias("^dup$", () => { echo("fired_dup"); });
        echo("PKG_READY");
        "#,
    );
    let lines = run_scenario(
        9212,
        "pi_singleton_coexist",
        &[("main_mod.ts", r#"import "smudgy://wbk/gadget";"#)],
        vec![pkg],
        "PKG_READY",
        2,
        "dup",
    )
    .await;

    assert_eq!(
        lines.iter().filter(|l| *l == "fired_dup").count(),
        2,
        "without `singleton` the two coexisting copies must both fire; transcript:\n{lines:#?}"
    );
}

/// Cross-isolate `off` is unforgeable: package A and package B each subscribe to the SAME event
/// (`user#evt`, emitted by a main-isolate module). Because each package's `on` is the FIRST function
/// it registers, both subscribers get the SAME numeric token (`FunctionId(0)`) in their own
/// per-isolate `script_functions`. B then `off()`s — passing token `0` — which must drop ONLY B's own
/// subscription (the `(isolate, function_id)` scoping in `op_smudgy_off`); A's identically-numbered
/// subscription lives in a different isolate, so it survives. If the guard matched on the raw token
/// alone, B's `off(0)` would also remove A's `0` and `A_FIRED` would vanish — so A still firing is the
/// proof the isolate dimension is load-bearing. (Also exercises main-isolate `emit` → package-isolate
/// `on` cross-boundary delivery, and `off` on the `op_smudgy_emit` path.)
#[tokio::test]
async fn off_token_is_scoped_to_its_isolate() {
    // Main isolate: an alias that emits `user#evt` (a module's event handle is stamped to
    // `user#…`).
    let main_mod = r#"
        import { createAlias, createEvent, echo } from "smudgy:core";
        const evt = createEvent("evt");
        createAlias("^fire$", () => { evt.emit({}); });
        echo("READY");
    "#;
    // Package A subscribes and stays subscribed. Its `.on` is its first registered function →
    // FunctionId 0.
    let pkg_a = TestPackage::new(
        "wbk",
        "alpha",
        "1.0.0",
        r#"
        import { events, echo } from "smudgy:core";
        events.lookup("user", "evt").on(() => { echo("A_FIRED"); });
        echo("READY");
        "#,
    );
    // Package B subscribes (also FunctionId 0 in its OWN isolate) then immediately unsubscribes
    // itself with that same token. Its `off(user#evt, 0)` must not touch A's `0` in another isolate.
    let pkg_b = TestPackage::new(
        "wbk",
        "beta",
        "1.0.0",
        r#"
        import { events, echo } from "smudgy:core";
        const sub = events.lookup("user", "evt").on(() => { echo("B_FIRED"); });
        sub.off();
        echo("READY");
        "#,
    );

    // Gate on all three modules signalling READY (main + A + B) so every subscription / B's
    // unsubscription and the `fire` alias are live before the event is emitted.
    let lines = run_scenario(
        9221,
        "pi_off_forgery",
        &[("main_mod.ts", main_mod)],
        vec![pkg_a, pkg_b],
        "READY",
        3,
        "fire",
    )
    .await;

    assert!(
        lines.iter().any(|l| l == "A_FIRED"),
        "package A's subscription must survive B's same-token off() (isolate-scoped removal); transcript:\n{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l == "B_FIRED"),
        "package B unsubscribed itself, so its handler must not fire; transcript:\n{lines:#?}"
    );
}

/// Like [`run_scenario`] but the caller supplies the package-provider `factory` directly (plus the
/// specifiers to install untrusted), instead of one built from a fixed `TestPackage` set. Needed
/// when the two isolates must resolve the *same* package key to *different* versions — which a
/// single fixed set can't express (`InMemoryPackageProvider` keeps one "latest" per key), but a
/// call-order-stateful factory can.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_with_factory(
    session_id: u32,
    server: &str,
    local_modules: &[(&str, &str)],
    install_specifiers: &[&str],
    factory: PackageProviderFactory,
    gate: &str,
    gate_count: usize,
    input: &str,
) -> Vec<String> {
    let _test_guard = PACKAGE_ISOLATE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let session_id = SessionId::from(session_id);

    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home = smudgy_core::get_smudgy_home().expect("smudgy home");
    let server_dir = home.join(server);
    let modules_dir = server_dir.join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(server_dir.join("logs")).unwrap();
    for (name, source) in local_modules {
        std::fs::write(modules_dir.join(name), source).unwrap();
    }
    for spec in install_specifiers {
        shared_packages::install_package(server, spec, UpdateMode::Auto, true).unwrap();
        // Grant the full smudgy capability set so the sandboxed packages can use the gated
        // ops these singleton/coexistence tests rely on (`createAlias` / `echo`). See `run_scenario`.
        shared_packages::record_consent(
            server,
            spec,
            &PackagePermissions {
                smudgy: SmudgyCapabilities::all(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    let params = Arc::new(SessionParams {
        session_id,
        server_name: Arc::new(server.to_string()),
        profile_name: Arc::new("test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });

    let mut events = Box::pin(spawn_with_package_provider(params, factory));
    let mut lines: Vec<String> = Vec::new();
    let tx = loop {
        let event = tokio::time::timeout(Duration::from_mins(1), events.next())
            .await
            .expect("timed out waiting for RuntimeReady")
            .expect("event stream ended before RuntimeReady");
        match event.event {
            SessionEvent::RuntimeReady(tx) => break tx,
            SessionEvent::UpdateBuffer(updates) => {
                for update in updates.iter() {
                    if let BufferUpdate::Append(line) = update {
                        lines.push(line.text.clone());
                    }
                }
            }
            _ => {}
        }
    };

    tx.send(RuntimeAction::ApplySettings {
        command_separator: Arc::new(";".to_string()),
        raw_line_prefix: Arc::new("\\".to_string()),
        log_enabled: true,
        script_settings: Box::new(smudgy_core::models::settings::ScriptSettings::default()),
    })
    .unwrap();

    let mut seen_gate = 0usize;
    let mut sent = false;
    while let Ok(Some(event)) = tokio::time::timeout(QUIET_PERIOD, events.next()).await {
        if let SessionEvent::UpdateBuffer(updates) = event.event {
            for update in updates.iter() {
                if let BufferUpdate::Append(line) = update {
                    lines.push(line.text.clone());
                    if !sent && line.text == gate {
                        seen_gate += 1;
                        if seen_gate >= gate_count {
                            tx.send(RuntimeAction::Send(Arc::new(input.to_string())))
                                .unwrap();
                            sent = true;
                        }
                    }
                }
            }
        }
    }
    tx.send(RuntimeAction::Shutdown)
        .expect("runtime accepts shutdown");
    drop(tx);
    drop(events);
    let joined = tokio::task::spawn_blocking(move || join_runtime_thread(session_id))
        .await
        .expect("runtime join task does not panic");
    assert_eq!(joined, RuntimeThreadJoinOutcome::Clean { session_id });

    assert!(
        sent,
        "gate sentinel {gate:?} (x{gate_count}) was never observed; transcript:\n{lines:#?}"
    );
    lines
}

/// `singleton` collapses across *versions*: `wbk/mapper@1` runs in one isolate
/// and `wbk/mapper@2` in another, and each singleton-registers `heal`. Because the singleton key
/// drops the version (`PACKAGE-ISOLATES.md` — dedupe scope is `(owner, name)`, not
/// `(owner, name, major)`), the two collapse to a single live `heal`. The in-memory provider keeps
/// only one "latest" per key, so the factory hands its two construction-order calls (main first,
/// then the sandbox) different versions; each version's source echoes its own version + whether it
/// won the reservation.
#[tokio::test]
async fn singleton_collapses_across_versions() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Call #0 (main's loader) → mapper@1; call #1 (the sandbox) → mapper@2. Each version echoes
    // its own number and whether its singleton `heal` was created or already existed.
    let call = Arc::new(AtomicUsize::new(0));
    let factory: PackageProviderFactory = Arc::new(move || {
        let n = call.fetch_add(1, Ordering::SeqCst);
        let version = if n == 0 { "1.0.0" } else { "2.0.0" };
        let mut provider = InMemoryPackageProvider::new();
        provider.insert(ResolvedPackage {
            key: PackageKey {
                owner: "wbk".to_string(),
                name: "mapper".to_string(),
            },
            resolved_version: version.to_string(),
            manifest: PackageManifest::parse(&format!(
                "{{ \"name\": \"mapper\", \"version\": \"{version}\" }}"
            ))
            .expect("valid manifest"),
            integrity: format!("test-mapper-{version}"),
            modules: vec![PackageModuleSource {
                subpath: "index.js".to_string(),
                text: format!(
                    r#"
                    import {{ createAlias, echo }} from "smudgy:core";
                    const a = createAlias("^heal$", () => {{ echo("healed"); }}, {{ singleton: true }});
                    echo("mapper {version}: " + (a.created ? "created" : "existed"));
                    echo("MAPPER_READY");
                    "#
                ),
            }],
        });
        let provider: Rc<dyn PackageProvider> = Rc::new(provider);
        provider
    });

    let lines = run_with_factory(
        9213,
        "pi_singleton_versions",
        // Importing mapper pulls a (different-versioned) copy into the main isolate.
        &[("main_mod.ts", r#"import "smudgy://wbk/mapper";"#)],
        &["smudgy://wbk/mapper"],
        factory,
        "MAPPER_READY",
        2,
        "heal",
    )
    .await;

    // Both versions really evaluated (two heaps), proving these are two versions, not two copies.
    assert!(
        lines.iter().any(|l| l.starts_with("mapper 1.0.0:")),
        "mapper@1 must have loaded; transcript:\n{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("mapper 2.0.0:")),
        "mapper@2 must have loaded; transcript:\n{lines:#?}"
    );
    // Exactly one version won the version-independent singleton reservation; the other no-oped.
    assert_eq!(
        lines.iter().filter(|l| l.ends_with(": created")).count(),
        1,
        "exactly one version may register the singleton heal; transcript:\n{lines:#?}"
    );
    assert_eq!(
        lines.iter().filter(|l| l.ends_with(": existed")).count(),
        1,
        "the other version's singleton heal must no-op; transcript:\n{lines:#?}"
    );
    // The single surviving `heal` fires exactly once.
    assert_eq!(
        lines.iter().filter(|l| *l == "healed").count(),
        1,
        "exactly one heal alias may be registered session-wide; transcript:\n{lines:#?}"
    );
}

/// The bulk `createTriggers` helper must forward `{ singleton: true }` too, so a
/// package that batch-registers its load-time automations with `singleton`
/// actually dedupes through that path, not just via the single `createTrigger`/`createAlias`. The same
/// package runs in two isolates (its sandboxed install + a copy a local module imported into main); each
/// calls `createTriggers({ dup: { …, singleton: true } })`. The forwarded flag drives the op exactly as
/// the single-create path does, so exactly ONE registers: the first copy's handle reports
/// `created === true`, the second `false`. (Triggers fire on *received* lines, not sent commands, so this
/// asserts the reservation via the returned handle's `created` rather than via firing.) The `sink` alias
/// in the main module just absorbs the harness's post-gate input so it never reaches the (absent) socket.
#[tokio::test]
async fn singleton_dedupes_via_create_triggers_across_isolates() {
    let pkg = TestPackage::new(
        "wbk",
        "batch",
        "1.0.0",
        r#"
        import { createTriggers, echo } from "smudgy:core";
        const t = createTriggers({
            dup: { patterns: ["^dup$"], script: () => { echo("fired_dup"); }, singleton: true },
        });
        echo(t.dup.created ? "created_true" : "created_false");
        echo("PKG_READY");
        "#,
    );
    let lines = run_scenario(
        9214,
        "pi_singleton_create_triggers",
        &[(
            "main_mod.ts",
            r#"
            import { createAlias } from "smudgy:core";
            import "smudgy://wbk/batch";
            createAlias("^sink$", () => {});
            "#,
        )],
        vec![pkg],
        "PKG_READY",
        2,
        "sink",
    )
    .await;

    assert_eq!(
        lines.iter().filter(|l| *l == "created_true").count(),
        1,
        "exactly one createTriggers copy must win the singleton reservation; transcript:\n{lines:#?}"
    );
    assert_eq!(
        lines.iter().filter(|l| *l == "created_false").count(),
        1,
        "the second createTriggers copy's singleton must report it already existed — the flag must be \
         forwarded through the bulk helper, not dropped; transcript:\n{lines:#?}"
    );
}
