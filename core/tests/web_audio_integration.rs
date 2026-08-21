#![cfg(feature = "web-audio")]

//! End-to-end Web Audio smoke through a real Smudgy trusted session isolate.
//!
//! The silent system sink keeps this deterministic and hardware-independent;
//! the same extension uses CPAL when the app enables `web-audio-cpal` and a
//! script constructs the default `AudioContext`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use smudgy_audio::{
    AudioSessionId,
    test_support::{TestDriverConfig, start_test_mixer},
};
use smudgy_audio_web::{ApplicationAudioOwner, SessionAudioRegistration};
use smudgy_core::session::runtime::{RuntimeAction, join_runtime_threads};
use smudgy_core::session::{
    BufferUpdate, SessionEvent, SessionId, SessionParams, registry, spawn, spawn_with_audio,
};

const WEB_AUDIO_TS: &str = r#"
import { echo } from "smudgy:core";

const required = [
  AudioContext,
  OfflineAudioContext,
  AudioBuffer,
  AudioBufferSourceNode,
  GainNode,
  OscillatorNode,
];

if (!required.every((value) => typeof value === "function")) {
  throw new Error("Web Audio constructors were not installed");
}

const context = new AudioContext({ sampleRate: 48_000, sinkId: "none" });
const gain = context.createGain();
const oscillator = context.createOscillator();

gain.gain.value = 0.05;
oscillator.frequency.value = 440;
oscillator.connect(gain);
gain.connect(context.destination);

oscillator.onended = async () => {
  await context.close();
  echo("WEB_AUDIO_OK");
};

oscillator.start();
oscillator.stop(context.currentTime + 0.02);
"#;

// These integration cases all exercise process-global V8/session registries
// and explicit mixer shutdown. Keep their full lifetimes disjoint inside this
// test binary; their internal trusted/package isolates still run concurrently.
static AUDIO_TEST_LOCK: Mutex<()> = Mutex::new(());

fn audio_registration(
    session_id: u32,
) -> (
    smudgy_audio::MixerService,
    ApplicationAudioOwner,
    SessionAudioRegistration,
) {
    let (service, _probe) = start_test_mixer(48_000, TestDriverConfig::default())
        .expect("headless process mixer starts");
    let mixer_owner = service
        .add_session(AudioSessionId(u64::from(session_id)))
        .expect("test session joins process mixer");
    let application = ApplicationAudioOwner::new(
        deno_audio::AudioHostLimits::unlimited()
            .max_online_contexts(Some(8))
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
        .expect("session audio registration succeeds");
    (service, application, registration)
}

#[tokio::test]
async fn trusted_smudgy_script_can_render_and_close_web_audio() {
    let _guard = AUDIO_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home_path = smudgy_core::get_smudgy_home().expect("smudgy home");

    let server = "WebAudioSmoke";
    let modules_dir = home_path.join(server).join("modules");
    std::fs::create_dir_all(&modules_dir).expect("create module directory");
    std::fs::create_dir_all(home_path.join(server).join("logs")).expect("create log directory");
    std::fs::write(modules_dir.join("web-audio.ts"), WEB_AUDIO_TS).expect("write Web Audio script");

    let session_id = 7_401;
    let params = Arc::new(SessionParams {
        session_id: SessionId::from(session_id),
        server_name: Arc::new(server.to_string()),
        profile_name: Arc::new("Test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });

    let (service, _application, registration) = audio_registration(session_id);
    let mut events = Box::pin(
        spawn_with_audio(params, registration.scope()).expect("scope matches core session"),
    );
    let mut runtime_tx = None;
    let mut transcript = Vec::new();

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events
                .next()
                .await
                .expect("session ended before Web Audio smoke completed");
            match event.event {
                SessionEvent::RuntimeReady(tx) => runtime_tx = Some(tx),
                SessionEvent::UpdateBuffer(updates) => {
                    for update in updates.iter() {
                        if let BufferUpdate::Append(line) = update {
                            transcript.push(line.text.clone());
                            if line.text == "WEB_AUDIO_OK" {
                                return;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for Web Audio\n{}", transcript.join("\n")));

    runtime_tx
        .expect("RuntimeReady should precede completion")
        .send(RuntimeAction::Shutdown)
        .ok();
    drop(events);
    join_runtime_threads();
    drop(registration);
    assert!(service.shutdown().clean);
}

#[tokio::test]
async fn legacy_spawn_installs_no_web_audio_or_output_fallback() {
    let _guard = AUDIO_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home_path = smudgy_core::get_smudgy_home().expect("smudgy home");
    let server = "WebAudioLegacyNone";
    let modules_dir = home_path.join(server).join("modules");
    std::fs::create_dir_all(&modules_dir).expect("create module directory");
    std::fs::create_dir_all(home_path.join(server).join("logs")).expect("create log directory");
    std::fs::write(
        modules_dir.join("legacy.ts"),
        r#"
        import { echo } from "smudgy:core";
        echo(`LEGACY_AUDIO:${typeof globalThis.AudioContext}`);
        "#,
    )
    .expect("write legacy probe");
    let params = Arc::new(SessionParams {
        session_id: SessionId::from(7_402),
        server_name: Arc::new(server.to_string()),
        profile_name: Arc::new("Test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });
    let mut events = Box::pin(spawn(params));
    let mut shutdown = None;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events.next().await.expect("legacy session stays live");
            match event.event {
                SessionEvent::RuntimeReady(tx) => shutdown = Some(tx),
                SessionEvent::UpdateBuffer(updates)
                    if updates.iter().any(|update| {
                        matches!(update, BufferUpdate::Append(line) if line.text == "LEGACY_AUDIO:undefined")
                    }) =>
                {
                    return;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("legacy no-audio probe completes");
    shutdown.unwrap().send(RuntimeAction::Shutdown).ok();
    drop(events);
    join_runtime_threads();
}

#[test]
fn mismatched_audio_scope_is_rejected_before_runtime_publication() {
    let _guard = AUDIO_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (service, _application, registration) = audio_registration(7_403);
    let params = Arc::new(SessionParams {
        session_id: SessionId::from(7_404),
        server_name: Arc::new("WebAudioMismatch".to_string()),
        profile_name: Arc::new("Test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });
    let error = match spawn_with_audio(params, registration.scope()) {
        Ok(_) => panic!("cross-session scope must be rejected before spawn"),
        Err(error) => error,
    };
    assert_eq!(error.session_id, SessionId::from(7_404));
    assert_eq!(error.audio_session_id, 7_403);
    assert!(registry::get_runtime(SessionId::from(7_404)).is_none());
    drop(registration);
    assert!(service.shutdown().clean);
}

#[tokio::test]
async fn two_registered_sessions_contend_on_one_application_host() {
    let _guard = AUDIO_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home_path = smudgy_core::get_smudgy_home().expect("smudgy home");
    let first_server = "WebAudioSharedHostFirst";
    let second_server = "WebAudioSharedHostSecond";
    for server in [first_server, second_server] {
        std::fs::create_dir_all(home_path.join(server).join("modules"))
            .expect("create module directory");
        std::fs::create_dir_all(home_path.join(server).join("logs")).expect("create log directory");
    }
    std::fs::write(
        home_path.join(first_server).join("modules/hold.ts"),
        r#"
        import { echo } from "smudgy:core";
        globalThis.__held_audio_context = new AudioContext({ sampleRate: 48_000, sinkId: "none" });
        echo("FIRST_CONTEXT_HELD");
        "#,
    )
    .expect("write first host probe");
    std::fs::write(
        home_path.join(second_server).join("modules/contend.ts"),
        r#"
        import { echo } from "smudgy:core";
        const wasContended = localStorage.getItem("smudgy-shared-host-contended") === "yes";
        try {
          globalThis.__held_audio_context = new AudioContext({ sampleRate: 48_000, sinkId: "none" });
          echo(wasContended ? "SECOND_REPLACEMENT_HELD" : "SHARED_HOST_LIMIT_MISSED");
        } catch (error) {
          localStorage.setItem("smudgy-shared-host-contended", "yes");
          echo(`SECOND_SHARED_HOST_LIMIT:${String(error)}`);
        }
        "#,
    )
    .expect("write second host probe");

    let first_id = 7_406;
    let second_id = 7_407;
    let params = |session_id: u32, server: &str| {
        Arc::new(SessionParams {
            session_id: SessionId::from(session_id),
            server_name: Arc::new(server.to_string()),
            profile_name: Arc::new("Test".to_string()),
            profile_subtext: Arc::new(String::new()),
            mapper: None,
            package_client: None,
            extra_script_extensions: Arc::new(Vec::new),
            on_engine_rebuild: None,
        })
    };
    let (service, _probe) = start_test_mixer(48_000, TestDriverConfig::default())
        .expect("headless process mixer starts");
    let first_owner = service
        .add_session(AudioSessionId(u64::from(first_id)))
        .expect("first session joins process mixer");
    let second_owner = service
        .add_session(AudioSessionId(u64::from(second_id)))
        .expect("second session joins process mixer");
    let application = ApplicationAudioOwner::new(
        deno_audio::AudioHostLimits::unlimited()
            .max_online_contexts(Some(1))
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
    let registrar = application.registrar();
    let first_registration = registrar
        .register_session(first_owner)
        .expect("first session registration succeeds");
    let second_registration = registrar
        .register_session(second_owner)
        .expect("second session registration succeeds");

    let mut first_events = Box::pin(
        spawn_with_audio(params(first_id, first_server), first_registration.scope())
            .expect("first scope matches"),
    );
    let mut first_tx = None;
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut held = false;
        while first_tx.is_none() || !held {
            let event = first_events.next().await.expect("first session stays live");
            match event.event {
                SessionEvent::RuntimeReady(tx) => first_tx = Some(tx),
                SessionEvent::UpdateBuffer(updates) => {
                    held |= updates.iter().any(|update| {
                        matches!(update, BufferUpdate::Append(line) if line.text == "FIRST_CONTEXT_HELD")
                    });
                }
                _ => {}
            }
        }
    })
    .await
    .expect("first session holds its context");
    assert_eq!(application.usage().online_contexts(), 1);

    let mut second_events = Box::pin(
        spawn_with_audio(
            params(second_id, second_server),
            second_registration.scope(),
        )
        .expect("second scope matches"),
    );
    let mut second_tx = None;
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut contended = false;
        while second_tx.is_none() || !contended {
            let event = second_events.next().await.expect("second session stays live");
            match event.event {
                SessionEvent::RuntimeReady(tx) => second_tx = Some(tx),
                SessionEvent::UpdateBuffer(updates) => {
                    contended |= updates.iter().any(|update| {
                        matches!(update, BufferUpdate::Append(line) if line.text.starts_with("SECOND_SHARED_HOST_LIMIT:") && line.text.contains("QuotaExceededError"))
                    });
                    assert!(!updates.iter().any(|update| {
                        matches!(update, BufferUpdate::Append(line) if line.text == "SHARED_HOST_LIMIT_MISSED")
                    }));
                }
                _ => {}
            }
        }
    })
    .await
    .expect("second session observes the first session's host permit");
    assert_eq!(application.usage().online_contexts(), 1);

    first_tx.take().unwrap().send(RuntimeAction::Shutdown).ok();
    drop(first_events);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while application.usage().online_contexts() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "first session shutdown did not return its application permit"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    second_tx
        .as_ref()
        .unwrap()
        .send(RuntimeAction::Reload)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = second_events.next().await.expect("replacement session stays live");
            if let SessionEvent::UpdateBuffer(updates) = event.event
                && updates.iter().any(|update| {
                    matches!(update, BufferUpdate::Append(line) if line.text == "SECOND_REPLACEMENT_HELD")
                })
            {
                return;
            }
        }
    })
    .await
    .expect("second session acquires the returned shared permit after reload");
    assert_eq!(application.usage().online_contexts(), 1);

    second_tx.take().unwrap().send(RuntimeAction::Shutdown).ok();
    drop(second_events);
    join_runtime_threads();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while application.usage().online_contexts() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "second session shutdown did not return its application permit"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(first_registration);
    drop(second_registration);
    assert!(service.shutdown().clean);
}

#[tokio::test]
async fn reload_reuses_exact_scope_and_global_host_limit_precedes_output() {
    let _guard = AUDIO_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home_path = smudgy_core::get_smudgy_home().expect("smudgy home");
    let server = "WebAudioReloadScope";
    let modules_dir = home_path.join(server).join("modules");
    std::fs::create_dir_all(&modules_dir).expect("create module directory");
    std::fs::create_dir_all(home_path.join(server).join("logs")).expect("create log directory");
    std::fs::write(
        modules_dir.join("reload.ts"),
        r#"
        import { echo } from "smudgy:core";
        const first = new AudioContext({ sampleRate: 48_000, sinkId: "none" });
        try {
          new AudioContext({ sampleRate: 48_000, sinkId: "named-device" });
          echo("HOST_LIMIT_MISSED");
        } catch (error) {
          echo(`HOST_LIMIT:${String(error)}`);
        }
        await first.close();
        try {
          new AudioContext({ sampleRate: 48_000, sinkId: "named-device" });
          echo("OUTPUT_REJECTION_MISSED");
        } catch (error) {
          echo(`OUTPUT_REJECTION:${String(error)}`);
        }
        globalThis.__held_audio_context = new AudioContext({ sampleRate: 48_000, sinkId: "none" });
        echo("RELOAD_SCOPE_OK");
        "#,
    )
    .expect("write reload probe");
    let session_id = 7_405;
    let params = Arc::new(SessionParams {
        session_id: SessionId::from(session_id),
        server_name: Arc::new(server.to_string()),
        profile_name: Arc::new("Test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });
    let (service, _probe) = start_test_mixer(48_000, TestDriverConfig::default())
        .expect("headless process mixer starts");
    let mixer_owner = service
        .add_session(AudioSessionId(u64::from(session_id)))
        .expect("reload session joins process mixer");
    let application = ApplicationAudioOwner::new(
        deno_audio::AudioHostLimits::unlimited()
            .max_online_contexts(Some(1))
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
        .expect("reload session audio registration succeeds");
    let scope = registration.scope();
    let mut events =
        Box::pin(spawn_with_audio(params, scope.clone()).expect("scope matches reload session"));
    let mut tx = None;
    let mut reload_count = 0;
    let mut host_limit_count = 0;
    let mut output_rejection_count = 0;
    let mut transcript = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let event = events.next().await.expect("reload session remains live");
            match event.event {
                SessionEvent::RuntimeReady(runtime_tx) => tx = Some(runtime_tx),
                SessionEvent::UpdateBuffer(updates) => {
                    for update in updates.iter() {
                        if let BufferUpdate::Append(line) = update {
                            transcript.push(line.text.clone());
                            if line.text.starts_with("HOST_LIMIT:")
                                && line.text.contains("QuotaExceededError")
                            {
                                host_limit_count += 1;
                            }
                            if line.text.starts_with("OUTPUT_REJECTION:")
                                && line.text.contains("audio output was rejected")
                            {
                                output_rejection_count += 1;
                            }
                            if line.text == "HOST_LIMIT_MISSED" {
                                panic!("second context bypassed host limit: {transcript:#?}");
                            }
                            if line.text == "OUTPUT_REJECTION_MISSED" {
                                panic!("unsupported sink bypassed output factory: {transcript:#?}");
                            }
                            if line.text == "RELOAD_SCOPE_OK" {
                                reload_count += 1;
                                assert_eq!(
                                    application.usage().online_contexts(),
                                    1,
                                    "the current runtime generation must hold one permit on the application host"
                                );
                                if reload_count == 1 {
                                    tx.as_ref().unwrap().send(RuntimeAction::Reload).unwrap();
                                } else {
                                    return;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("reload audio scope timed out: {transcript:#?}"));
    assert_eq!(reload_count, 2);
    assert_eq!(host_limit_count, 2);
    assert_eq!(output_rejection_count, 2);
    registration.native_bus().set_gain(0.75).unwrap();
    tx.unwrap().send(RuntimeAction::Shutdown).ok();
    drop(events);
    join_runtime_threads();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while application.usage().online_contexts() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "runtime shutdown did not release the exact host permit"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(scope);
    drop(registration);
    assert!(service.shutdown().clean);
}
