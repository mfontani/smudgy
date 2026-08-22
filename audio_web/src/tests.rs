use std::future::Future;
use std::panic::panic_any;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use futures::executor::block_on;
use smudgy_audio::{
    AudioSessionId, MixerControlError, MixerFrame, MixerInput, MixerInputStatus, MixerService,
    test_support::{TestDriverConfig, TestDriverProbe, TestInputOpenPause, start_test_mixer},
};
use web_audio_api::context::{
    AudioContext, AudioContextOptions, AudioContextShutdownOutcome, AudioContextState,
    BaseAudioContext,
};
use web_audio_api::node::{AudioNode, AudioScheduledSourceNode};

use super::*;

const TEST_RATE: u32 = 48_000;
const TEST_RATE_F32: f32 = 48_000.0;
const INPUT_CAPACITY: usize = 32;

#[test]
fn package_audio_usage_observer_retries_until_first_accepted_prepare() {
    let reports = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reports);
    let output: Arc<dyn AudioOutputFactory> = observe_audio_use(
        Arc::new(SilentAudioOutput::new()),
        Some(Arc::new(move || {
            observed.fetch_add(1, Ordering::AcqRel) > 0
        })),
    );

    let named = AudioContext::builder(Arc::clone(&output))
        .options(AudioContextOptions {
            sink_id: "named-device".into(),
            ..AudioContextOptions::default()
        })
        .build();
    assert!(named.is_err());
    assert_eq!(reports.load(Ordering::Acquire), 0);

    let first = AudioContext::builder(Arc::clone(&output)).build().unwrap();
    assert_eq!(reports.load(Ordering::Acquire), 1);
    let second = AudioContext::builder(Arc::clone(&output)).build().unwrap();
    let third = AudioContext::builder(output).build().unwrap();
    assert_eq!(reports.load(Ordering::Acquire), 2);
    first.close_sync();
    second.close_sync();
    third.close_sync();
}

struct HostilePayload;

impl Drop for HostilePayload {
    fn drop(&mut self) {
        panic!("hostile panic payload destructor");
    }
}

#[derive(Clone, Default)]
struct RenderProbe(Option<TestDriverProbe>);

impl RenderProbe {
    fn render(&self) -> [f32; INTERLEAVED_SAMPLES] {
        let mut output = [0.0; INTERLEAVED_SAMPLES];
        self.0
            .as_ref()
            .expect("renderer is live")
            .render(&mut output, 2)
            .expect("valid fake physical render");
        output
    }

    fn fail_output(&self) -> bool {
        self.0.as_ref().expect("renderer is live").fail_output()
    }

    fn physical_start_count(&self) -> usize {
        self.0.as_ref().expect("renderer is live").start_count()
    }

    fn physical_play_count(&self) -> usize {
        self.0.as_ref().expect("renderer is live").play_count()
    }

    fn pause_next_input_open_after_snapshot(&self) -> TestInputOpenPause {
        self.0
            .as_ref()
            .expect("renderer is live")
            .pause_next_input_open_after_snapshot()
    }

    fn pause_next_input_open_before_publish(&self) -> TestInputOpenPause {
        self.0
            .as_ref()
            .expect("renderer is live")
            .pause_next_input_open_before_publish()
    }
}

fn service(session_id: u64) -> (MixerService, smudgy_audio::MixerSessionHandle, RenderProbe) {
    let (service, probe) =
        start_test_mixer(TEST_RATE, TestDriverConfig::default()).expect("probe mixer starts");
    let session = service
        .add_session(AudioSessionId(session_id))
        .expect("test session is admitted");
    (service, session, RenderProbe(Some(probe)))
}

fn assert_samples(output: &[f32], expected: f32) {
    assert!(
        output
            .iter()
            .all(|sample| (*sample - expected).abs() < 1.0e-5),
        "expected every sample to be {expected}, got {output:?}"
    );
}

fn assert_gain(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < f32::EPSILON,
        "expected gain {expected}, got {actual}"
    );
}

fn render_until(probe: &RenderProbe, expected: f32) -> [f32; INTERLEAVED_SAMPLES] {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let output = probe.render();
        if output
            .iter()
            .all(|sample| (*sample - expected).abs() < 1.0e-5)
        {
            return output;
        }
        assert!(Instant::now() < deadline, "mixer never rendered {expected}");
        thread::yield_now();
    }
}

fn control_while_rendering(
    probe: &RenderProbe,
    context: &AudioContext,
    control: impl FnOnce(&AudioContext) + Send,
) {
    thread::scope(|scope| {
        let control = scope.spawn(|| control(context));
        while !control.is_finished() {
            let _ = probe.render();
            thread::yield_now();
        }
        control.join().expect("hosted lifecycle control succeeds");
    });
}

fn reserve_all(
    bus: &smudgy_audio::MixerScriptBusHandle,
) -> Vec<smudgy_audio::MixerInputReservation> {
    (0..INPUT_CAPACITY)
        .map(|_| bus.try_reserve_input().expect("expected free mixer slot"))
        .collect()
}

fn retire_reservations(reservations: Vec<smudgy_audio::MixerInputReservation>) {
    for reservation in reservations {
        assert!(block_on(reservation.abort()).unwrap().is_clean());
    }
}

#[test]
fn public_factory_and_dependency_boundary_compile() {
    fn assert_factory<T: AudioOutputFactory + Clone + Send + Sync>() {}
    fn assert_clone_send_sync<T: Clone + Send + Sync>() {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_factory::<ScriptBusAudioOutputFactory>();
    assert_factory::<SessionAudioOutputFactory>();
    assert_clone_send_sync::<ApplicationAudioRegistrar>();
    assert_clone_send_sync::<SessionAudioScope>();
    assert_send_sync::<ApplicationAudioOwner>();
    assert_send_sync::<SessionAudioRegistration>();
    assert_send_sync::<UnavailableSessionAudioRegistration>();
}

fn authority_limits(max_online_contexts: usize) -> AudioHostLimits {
    AudioHostLimits::unlimited()
        .max_online_contexts(Some(max_online_contexts))
        .max_live_audio_bytes(Some(16 * 1024 * 1024))
        .max_graph_nodes(Some(256))
        .max_graph_connections(Some(256))
        .max_scheduled_sources(Some(128))
        .max_automation_events(Some(512))
        .max_queued_control_commands(Some(128))
        .max_queued_events(Some(128))
        .max_decode_jobs(Some(2))
        .max_offline_render_jobs(Some(2))
}

fn scoped_context(
    scope: &SessionAudioScope,
    sink_id: &str,
) -> Result<AudioContext, web_audio_api::context::AudioContextBuildError> {
    AudioContext::builder(Arc::clone(&scope.inner.output))
        .options(AudioContextOptions {
            sample_rate: Some(TEST_RATE_F32),
            sink_id: sink_id.into(),
            ..AudioContextOptions::default()
        })
        .number_of_channels(CHANNELS)
        .build()
}

fn await_session_retirement(
    retirement: &mut smudgy_audio::MixerSessionRetirement,
    probe: &RenderProbe,
) {
    let wake = Waker::from(Arc::new(WakeProbe::default()));
    let mut context = Context::from_waker(&wake);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Poll::Ready(result) = Pin::new(&mut *retirement).poll(&mut context) {
            result.expect("the registered mixer session retires exactly");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "registered session retirement was not acknowledged"
        );
        let _ = probe.render();
        thread::yield_now();
    }
}

#[test]
fn application_and_session_authorities_share_only_the_intended_scope() {
    let (service, first_owner, probe) = service(201);
    let second_owner = service.add_session(AudioSessionId(202)).unwrap();
    let first_script = first_owner.script_bus();
    let second_script = second_owner.script_bus();
    first_script.set_gain(0.5).unwrap();
    second_script.set_gain(1.0).unwrap();
    let mut application = ApplicationAudioOwner::new(authority_limits(4));
    let registrar = application.registrar();
    let first = registrar.register_session(first_owner).unwrap();
    let second = registrar.clone().register_session(second_owner).unwrap();
    let first_scope = first.scope();
    let second_scope = second.scope();

    assert_eq!(first_scope.session_id(), 201);
    assert_eq!(second_scope.session_id(), 202);
    assert_ne!(first_scope.inner.generation, second_scope.inner.generation);
    assert!(!Arc::ptr_eq(
        &first_scope.inner.session_gate,
        &second_scope.inner.session_gate
    ));
    first_scope
        .inner
        .permissions
        .check_playback("AudioContext")
        .unwrap();
    second_scope
        .inner
        .permissions
        .check_playback("AudioContext")
        .unwrap();
    assert!(
        first_scope
            .inner
            .permissions
            .check_capture("MediaStreamAudioSourceNode")
            .unwrap_err()
            .to_string()
            .contains("capture is not permitted")
    );

    // Each registered scope's default route reaches its own exact Script bus,
    // while the process mixer combines both on the one fake physical output.
    let first_default = scoped_context(&first_scope, "").unwrap();
    let second_default = scoped_context(&second_scope, "").unwrap();
    let _first_graph = attach_constant(&first_default, 0.125, 1.0);
    let _second_graph = attach_constant(&second_default, 0.25, 1.0);
    // The unequal per-bus gains make a wrong same-session factory pairing
    // observable: 0.125 * 0.5 + 0.25 * 1.0 = 0.3125.
    render_until(&probe, 0.3125);
    control_while_rendering(&probe, &first_default, AudioContext::close_sync);
    render_until(&probe, 0.25);
    control_while_rendering(&probe, &second_default, AudioContext::close_sync);
    render_until(&probe, 0.0);

    let first_none = scoped_context(&first_scope, "none").unwrap();
    let second_none = scoped_context(&second_scope, "none").unwrap();
    first_none.close_sync();
    second_none.close_sync();

    drop(first);
    assert!(
        first_scope
            .inner
            .permissions
            .check_playback("AudioContext")
            .is_err()
    );
    assert!(scoped_context(&first_scope, "none").is_err());
    assert!(scoped_context(&second_scope, "none").is_ok());

    assert!(application.seal());
    assert!(
        second_scope
            .inner
            .permissions
            .check_playback("AudioContext")
            .is_err()
    );
    assert!(scoped_context(&second_scope, "none").is_err());
    drop(second);
    assert!(service.shutdown().clean);
}

#[test]
fn hosted_registration_retains_exact_gain_authority_outside_script_scope() {
    let (service, owner, probe) = service(2_011);
    let mut application = ApplicationAudioOwner::new(authority_limits(2));
    let registrar = application.registrar();
    let registration = registrar.register_session(owner).unwrap();
    let first_key = registration.control_key();
    let scope = registration.scope();

    let applied = registration.set_gain_linear(0.375).unwrap();
    assert_gain(applied.linear(), 0.375);
    assert!(!applied.is_muted());
    let muted = registration.set_gain_muted(true).unwrap();
    assert_gain(muted.linear(), 0.375);
    assert!(muted.is_muted());
    assert_gain(muted.effective_linear(), 0.0);
    assert_eq!(registration.gain_output_failure(), None);

    // Script scopes remain limited to Web Audio construction/lifecycle. The
    // exact application-control key is minted and retained by registration.
    assert_eq!(scope.session_id(), 2_011);
    let mut retirement = registration.retire();
    await_session_retirement(&mut retirement, &probe);

    let replacement_owner = service.add_session(AudioSessionId(2_011)).unwrap();
    let replacement = registrar.register_session(replacement_owner).unwrap();
    assert_ne!(first_key, replacement.control_key());
    assert_gain(replacement.set_gain_muted(false).unwrap().linear(), 1.0);
    let mut retirement = replacement.retire();
    await_session_retirement(&mut retirement, &probe);

    assert!(application.seal());
    assert!(service.shutdown().clean);
}

#[test]
fn sealed_application_rejects_registration_without_consuming_owner() {
    let (service, owner, _probe) = service(203);
    let mut application = ApplicationAudioOwner::new(authority_limits(1));
    let registrar = application.registrar();
    assert!(application.seal());
    let failure = registrar
        .register_session(owner)
        .expect_err("sealed app rejects an exact session owner");
    assert_eq!(
        failure.error(),
        SessionAudioRegistrationError::ApplicationSealed
    );
    assert_eq!(failure.into_owner().session_id(), AudioSessionId(203));
    assert!(service.shutdown().clean);
}

#[test]
fn unavailable_registration_routes_without_mixer_state_and_retires_logically() {
    let mut application = ApplicationAudioOwner::new(authority_limits(4));
    let baseline = application.usage();
    let registrar = application.registrar();
    let cause = UnavailableAudioOutputCause::new(
        "physical output Enumerate failed (DeviceUnavailable): no default device",
    );
    let mut first = registrar
        .register_unavailable_session(AudioSessionId(220), cause.clone())
        .expect("mixer-free session registers");
    let second = registrar
        .register_unavailable_session(AudioSessionId(221), cause)
        .expect("a mixer-free sibling registers independently");
    let first_scope = first.scope();
    let second_scope = second.scope();

    let default = scoped_context(&first_scope, "")
        .expect("default uses the embedder-selected emulated output");
    assert_eq!(default.sink_id(), "");
    assert_eq!(default.state(), AudioContextState::Running);
    let started_at = default.current_time();
    let deadline = Instant::now() + Duration::from_secs(2);
    while default.current_time() <= started_at {
        assert!(
            Instant::now() < deadline,
            "emulated default output did not render"
        );
        thread::yield_now();
    }
    default.suspend_sync();
    assert_eq!(default.state(), AudioContextState::Suspended);
    default.resume_sync();
    assert_eq!(default.state(), AudioContextState::Running);
    default.close_sync();
    assert_eq!(default.state(), AudioContextState::Closed);
    assert_eq!(
        application.usage(),
        baseline,
        "closed emulated default releases host usage"
    );

    let named = scoped_context(&first_scope, "named-device")
        .expect_err("named output stays unsupported even without a device");
    assert_eq!(
        named.output_error().map(AudioOutputError::kind),
        Some(AudioOutputErrorKind::NotSupported)
    );
    assert_eq!(
        application.usage(),
        baseline,
        "failed named sink is non-mutating"
    );

    let silent = scoped_context(&first_scope, "none").expect("none stays joinable");
    silent.close_sync();
    assert_eq!(application.usage(), baseline);

    assert!(first.seal());
    assert!(!first.seal());
    assert!(scoped_context(&first_scope, "none").is_err());
    let first_retirement = first.retire();
    assert_eq!(first_retirement.session_id(), 220);
    assert_ne!(first_retirement.generation(), 0);

    let sibling = scoped_context(&second_scope, "none").expect("sibling remains live");
    sibling.close_sync();
    let second_retirement = second.retire();
    assert_eq!(second_retirement.session_id(), 221);
    assert_ne!(
        first_retirement.generation(),
        second_retirement.generation()
    );
    assert!(application.seal());
    assert!(matches!(
        registrar.register_unavailable_session(
            AudioSessionId(222),
            UnavailableAudioOutputCause::new("still unavailable")
        ),
        Err(SessionAudioRegistrationError::ApplicationSealed)
    ));
    assert_eq!(application.usage(), baseline);
}

#[test]
fn stale_same_id_scope_is_inert_after_replacement_generation() {
    let (service, owner, probe) = service(204);
    let application = ApplicationAudioOwner::new(authority_limits(4));
    let registrar = application.registrar();
    let registration = registrar.register_session(owner).unwrap();
    let stale = registration.scope();
    let stale_generation = stale.inner.generation;
    let mut retirement = registration.retire();
    assert!(scoped_context(&stale, "none").is_err());
    await_session_retirement(&mut retirement, &probe);

    let replacement_owner = service.add_session(AudioSessionId(204)).unwrap();
    let replacement = registrar.register_session(replacement_owner).unwrap();
    let current = replacement.scope();
    assert_eq!(current.session_id(), stale.session_id());
    assert_ne!(current.inner.generation, stale_generation);
    assert!(scoped_context(&stale, "none").is_err());
    let context = scoped_context(&current, "none").unwrap();
    context.close_sync();
    drop(replacement);
    assert!(service.shutdown().clean);
}

#[test]
fn prepare_and_session_seal_are_totally_ordered_at_delegate_boundary() {
    let (service, owner, probe) = service(205);
    let application = ApplicationAudioOwner::new(authority_limits(4));
    let registrar = application.registrar();
    let registration = registrar.register_session(owner).unwrap();
    let scope = registration.scope();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    *application
        .state
        .prepare_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(TestPrepareHook {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));

    let prepare_scope = scope.clone();
    let prepare = thread::spawn(move || scoped_context(&prepare_scope, "none"));
    entered.wait();
    let seal = thread::spawn(move || {
        let mut registration = registration;
        let first = registration.seal();
        let second = registration.seal();
        (registration, first, second)
    });
    assert!(
        !seal.is_finished(),
        "session sealing crossed an admitted factory transaction"
    );
    release.wait();
    let context = prepare
        .join()
        .unwrap()
        .expect("prepare admitted before seal completes");
    let (registration, first, second) = seal.join().unwrap();
    assert!(first, "the first explicit seal closes admission");
    assert!(!second, "session sealing is absorbing and idempotent");
    context.close_sync();

    *application
        .state
        .prepare_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    assert!(scoped_context(&scope, "none").is_err());
    assert_eq!(
        service.add_session(AudioSessionId(205)).unwrap_err(),
        MixerControlError::DuplicateSession,
        "sealing retains the exact mixer owner until explicit retirement"
    );
    let mut retirement = registration.retire();
    await_session_retirement(&mut retirement, &probe);
    let replacement = service
        .add_session(AudioSessionId(205))
        .expect("retirement receipt returns the exact mixer-session capacity");
    drop(replacement);
    assert!(service.shutdown().clean);
}

#[test]
fn prepare_and_application_seal_are_totally_ordered_at_delegate_boundary() {
    let (service, owner, _probe) = service(206);
    let application = ApplicationAudioOwner::new(authority_limits(4));
    let registrar = application.registrar();
    let registration = registrar.register_session(owner).unwrap();
    let scope = registration.scope();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    *application
        .state
        .prepare_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(TestPrepareHook {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));

    let prepare_scope = scope.clone();
    let prepare = thread::spawn(move || scoped_context(&prepare_scope, "none"));
    entered.wait();
    let seal = thread::spawn(move || {
        let mut application = application;
        application.seal()
    });
    assert!(
        !seal.is_finished(),
        "application sealing crossed an admitted factory transaction"
    );
    release.wait();
    let context = prepare
        .join()
        .unwrap()
        .expect("prepare admitted before application seal completes");
    assert!(seal.join().unwrap());
    context.close_sync();

    assert!(scoped_context(&scope, "none").is_err());
    drop(registration);
    assert!(service.shutdown().clean);
}

#[test]
fn unavailable_none_prepare_and_session_seal_are_totally_ordered() {
    let application = ApplicationAudioOwner::new(authority_limits(4));
    let mut registration = application
        .registrar()
        .register_unavailable_session(
            AudioSessionId(207),
            UnavailableAudioOutputCause::new("deterministic test device failure"),
        )
        .unwrap();
    let scope = registration.scope();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    *application
        .state
        .prepare_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(TestPrepareHook {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));

    let prepare_scope = scope.clone();
    let prepare = thread::spawn(move || scoped_context(&prepare_scope, "none"));
    entered.wait();
    let seal = thread::spawn(move || {
        let first = registration.seal();
        let second = registration.seal();
        (registration, first, second)
    });
    assert!(
        !seal.is_finished(),
        "mixer-free sealing crossed an admitted none transaction"
    );
    release.wait();
    let context = prepare
        .join()
        .unwrap()
        .expect("admitted mixer-free none preparation completes");
    let (registration, first, second) = seal.join().unwrap();
    assert!(first);
    assert!(!second);
    context.close_sync();

    *application
        .state
        .prepare_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    assert!(scoped_context(&scope, "none").is_err());
    let receipt = registration.retire();
    assert_eq!(receipt.session_id(), 207);
}

fn composite_context(
    factory: &SessionAudioOutputFactory,
    sink_id: &str,
    sample_rate: f32,
    number_of_channels: usize,
) -> Result<AudioContext, web_audio_api::context::AudioContextBuildError> {
    let output: Arc<dyn AudioOutputFactory> = Arc::new(factory.clone());
    AudioContext::builder(output)
        .options(AudioContextOptions {
            sample_rate: Some(sample_rate),
            sink_id: sink_id.into(),
            ..AudioContextOptions::default()
        })
        .number_of_channels(number_of_channels)
        .build()
}

fn bind_package_factory(
    scope: &SessionAudioScope,
    owner: &str,
    name: &str,
) -> (SessionAudioOutputFactory, PackageAudioScopeBinding) {
    let registry = scope
        .inner
        .package_audio
        .as_ref()
        .expect("physical scope owns package metadata");
    let (gain, mut binding) = registry.bind(owner, name).expect("package root binds");
    binding.commit().unwrap();
    (
        SessionAudioOutputFactory::with_package_gain(
            registry.bus.clone(),
            gain,
            Arc::new(AtomicBool::new(false)),
        ),
        binding,
    )
}

#[test]
fn complete_policy_preflights_before_session_mutation_and_resets_active_root_to_default() {
    let (service, session, probe) = service(211);
    let mut application = ApplicationAudioOwner::new(authority_limits(2));
    let registration = application.registrar().register_session(session).unwrap();
    let scope = registration.scope();

    registration
        .stage_gain_policy(
            0.4,
            true,
            1.0,
            false,
            [(Arc::from("owner"), Arc::from("bell"), 0.5, false)],
        )
        .unwrap();
    let (_factory, binding) = bind_package_factory(&scope, "OWNER", "BELL");
    let key = registration.package_control_key("owner", "bell").unwrap();
    let package = registration.set_package_gain_muted(&key, false).unwrap();
    assert_gain(package.linear(), 0.5);

    let error = registration
        .stage_gain_policy(
            0.8,
            false,
            0.4,
            true,
            [(Arc::from(""), Arc::from("bad"), 0.25, false)],
        )
        .unwrap_err();
    assert_eq!(
        error,
        SessionAudioPolicyError::Package(PackageAudioScopeError::InvalidIdentity)
    );
    let unchanged = registration.set_gain_muted(true).unwrap();
    assert_gain(unchanged.linear(), 0.4);
    assert!(unchanged.is_muted());

    registration
        .stage_gain_policy(
            1.0,
            false,
            0.4,
            true,
            [(Arc::from("owner"), Arc::from("bell"), 1.0, false)],
        )
        .unwrap();
    let reset = registration.set_package_gain_muted(&key, false).unwrap();
    assert_gain(reset.linear(), 1.0);
    assert!(!reset.is_muted());

    drop(binding);
    let mut retirement = registration.retire();
    await_session_retirement(&mut retirement, &probe);
    application.seal();
    assert!(service.shutdown().clean);
}

#[test]
fn default_policy_for_many_absent_roots_consumes_no_registry_capacity() {
    let (service, session, probe) = service(210);
    let mut application = ApplicationAudioOwner::new(authority_limits(1));
    let registration = application.registrar().register_session(session).unwrap();
    registration
        .stage_gain_policy(
            1.0,
            false,
            1.0,
            false,
            (0..MAX_PACKAGE_AUDIO_SCOPES + 10).map(|index| {
                (
                    Arc::from("owner"),
                    Arc::from(format!("default-{index}")),
                    1.0,
                    false,
                )
            }),
        )
        .unwrap();
    let entries = registration
        .scope()
        .inner
        .package_audio
        .as_ref()
        .unwrap()
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    assert_eq!(entries, 0);

    let mut retirement = registration.retire();
    await_session_retirement(&mut retirement, &probe);
    application.seal();
    assert!(service.shutdown().clean);
}

#[test]
#[allow(clippy::float_cmp)] // exact atomic snapshots are part of the gain contract
fn package_scopes_compose_isolate_remember_and_stale_exact_generations() {
    let (service, session, probe) = service(212);
    let mut application = ApplicationAudioOwner::new(authority_limits(8));
    let registration = application.registrar().register_session(session).unwrap();
    let scope = registration.scope();
    let main = scoped_context(&scope, "").unwrap();
    let (alpha_factory, alpha_binding) = bind_package_factory(&scope, "Owner", "Alpha");
    let (beta_factory, beta_binding) = bind_package_factory(&scope, "owner", "beta");
    let alpha_first = composite_context(&alpha_factory, "", TEST_RATE_F32, CHANNELS).unwrap();
    let beta = composite_context(&beta_factory, "", TEST_RATE_F32, CHANNELS).unwrap();
    let _main_graph = attach_constant(&main, 0.1, 1.0);
    let _alpha_first_graph = attach_constant(&alpha_first, 0.2, 1.0);
    let _beta_graph = attach_constant(&beta, 0.4, 1.0);

    let alpha_key = registration
        .package_control_key("owner", "ALPHA")
        .expect("package identity is ASCII-folded and versionless");
    let beta_key = registration.package_control_key("OWNER", "BETA").unwrap();
    assert_eq!(
        registration
            .set_package_gain_linear(&alpha_key, 0.5)
            .unwrap()
            .effective_linear(),
        0.5
    );
    registration
        .set_package_gain_linear(&beta_key, 0.25)
        .unwrap();

    // A context constructed after the mutation shares the same preallocated
    // atomic snapshot as its predecessor and the dependency closure would.
    let alpha_second = composite_context(&alpha_factory, "", TEST_RATE_F32, CHANNELS).unwrap();
    let _alpha_second_graph = attach_constant(&alpha_second, 0.1, 1.0);
    render_until(&probe, 0.35);

    let muted = registration
        .set_package_gain_muted(&alpha_key, true)
        .unwrap();
    assert!(muted.is_muted());
    assert_eq!(muted.linear(), 0.5);
    render_until(&probe, 0.2);
    let remembered = registration
        .set_package_gain_linear(&alpha_key, 0.25)
        .unwrap();
    assert!(remembered.is_muted());
    assert_eq!(remembered.effective_linear(), 0.0);
    render_until(&probe, 0.2);
    registration
        .set_package_gain_muted(&alpha_key, false)
        .unwrap();
    render_until(&probe, 0.275);

    for context in [&main, &alpha_first, &alpha_second, &beta] {
        control_while_rendering(&probe, context, AudioContext::close_sync);
    }
    drop(alpha_binding);
    assert_eq!(
        registration.set_package_gain_muted(&alpha_key, false),
        Err(PackageAudioControlError::StalePackage)
    );
    let (replacement_factory, replacement_binding) = bind_package_factory(&scope, "OWNER", "alpha");
    let replacement_key = registration.package_control_key("owner", "alpha").unwrap();
    assert_ne!(alpha_key, replacement_key);
    assert_eq!(
        registration
            .set_package_gain_muted(&replacement_key, false)
            .unwrap()
            .linear(),
        0.25,
        "a successful version/root reload reuses remembered state"
    );
    let replacement = composite_context(&replacement_factory, "", TEST_RATE_F32, CHANNELS).unwrap();
    let _replacement_graph = attach_constant(&replacement, 0.4, 1.0);
    render_until(&probe, 0.1);
    control_while_rendering(&probe, &replacement, AudioContext::close_sync);
    drop(replacement_binding);
    drop(beta_binding);

    let mut retirement = registration.retire();
    await_session_retirement(&mut retirement, &probe);
    application.seal();
    assert!(service.shutdown().clean);
}

#[test]
fn package_scope_capacity_is_independent_bounded_and_failed_bindings_rollback() {
    let (service, session, probe) = service(213);
    let application = ApplicationAudioOwner::new(authority_limits(1));
    let registration = application.registrar().register_session(session).unwrap();
    let scope = registration.scope();

    let (_, pending) = scope
        .extension_options_for_sandbox_root("pending", "root")
        .unwrap();
    assert!(matches!(
        scope.extension_options_for_sandbox_root("PENDING", "ROOT"),
        Err(PackageAudioScopeError::AlreadyBinding)
    ));
    drop(pending);
    assert!(
        scope
            .inner
            .package_audio
            .as_ref()
            .unwrap()
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );

    let mut bindings = Vec::with_capacity(MAX_PACKAGE_AUDIO_SCOPES);
    for index in 0..MAX_PACKAGE_AUDIO_SCOPES {
        let (_, binding) = scope
            .extension_options_for_sandbox_root("bounded", &format!("root-{index}"))
            .unwrap();
        let mut binding = binding.expect("physical sandbox receives an exact lease");
        binding.commit().unwrap();
        bindings.push(binding);
    }
    assert!(matches!(
        scope.extension_options_for_sandbox_root("bounded", "overflow"),
        Err(PackageAudioScopeError::Capacity)
    ));
    drop(bindings);
    let mut retirement = registration.retire();
    await_session_retirement(&mut retirement, &probe);
    assert!(service.shutdown().clean);
}

#[test]
fn unavailable_sandbox_binding_allocates_no_package_state_or_capacity() {
    let mut application = ApplicationAudioOwner::new(authority_limits(1));
    let registration = application
        .registrar()
        .register_unavailable_session(
            AudioSessionId(214),
            UnavailableAudioOutputCause::new("no device"),
        )
        .unwrap();
    let scope = registration.scope();
    for index in 0..=MAX_PACKAGE_AUDIO_SCOPES {
        let (_, binding) = scope
            .extension_options_for_sandbox_root("unavailable", &format!("root-{index}"))
            .unwrap();
        assert!(binding.is_none());
    }
    assert!(scope.inner.package_audio.is_none());
    application.seal();
    drop(registration);
}

#[test]
fn pending_package_scope_cannot_publish_after_exact_session_seal() {
    let (service, session, probe) = service(215);
    let application = ApplicationAudioOwner::new(authority_limits(1));
    let mut registration = application.registrar().register_session(session).unwrap();
    let scope = registration.scope();
    let (_, binding) = scope
        .extension_options_for_sandbox_root("closing", "root")
        .unwrap();
    let mut binding = binding.unwrap();
    assert!(registration.seal());
    assert_eq!(binding.commit(), Err(PackageAudioScopeError::SessionClosed));
    drop(binding);
    assert!(
        scope
            .inner
            .package_audio
            .as_ref()
            .unwrap()
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    let mut retirement = registration.retire();
    await_session_retirement(&mut retirement, &probe);
    assert!(service.shutdown().clean);
}

#[test]
fn composite_none_is_slot_free_while_default_owns_the_only_available_slot() {
    let (service, session, probe) = service(114);
    let bus = session.script_bus();
    let factory = SessionAudioOutputFactory::new(bus.clone());
    // Leave one usable Script slot, then prove the default route consumes it.
    let held: Vec<_> = (0..INPUT_CAPACITY - 1)
        .map(|_| bus.try_reserve_input().unwrap())
        .collect();
    let default = composite_context(&factory, "", TEST_RATE_F32, CHANNELS)
        .expect("the default sink routes to the session Script bus");
    assert_eq!(default.sink_id(), "");
    assert!((default.sample_rate() - TEST_RATE_F32).abs() < f32::EPSILON);
    assert_eq!(default.destination().channel_count(), CHANNELS);
    assert!((default.output_latency() - 128.0 / f64::from(TEST_RATE)).abs() < f64::EPSILON);
    let _default_graph = attach_constant(&default, 0.25, 1.0);
    render_until(&probe, 0.25);
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));
    let capacity_emulated = composite_context(&factory, "", TEST_RATE_F32, CHANNELS)
        .expect("default output emulates silence when the physical Script bus is full");
    assert_eq!(capacity_emulated.sink_id(), "");
    assert!(capacity_emulated.output_latency().abs() < f64::EPSILON);
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));

    // Hosted contexts do not migrate between delegates. Construct separate
    // contexts to exercise each exact sink route.
    let first_none = composite_context(&factory, "none", 44_100.0, 1)
        .expect("none remains available while the Script bus is full");
    let second_none = composite_context(&factory, "none", TEST_RATE_F32, CHANNELS)
        .expect("independent none contexts do not consume mixer inputs");
    assert_eq!(first_none.sink_id(), "none");
    assert!((first_none.sample_rate() - 44_100.0).abs() < f32::EPSILON);
    assert_eq!(first_none.destination().channel_count(), 1);
    assert!(first_none.output_latency().abs() < f64::EPSILON);
    assert_eq!(second_none.sink_id(), "none");
    assert!((second_none.sample_rate() - TEST_RATE_F32).abs() < f32::EPSILON);
    assert_eq!(second_none.destination().channel_count(), CHANNELS);
    assert!(second_none.output_latency().abs() < f64::EPSILON);
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));

    for context in [&capacity_emulated, &first_none, &second_none] {
        assert!(matches!(
            context.request_close().unwrap().wait(),
            AudioContextShutdownOutcome::Confirmed(_)
        ));
        assert_eq!(context.state(), AudioContextState::Closed);
    }
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));

    control_while_rendering(&probe, &default, AudioContext::close_sync);
    render_until(&probe, 0.0);
    let replacement = bus
        .try_reserve_input()
        .expect("closing the default context returns its exact Script slot");
    assert!(block_on(replacement.abort()).unwrap().is_clean());
    retire_reservations(held);
    assert!(service.shutdown().clean);
}

#[test]
fn forced_emulated_default_preserves_explicit_logical_rate_while_physical_rejects_it() {
    let (service, session, _probe) = service(224);
    let bus = session.script_bus();
    let physical = SessionAudioOutputFactory::new(bus.clone());
    let error = composite_context(&physical, "", 44_100.0, CHANNELS)
        .expect_err("audible default output keeps the fixed process-mixer rate");
    assert_eq!(
        error.output_error().map(AudioOutputError::kind),
        Some(AudioOutputErrorKind::NotSupported)
    );

    let force_emulated = Arc::new(AtomicBool::new(true));
    let emulated = SessionAudioOutputFactory::with_force_emulated(bus, force_emulated);
    let context = composite_context(&emulated, "", 44_100.0, CHANNELS)
        .expect("a pre-forced silent generation keeps its requested logical rate");
    assert_eq!(context.state(), AudioContextState::Running);
    assert!((context.sample_rate() - 44_100.0).abs() < f32::EPSILON);
    let started_at = context.current_time();
    let deadline = Instant::now() + Duration::from_secs(2);
    while context.current_time() <= started_at {
        assert!(Instant::now() < deadline, "emulated clock did not advance");
        thread::yield_now();
    }
    assert!(matches!(
        context.request_close().unwrap().wait(),
        AudioContextShutdownOutcome::Confirmed(_)
    ));
    assert!(service.shutdown().clean);
}

#[test]
fn composite_unsupported_sink_is_stable_and_does_not_mutate_mixer_capacity() {
    let (service, session, _probe) = service(115);
    let bus = session.script_bus();
    let factory = SessionAudioOutputFactory::new(bus.clone());
    let error = composite_context(&factory, "named-device", TEST_RATE_F32, CHANNELS)
        .expect_err("a named device must not bypass the process mixer");
    assert_eq!(
        error.kind(),
        web_audio_api::context::AudioContextBuildErrorKind::OutputRejected
    );
    let output_error = error.output_error().expect("factory rejection is retained");
    assert_eq!(output_error.kind(), AudioOutputErrorKind::NotSupported);
    assert_eq!(
        output_error.message(),
        "Smudgy Web Audio supports only the default and none output sinks"
    );
    assert!(error.cleanup_receipt().is_none());

    let reservations = reserve_all(&bus);
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));
    retire_reservations(reservations);
    assert!(service.shutdown().clean);
}

#[test]
fn composite_routes_before_touching_a_retired_script_bus() {
    let (service, session, probe) = service(116);
    let bus = session.script_bus();
    let factory = SessionAudioOutputFactory::new(bus);
    let mut retirement = session.retire();

    let default_error = composite_context(&factory, "", TEST_RATE_F32, CHANNELS)
        .expect_err("the default route must observe its retired Script bus");
    assert_eq!(
        default_error.kind(),
        web_audio_api::context::AudioContextBuildErrorKind::OutputRejected
    );
    assert_eq!(
        default_error.output_error().map(AudioOutputError::kind),
        Some(AudioOutputErrorKind::BackendSpecific)
    );

    let none = composite_context(&factory, "none", 44_100.0, 1)
        .expect("the silent route is independent of a retired Script bus");
    assert_eq!(none.sink_id(), "none");
    assert!((none.sample_rate() - 44_100.0).abs() < f32::EPSILON);

    let unsupported = composite_context(&factory, "named-device", TEST_RATE_F32, CHANNELS)
        .expect_err("unsupported classification must not depend on bus state");
    let unsupported = unsupported.output_error().unwrap();
    assert_eq!(unsupported.kind(), AudioOutputErrorKind::NotSupported);
    assert_eq!(
        unsupported.message(),
        "Smudgy Web Audio supports only the default and none output sinks"
    );

    assert!(matches!(
        none.request_close().unwrap().wait(),
        AudioContextShutdownOutcome::Confirmed(_)
    ));
    let wake = Waker::from(Arc::new(WakeProbe::default()));
    let mut context = Context::from_waker(&wake);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Poll::Ready(result) = Pin::new(&mut retirement).poll(&mut context) {
            result.expect("the retired session is exactly acknowledged");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session retirement was not acknowledged"
        );
        let _ = probe.render();
        thread::yield_now();
    }
    assert!(service.shutdown().clean);
}

#[test]
fn composite_mixes_two_defaults_and_native_while_none_stays_private() {
    let (service, session, probe) = service(117);
    let script = session.script_bus();
    let native = session.native_bus();
    let factory = SessionAudioOutputFactory::new(script.clone());
    assert_eq!(probe.physical_start_count(), 1);
    assert_eq!(probe.physical_play_count(), 1);
    script.set_gain(0.5).unwrap();
    native.set_gain(0.25).unwrap();
    let native = native
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(ConstantInput(0.125)))
        .unwrap();

    let first = composite_context(&factory, "", TEST_RATE_F32, CHANNELS).unwrap();
    let second = composite_context(&factory, "", TEST_RATE_F32, CHANNELS).unwrap();
    let none = composite_context(&factory, "none", TEST_RATE_F32, CHANNELS).unwrap();
    let _first_graph = attach_constant(&first, 0.25, 0.5);
    let _second_graph = attach_constant(&second, 0.5, 0.5);
    let _none_graph = attach_constant(&none, 1.0, 1.0);

    // The none graph renders on its private silent owner and contributes
    // nothing to the process mixer.
    render_until(&probe, 0.21875);
    assert!(matches!(
        none.request_close().unwrap().wait(),
        AudioContextShutdownOutcome::Confirmed(_)
    ));
    render_until(&probe, 0.21875);
    assert_eq!(probe.physical_start_count(), 1);
    assert_eq!(probe.physical_play_count(), 1);

    control_while_rendering(&probe, &first, AudioContext::suspend_sync);
    render_until(&probe, 0.15625);
    control_while_rendering(&probe, &first, AudioContext::resume_sync);
    render_until(&probe, 0.21875);
    control_while_rendering(&probe, &first, AudioContext::close_sync);
    render_until(&probe, 0.15625);
    control_while_rendering(&probe, &second, AudioContext::close_sync);
    render_until(&probe, 0.03125);
    assert!(block_on(native.shutdown()).unwrap().is_clean());
    render_until(&probe, 0.0);
    assert!(service.shutdown().clean);
}

#[test]
fn physical_device_death_affects_default_but_none_closes_independently() {
    let (service, session, probe) = service(118);
    let script = session.script_bus();
    let factory = SessionAudioOutputFactory::new(script.clone());
    let default = composite_context(&factory, "", TEST_RATE_F32, CHANNELS).unwrap();
    let none = composite_context(&factory, "none", TEST_RATE_F32, CHANNELS).unwrap();
    let _default_graph = attach_constant(&default, 0.25, 1.0);
    let _none_graph = attach_constant(&none, 0.5, 1.0);
    render_until(&probe, 0.25);

    assert!(probe.fail_output());
    let default_close = default
        .request_close()
        .expect("physical death retains default-route cleanup proof");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match script.set_gain(0.75) {
            Err(MixerControlError::OwnerStopped) => break,
            Ok(()) | Err(MixerControlError::Saturated) => {
                assert!(
                    Instant::now() < deadline,
                    "device death did not seal the Script route"
                );
                thread::yield_now();
            }
            Err(error) => panic!("unexpected post-death Script result: {error:?}"),
        }
    }

    let emulated = composite_context(&factory, "", TEST_RATE_F32, CHANNELS)
        .expect("new default contexts use emulated output after exact device death");
    assert_eq!(emulated.sink_id(), "");
    assert_eq!(emulated.state(), AudioContextState::Running);
    let emulated_started_at = emulated.current_time();
    let deadline = Instant::now() + Duration::from_secs(2);
    while emulated.current_time() <= emulated_started_at {
        assert!(
            Instant::now() < deadline,
            "post-death emulated context did not render"
        );
        thread::yield_now();
    }
    emulated.close_sync();

    assert_eq!(none.state(), AudioContextState::Running);
    let AudioContextShutdownOutcome::Confirmed(none_report) = none.request_close().unwrap().wait()
    else {
        panic!("silent-route cleanup was not confirmed")
    };
    assert_eq!(none_report.endpoint_death(), None);
    assert_eq!(none.state(), AudioContextState::Closed);

    assert!(service.shutdown().clean);
    let AudioContextShutdownOutcome::Confirmed(default_report) = default_close.wait() else {
        panic!("default-route cleanup was not confirmed")
    };
    assert_eq!(
        default_report.endpoint_death(),
        Some(AudioOutputDeathReason::BackendFailure)
    );
    assert_eq!(default.state(), AudioContextState::Closed);
}

#[test]
fn device_death_after_default_prepare_reroutes_the_unpublished_callback_to_emulation() {
    let (service, session, probe) = service(219);
    let factory = SessionAudioOutputFactory::new(session.script_bus());
    let (prepared, prepared_rx) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let (cleanup_thread, _cleanup_thread_rx) = mpsc::sync_channel(1);
    let output: Arc<dyn AudioOutputFactory> = Arc::new(BlockAfterPrepareFactory {
        inner: Arc::new(factory),
        prepared,
        release: Arc::clone(&release),
        cleanup_thread,
    });

    let build = thread::spawn(move || {
        let context = AudioContext::builder(output)
            .options(AudioContextOptions {
                sample_rate: Some(TEST_RATE_F32),
                ..AudioContextOptions::default()
            })
            .number_of_channels(CHANNELS)
            .build()
            .expect("default construction silently recovers a prepared physical endpoint");
        assert_eq!(context.sink_id(), "");
        assert_eq!(context.state(), AudioContextState::Running);
        let started_at = context.current_time();
        let deadline = Instant::now() + Duration::from_secs(2);
        while context.current_time() <= started_at {
            assert!(
                Instant::now() < deadline,
                "recovered emulated output did not advance currentTime"
            );
            thread::yield_now();
        }
        let AudioContextShutdownOutcome::Confirmed(report) =
            context.request_close().unwrap().wait()
        else {
            panic!("physical and emulated cleanup was not jointly confirmed")
        };
        assert_eq!(report.endpoint_death(), None);
    });

    prepared_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the physical endpoint completed prepare before device death");
    assert!(probe.fail_output());
    release.wait();
    build.join().unwrap();
    let shutdown = service.shutdown();
    assert!(shutdown.clean);
    assert_eq!(shutdown.failure, Some(MixerOutputFailure::BackendFailure));
}

#[test]
fn implicit_non_48khz_default_fallback_preserves_negotiated_format_and_clock_cadence() {
    const RATE: u32 = 44_100;
    const RATE_F32: f32 = 44_100.0;
    let (service, driver_probe) = start_test_mixer(
        RATE,
        TestDriverConfig {
            actual_sample_rate: RATE,
            ..TestDriverConfig::default()
        },
    )
    .unwrap();
    let session = service.add_session(AudioSessionId(222)).unwrap();
    let probe = RenderProbe(Some(driver_probe));
    let (prepared, prepared_rx) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let (cleanup_thread, _cleanup_thread_rx) = mpsc::sync_channel(1);
    let output: Arc<dyn AudioOutputFactory> = Arc::new(BlockAfterPrepareFactory {
        inner: Arc::new(SessionAudioOutputFactory::new(session.script_bus())),
        prepared,
        release: Arc::clone(&release),
        cleanup_thread,
    });

    let build = thread::spawn(move || {
        let context = AudioContext::builder(output)
            .number_of_channels(CHANNELS)
            .build()
            .expect("implicit-rate default construction retains exact negotiated emulation");
        assert!((context.sample_rate() - RATE_F32).abs() < f32::EPSILON);
        let quantum = f64::from(u32::try_from(FRAMES).unwrap()) / f64::from(RATE);
        assert!((context.output_latency() - quantum).abs() < f64::EPSILON);
        let deadline = Instant::now() + Duration::from_secs(2);
        let advanced = loop {
            let current = context.current_time();
            if current > 0.0 {
                break current;
            }
            assert!(Instant::now() < deadline, "emulated clock did not advance");
            thread::yield_now();
        };
        let rendered_quanta = advanced / quantum;
        assert!(
            (rendered_quanta - rendered_quanta.round()).abs() < 1.0e-9,
            "currentTime {advanced} did not advance at the negotiated 44.1 kHz cadence"
        );
        let AudioContextShutdownOutcome::Confirmed(report) =
            context.request_close().unwrap().wait()
        else {
            panic!("exact-config fallback cleanup was not confirmed")
        };
        assert_eq!(report.endpoint_death(), None);
    });

    prepared_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("44.1 kHz physical prepare completed");
    assert!(probe.fail_output());
    release.wait();
    build.join().unwrap();
    assert!(service.shutdown().clean);
}

#[test]
fn device_death_after_live_open_snapshot_never_publishes_the_pending_callback() {
    let (service, session, probe) = service(221);
    let script = session.script_bus();
    let pause = probe.pause_next_input_open_after_snapshot();
    let (failure_entered, failure_entered_rx) = mpsc::sync_channel(1);
    let failure_release = Arc::new(Barrier::new(2));
    let output: Arc<dyn AudioOutputFactory> =
        Arc::new(SessionAudioOutputFactory::with_failure_hook(
            script.clone(),
            Arc::new(TestFailureHook {
                entered: failure_entered,
                release: Arc::clone(&failure_release),
            }),
        ));
    let build = thread::spawn(move || {
        let context = AudioContext::builder(output)
            .options(AudioContextOptions {
                sample_rate: Some(TEST_RATE_F32),
                ..AudioContextOptions::default()
            })
            .number_of_channels(CHANNELS)
            .build()
            .expect("the start-snapshot race reroutes to emulated output");
        assert_eq!(context.state(), AudioContextState::Running);
        let started_at = context.current_time();
        let deadline = Instant::now() + Duration::from_secs(2);
        while context.current_time() <= started_at {
            assert!(Instant::now() < deadline, "emulated clock did not advance");
            thread::yield_now();
        }
        let AudioContextShutdownOutcome::Confirmed(report) =
            context.request_close().unwrap().wait()
        else {
            panic!("race fallback cleanup was not confirmed")
        };
        assert_eq!(report.endpoint_death(), None);
    });

    pause.wait_until_paused();
    assert!(probe.fail_output());
    let deadline = Instant::now() + Duration::from_secs(2);
    while script.output_failure().is_none() {
        assert!(
            Instant::now() < deadline,
            "the injected output death was not retained"
        );
        thread::yield_now();
    }
    pause.release();
    failure_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("physical cleanup reached the disarmed pending observer");
    failure_release.wait();
    build.join().unwrap();
    assert!(service.shutdown().clean);
}

#[test]
fn device_death_after_final_start_snapshot_is_typed_and_emulated() {
    let (service, session, probe) = service(223);
    let script = session.script_bus();
    let pause = probe.pause_next_input_open_before_publish();
    let output: Arc<dyn AudioOutputFactory> =
        Arc::new(SessionAudioOutputFactory::new(script.clone()));
    let build = thread::spawn(move || {
        let context = AudioContext::builder(output)
            .options(AudioContextOptions {
                sample_rate: Some(TEST_RATE_F32),
                ..AudioContextOptions::default()
            })
            .number_of_channels(CHANNELS)
            .build()
            .expect("the final start window produces emulated default output");
        assert_eq!(context.state(), AudioContextState::Running);
        let started_at = context.current_time();
        let deadline = Instant::now() + Duration::from_secs(2);
        while context.current_time() <= started_at {
            assert!(Instant::now() < deadline, "emulated clock did not advance");
            thread::yield_now();
        }
        let AudioContextShutdownOutcome::Confirmed(report) =
            context.request_close().unwrap().wait()
        else {
            panic!("final-window fallback cleanup was not confirmed")
        };
        assert_eq!(report.endpoint_death(), None);
    });

    pause.wait_until_paused();
    assert!(probe.fail_output());
    let deadline = Instant::now() + Duration::from_secs(2);
    while script.output_failure().is_none() {
        assert!(
            Instant::now() < deadline,
            "the final-window output failure was not retained"
        );
        thread::yield_now();
    }
    pause.release();
    build.join().unwrap();
    assert!(service.shutdown().clean);
}

#[test]
fn clean_stopped_owner_uses_emulated_default_without_a_retained_device_cause() {
    let (service, session, _probe) = service(220);
    let factory = SessionAudioOutputFactory::new(session.script_bus());
    assert!(service.shutdown().clean);

    let context = composite_context(&factory, "", 44_100.0, CHANNELS)
        .expect("a clean stopped mixer owner is equivalent to no physical device");
    assert_eq!(context.sink_id(), "");
    assert!((context.sample_rate() - 44_100.0).abs() < f32::EPSILON);
    assert_eq!(context.state(), AudioContextState::Running);
    let AudioContextShutdownOutcome::Confirmed(report) = context.request_close().unwrap().wait()
    else {
        panic!("emulated context cleanup was not confirmed")
    };
    assert_eq!(report.endpoint_death(), None);
}

#[test]
fn validation_precedes_capacity_and_prepared_abort_or_drop_restores_it() {
    let (service, session, probe) = service(101);
    let bus = session.script_bus();
    let factory = ScriptBusAudioOutputFactory::new(bus.clone());

    assert!(
        factory
            .prepare_parts("none", Some(TEST_RATE_F32), CHANNELS)
            .is_err()
    );
    assert!(factory.prepare_parts("", Some(44_100.0), CHANNELS).is_err());
    assert!(factory.prepare_parts("", Some(TEST_RATE_F32), 1).is_err());

    let reservations = reserve_all(&bus);
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));
    retire_reservations(reservations);

    let prepared = factory
        .prepare_parts("", Some(TEST_RATE_F32), CHANNELS)
        .unwrap();
    assert!((prepared.config.format().sample_rate() - TEST_RATE_F32).abs() < f32::EPSILON);
    assert_eq!(prepared.config.format().number_of_channels(), CHANNELS);
    assert_eq!(prepared.config.format().max_frames_per_callback(), FRAMES);
    assert_eq!(prepared.config.accepted_sink_id(), "");
    assert_samples(&probe.render(), 0.0);
    assert!(block_on(prepared.abort()).is_ok());

    let prepared = factory
        .prepare_parts("", None, CHANNELS)
        .expect("default rate is accepted");
    drop(prepared);
    let deadline = Instant::now() + Duration::from_secs(2);
    let replacement = loop {
        match bus.try_reserve_input() {
            Ok(reservation) => break reservation,
            Err(MixerControlError::InputCapacity) => {
                assert!(Instant::now() < deadline, "dropped prepare did not retire");
                thread::yield_now();
            }
            Err(error) => panic!("unexpected replacement error: {error:?}"),
        }
    };
    assert!(block_on(replacement.abort()).unwrap().is_clean());
    assert!(service.shutdown().clean);
}

#[test]
#[allow(clippy::too_many_lines)] // one callback lifecycle keeps all fail-closed phases ordered
fn render_copy_stop_panic_and_invalid_geometry_fail_closed_without_allocating() {
    let mut output = [MixerFrame::ZERO; FRAMES];
    let mut scratch = [0.0; INTERLEAVED_SAMPLES];
    let deaths = AtomicUsize::new(0);

    let status = assert_no_alloc::assert_no_alloc(|| {
        render_fixed(
            &mut output,
            &mut scratch,
            1.0,
            |samples| {
                for frame in samples.chunks_exact_mut(2) {
                    frame[0] = 0.25;
                    frame[1] = -0.5;
                }
                AudioRenderStatus::Continue
            },
            |_| {
                deaths.fetch_add(1, Ordering::Relaxed);
            },
        )
    });
    assert_eq!(status, MixerInputStatus::Active);
    assert!(output.iter().all(|frame| {
        (frame.left() - 0.25).abs() < f32::EPSILON && (frame.right() + 0.5).abs() < f32::EPSILON
    }));

    let status = assert_no_alloc::assert_no_alloc(|| {
        render_fixed(
            &mut output,
            &mut scratch,
            1.0,
            |samples| {
                samples.fill(0.125);
                AudioRenderStatus::Continue
            },
            |_| {},
        )
    });
    assert_eq!(status, MixerInputStatus::Active);
    assert!(output.iter().all(|frame| {
        (frame.left() - 0.125).abs() < f32::EPSILON && (frame.right() - 0.125).abs() < f32::EPSILON
    }));

    let muted_callbacks = AtomicUsize::new(0);
    let status = assert_no_alloc::assert_no_alloc(|| {
        render_fixed(
            &mut output,
            &mut scratch,
            0.0,
            |samples| {
                muted_callbacks.fetch_add(1, Ordering::Relaxed);
                samples.fill(f32::NAN);
                AudioRenderStatus::Continue
            },
            |_| {},
        )
    });
    assert_eq!(status, MixerInputStatus::Active);
    assert_eq!(muted_callbacks.load(Ordering::Relaxed), 1);
    assert!(output.iter().all(|frame| *frame == MixerFrame::ZERO));

    output.fill(MixerFrame::from_mono(1.0));
    let status = assert_no_alloc::assert_no_alloc(|| {
        render_fixed(
            &mut output,
            &mut scratch,
            1.0,
            |samples| {
                samples.fill(1.0);
                AudioRenderStatus::Stop
            },
            |_| {},
        )
    });
    assert_eq!(status, MixerInputStatus::Finished);
    assert!(output.iter().all(|frame| *frame == MixerFrame::ZERO));

    output.fill(MixerFrame::from_mono(1.0));
    let status = assert_no_alloc::assert_no_alloc(|| {
        render_fixed(
            &mut output,
            &mut scratch,
            1.0,
            |_| panic_any(HostilePayload),
            |_| {
                deaths.fetch_add(1, Ordering::Relaxed);
            },
        )
    });
    assert_eq!(status, MixerInputStatus::Finished);
    assert!(output.iter().all(|frame| *frame == MixerFrame::ZERO));

    let mut oversized = [MixerFrame::from_mono(1.0); FRAMES + 1];
    assert_eq!(
        render_fixed(
            &mut oversized,
            &mut scratch,
            1.0,
            |_| AudioRenderStatus::Continue,
            |_| {
                deaths.fetch_add(1, Ordering::Relaxed);
            },
        ),
        MixerInputStatus::Finished
    );
    assert!(oversized.iter().all(|frame| *frame == MixerFrame::ZERO));
    assert_eq!(
        render_fixed(
            &mut [],
            &mut scratch,
            1.0,
            |_| AudioRenderStatus::Continue,
            |_| {
                deaths.fetch_add(1, Ordering::Relaxed);
            },
        ),
        MixerInputStatus::Finished
    );
    assert_eq!(deaths.load(Ordering::Relaxed), 3);
}

#[test]
fn valid_short_quantum_is_fully_copied() {
    let mut short = [MixerFrame::ZERO; 64];
    let mut scratch = [0.0; INTERLEAVED_SAMPLES];
    assert_eq!(
        render_fixed(
            &mut short,
            &mut scratch,
            1.0,
            |samples| {
                samples.fill(0.75);
                AudioRenderStatus::Continue
            },
            |_| {},
        ),
        MixerInputStatus::Active
    );
    assert!(short.iter().all(|frame| {
        (frame.left() - 0.75).abs() < f32::EPSILON && (frame.right() - 0.75).abs() < f32::EPSILON
    }));
}

struct ConstantInput(f32);

impl MixerInput for ConstantInput {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        output.fill(MixerFrame::from_mono(self.0));
        MixerInputStatus::Active
    }
}

fn hosted_context(factory: &ScriptBusAudioOutputFactory) -> AudioContext {
    let output: Arc<dyn AudioOutputFactory> = Arc::new(factory.clone());
    AudioContext::builder(output)
        .options(AudioContextOptions {
            sample_rate: Some(TEST_RATE_F32),
            ..AudioContextOptions::default()
        })
        .number_of_channels(CHANNELS)
        .build()
        .expect("hosted context starts on the script mixer")
}

#[derive(Clone)]
struct BlockAfterPrepareFactory {
    inner: Arc<dyn AudioOutputFactory>,
    prepared: mpsc::SyncSender<()>,
    release: Arc<Barrier>,
    cleanup_thread: mpsc::SyncSender<thread::ThreadId>,
}

impl AudioOutputFactory for BlockAfterPrepareFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let prepared = self.inner.prepare(request)?;
        self.prepared
            .send(())
            .expect("test observes the production prepared endpoint");
        self.release.wait();
        Ok(Box::new(ObserveStartCleanup {
            inner: Some(prepared),
            cleanup_thread: self.cleanup_thread.clone(),
        }))
    }
}

struct ObserveStartCleanup {
    inner: Option<Box<dyn PreparedAudioOutput>>,
    cleanup_thread: mpsc::SyncSender<thread::ThreadId>,
}

#[derive(Clone)]
struct DropShutdownObserverFactory {
    inner: ScriptBusAudioOutputFactory,
}

impl AudioOutputFactory for DropShutdownObserverFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        self.inner.prepare(request).map(|inner| {
            Box::new(DropShutdownObserverPrepared { inner: Some(inner) })
                as Box<dyn PreparedAudioOutput>
        })
    }
}

struct DropShutdownObserverPrepared {
    inner: Option<Box<dyn PreparedAudioOutput>>,
}

impl PreparedAudioOutput for DropShutdownObserverPrepared {
    fn config(&self) -> &AudioOutputConfig {
        self.inner
            .as_ref()
            .expect("test wrapper is single-use")
            .config()
    }

    fn start(
        mut self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        self.inner
            .take()
            .expect("test wrapper is single-use")
            .start(callback, events)
            .map(|inner| {
                Box::new(DropShutdownObserverRunning { inner: Some(inner) })
                    as Box<dyn RunningAudioOutput>
            })
    }

    fn abort(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        self.inner
            .take()
            .expect("test wrapper is single-use")
            .abort()
    }
}

struct DropShutdownObserverRunning {
    inner: Option<Box<dyn RunningAudioOutput>>,
}

impl RunningAudioOutput for DropShutdownObserverRunning {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        self.inner
            .as_mut()
            .expect("test wrapper is single-use")
            .resume()
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        self.inner
            .as_mut()
            .expect("test wrapper is single-use")
            .suspend()
    }

    fn shutdown(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        let observer = self
            .inner
            .take()
            .expect("test wrapper is single-use")
            .shutdown();
        drop(observer);
        AudioOutputEndpointShutdown::ready(Err(output_error(
            AudioOutputErrorKind::Shutdown,
            "test deliberately dropped the production shutdown observer",
        )))
    }
}

impl PreparedAudioOutput for ObserveStartCleanup {
    fn config(&self) -> &AudioOutputConfig {
        self.inner
            .as_ref()
            .expect("test wrapper is single-use")
            .config()
    }

    fn start(
        mut self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        let prepared = self.inner.take().expect("test wrapper is single-use");
        match prepared.start(callback, events) {
            Ok(running) => Ok(running),
            Err(failure) => {
                let (error, shutdown) = failure.into_parts();
                let cleanup_thread = self.cleanup_thread.clone();
                Err(AudioOutputStartFailure::new(
                    error,
                    AudioOutputEndpointShutdown::from_future(async move {
                        let result = shutdown.await;
                        cleanup_thread
                            .send(thread::current().id())
                            .expect("test observes callback cleanup completion");
                        result
                    }),
                ))
            }
        }
    }

    fn abort(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        self.inner
            .take()
            .expect("test wrapper is single-use")
            .abort()
    }
}

fn attach_constant(
    context: &AudioContext,
    value: f32,
    gain_value: f32,
) -> (
    web_audio_api::node::ConstantSourceNode,
    web_audio_api::node::GainNode,
) {
    let mut source = context.create_constant_source();
    source.offset().set_value(value);
    let gain = context.create_gain();
    gain.gain().set_value(gain_value);
    source.connect(&gain);
    gain.connect(&context.destination());
    source.start();
    (source, gain)
}

#[test]
fn two_hosted_contexts_and_native_sibling_mix_with_independent_lifecycle() {
    let (service, session, probe) = service(102);
    let script = session.script_bus();
    let native = session.native_bus();
    let factory = ScriptBusAudioOutputFactory::new(script.clone());
    assert_eq!(probe.physical_start_count(), 1);
    assert_eq!(probe.physical_play_count(), 1);

    script.set_gain(0.5).unwrap();
    native.set_gain(0.25).unwrap();
    let native = native
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(ConstantInput(0.125)))
        .unwrap();
    let first = hosted_context(&factory);
    let second = hosted_context(&factory);
    let _first_graph = attach_constant(&first, 0.25, 0.5);
    let _second_graph = attach_constant(&second, 0.5, 0.5);

    let _ = assert_no_alloc::assert_no_alloc(|| probe.render());
    let _ = assert_no_alloc::assert_no_alloc(|| probe.render());
    // Contexts contribute distinct 0.125 and 0.25 values before the Script
    // bus gain; the native sibling contributes 0.125 before its own bus gain.
    render_until(&probe, 0.21875);
    control_while_rendering(&probe, &first, AudioContext::suspend_sync);
    render_until(&probe, 0.15625);
    control_while_rendering(&probe, &second, AudioContext::close_sync);
    render_until(&probe, 0.03125);
    control_while_rendering(&probe, &first, AudioContext::resume_sync);
    render_until(&probe, 0.09375);
    control_while_rendering(&probe, &first, AudioContext::close_sync);
    render_until(&probe, 0.03125);

    assert!(block_on(native.shutdown()).unwrap().is_clean());
    render_until(&probe, 0.0);
    assert!(service.shutdown().clean);
}

#[test]
fn capacity_rejection_is_unchanged_and_close_allows_immediate_replacement() {
    let (service, session, probe) = service(103);
    let bus = session.script_bus();
    let factory = ScriptBusAudioOutputFactory::new(bus.clone());
    let mut held: Vec<_> = (0..INPUT_CAPACITY - 1)
        .map(|_| bus.try_reserve_input().unwrap())
        .collect();
    let context = hosted_context(&factory);
    let _original_graph = attach_constant(&context, 0.125, 1.0);

    let output: Arc<dyn AudioOutputFactory> = Arc::new(factory.clone());
    assert!(
        AudioContext::builder(output)
            .options(AudioContextOptions {
                sample_rate: Some(TEST_RATE_F32),
                ..AudioContextOptions::default()
            })
            .build()
            .is_err()
    );
    let freed = held.pop().expect("one held reservation is available");
    assert!(block_on(freed.abort()).unwrap().is_clean());
    let replacement = hosted_context(&factory);
    let _replacement_graph = attach_constant(&replacement, 0.25, 1.0);
    render_until(&probe, 0.375);

    retire_reservations(held);
    control_while_rendering(&probe, &replacement, AudioContext::close_sync);
    render_until(&probe, 0.125);
    control_while_rendering(&probe, &context, AudioContext::close_sync);
    render_until(&probe, 0.0);
    assert!(service.shutdown().clean);
}

#[test]
fn production_start_rejects_stopped_owner_and_cleans_the_real_callback() {
    let (service, session, _probe) = service(108);
    let (prepared, prepared_rx) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let (cleanup_thread, cleanup_thread_rx) = mpsc::sync_channel(1);
    let output: Arc<dyn AudioOutputFactory> = Arc::new(BlockAfterPrepareFactory {
        inner: Arc::new(ScriptBusAudioOutputFactory::new(session.script_bus())),
        prepared,
        release: release.clone(),
        cleanup_thread,
    });

    let build = thread::spawn(move || {
        let build_thread = thread::current().id();
        let error = match AudioContext::builder(output)
            .options(AudioContextOptions {
                sample_rate: Some(TEST_RATE_F32),
                ..AudioContextOptions::default()
            })
            .build()
        {
            Ok(context) => {
                context.close_sync();
                panic!("stopped owner unexpectedly accepted a hosted context")
            }
            Err(error) => error,
        };
        let output_kind = error.output_error().map(AudioOutputError::kind);
        let cleanup = error
            .cleanup_receipt()
            .expect("start rejection owns callback cleanup")
            .wait();
        (build_thread, error.kind(), output_kind, cleanup)
    });

    prepared_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("production prepare reached its handoff boundary");
    assert!(service.shutdown().clean);
    release.wait();
    let (build_thread, kind, output_kind, cleanup) = build.join().unwrap();
    let callback_cleanup_thread = cleanup_thread_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("production rejected callback cleanup completed");
    assert_ne!(callback_cleanup_thread, thread::current().id());
    assert_ne!(callback_cleanup_thread, build_thread);
    assert_eq!(
        kind,
        web_audio_api::context::AudioContextBuildErrorKind::StartFailed
    );
    assert_eq!(output_kind, Some(AudioOutputErrorKind::DeviceUnavailable));
    let AudioContextShutdownOutcome::Confirmed(report) = cleanup else {
        panic!("stopped-owner callback cleanup was not confirmed: {cleanup:?}")
    };
    assert_eq!(
        report.endpoint_death(),
        Some(AudioOutputDeathReason::FactoryShutdown)
    );
}

#[test]
fn production_start_panic_retains_callback_until_unconfirmed_cleanup() {
    let (service, session, _probe) = service(112);
    let bus = session.script_bus();
    let held: Vec<_> = (0..INPUT_CAPACITY - 1)
        .map(|_| bus.try_reserve_input().unwrap())
        .collect();
    let (entered, _entered_rx) = mpsc::sync_channel(1);
    let (dropped, dropped_rx) = mpsc::sync_channel(1);
    let (shutdown, _shutdown_rx) = mpsc::sync_channel(1);
    let hook = Arc::new(TestRenderHook {
        entered,
        release: Arc::new(Barrier::new(1)),
        dropped,
        shutdown,
        block_once: AtomicBool::new(false),
        panic_on_drop: false,
    });
    let factory = ScriptBusAudioOutputFactory::with_start_panic(bus.clone(), hook);
    let output: Arc<dyn AudioOutputFactory> = Arc::new(factory);
    let error = match AudioContext::builder(output)
        .options(AudioContextOptions {
            sample_rate: Some(TEST_RATE_F32),
            ..AudioContextOptions::default()
        })
        .build()
    {
        Ok(context) => {
            context.close_sync();
            panic!("injected production start panic unexpectedly built a context")
        }
        Err(error) => error,
    };
    assert_eq!(
        error.kind(),
        web_audio_api::context::AudioContextBuildErrorKind::StartFailed
    );
    assert_eq!(
        error.output_error().map(AudioOutputError::kind),
        Some(AudioOutputErrorKind::BackendSpecific)
    );
    let cleanup = error
        .cleanup_receipt()
        .expect("contained start panic retains cleanup ownership")
        .wait();
    assert!(matches!(
        cleanup,
        AudioContextShutdownOutcome::Unconfirmed { .. }
    ));
    let callback_drop_thread = dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("panic cleanup eventually destroyed the retained callback shell");
    assert_ne!(callback_drop_thread, thread::current().id());

    let replacement = bus
        .try_reserve_input()
        .expect("panicked prepublication start restored exact capacity");
    assert!(block_on(replacement.abort()).unwrap().is_clean());
    retire_reservations(held);
    assert!(service.shutdown().clean);
}

#[test]
fn real_callback_stays_owned_during_active_render_and_drops_off_render() {
    let (service, session, probe) = service(109);
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let (dropped, dropped_rx) = mpsc::sync_channel(1);
    let (shutdown, shutdown_rx) = mpsc::sync_channel(1);
    let hook = Arc::new(TestRenderHook {
        entered,
        release: release.clone(),
        dropped,
        shutdown,
        block_once: AtomicBool::new(true),
        panic_on_drop: false,
    });
    let factory = ScriptBusAudioOutputFactory::with_render_hook(session.script_bus(), hook);
    let context = hosted_context(&factory);
    let _graph = attach_constant(&context, 0.25, 1.0);

    let render_probe = probe.clone();
    let render = thread::spawn(move || render_probe.render());
    let render_thread = entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("real adapter entered the callback bridge");

    thread::scope(|scope| {
        let close = scope.spawn(|| {
            context
                .request_close()
                .expect("close request is admitted")
                .wait()
        });
        let shutdown_while_active = shutdown_rx.recv_timeout(Duration::from_millis(250)).is_ok();
        assert!(matches!(
            dropped_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(!close.is_finished());
        assert!(
            !shutdown_while_active,
            "hosted lifecycle must quiesce its graph callback before endpoint shutdown"
        );

        release.wait();
        render.join().unwrap();
        let mut shutdown_committed = false;
        while !close.is_finished() {
            shutdown_committed |= shutdown_rx.try_recv().is_ok();
            let _ = probe.render();
            thread::yield_now();
        }
        shutdown_committed |= shutdown_rx.try_recv().is_ok();
        let outcome = close.join().unwrap();
        assert!(
            shutdown_committed,
            "hosted lifecycle never committed production adapter shutdown"
        );
        assert!(matches!(outcome, AudioContextShutdownOutcome::Confirmed(_)));
    });

    let callback_drop_thread = dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("real callback shell was destroyed");
    assert_ne!(callback_drop_thread, render_thread);
    assert_ne!(callback_drop_thread, thread::current().id());
    assert_eq!(context.state(), AudioContextState::Closed);
    assert!(service.shutdown().clean);
}

#[test]
fn live_mixer_shutdown_reports_the_callback_drop_classification() {
    let (service, session, probe) = service(110);
    let factory = ScriptBusAudioOutputFactory::new(session.script_bus());
    let context = hosted_context(&factory);
    let _graph = attach_constant(&context, 0.25, 1.0);
    render_until(&probe, 0.25);
    assert_eq!(context.state(), AudioContextState::Running);

    assert!(service.shutdown().clean);
    let outcome = context
        .request_close()
        .expect("factory death retains the shutdown receipt")
        .wait();
    let AudioContextShutdownOutcome::Confirmed(report) = outcome else {
        panic!("factory death did not confirm callback cleanup: {outcome:?}")
    };
    assert_eq!(
        report.endpoint_death(),
        Some(AudioOutputDeathReason::CallbackRetiredUnexpectedly)
    );
    assert_eq!(context.state(), AudioContextState::Closed);
}

#[test]
fn physical_output_death_seals_bus_and_settles_each_hosted_context_once() {
    let (service, session, probe) = service(903);
    let script = session.script_bus();
    let factory = ScriptBusAudioOutputFactory::new(script.clone());
    let first = hosted_context(&factory);
    let second = hosted_context(&factory);
    let _first_graph = attach_constant(&first, 0.125, 1.0);
    let _second_graph = attach_constant(&second, 0.25, 1.0);
    render_until(&probe, 0.375);

    assert!(probe.fail_output());
    let first_close = first
        .request_close()
        .expect("post-death immediate close retains endpoint cleanup proof");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match script.set_gain(0.75) {
            Err(MixerControlError::OwnerStopped) => break,
            Ok(()) | Err(MixerControlError::Saturated) => {
                assert!(
                    Instant::now() < deadline,
                    "device death did not seal bus control"
                );
                thread::yield_now();
            }
            Err(error) => panic!("unexpected post-death bus result: {error:?}"),
        }
    }
    assert!(service.shutdown().clean);

    let second_close = second
        .request_close()
        .expect("device death retains endpoint cleanup proof");
    for (context, close) in [(&first, first_close), (&second, second_close)] {
        let outcome = close.wait();
        let AudioContextShutdownOutcome::Confirmed(report) = outcome else {
            panic!("device-death cleanup was not confirmed: {outcome:?}");
        };
        assert_eq!(
            report.endpoint_death(),
            Some(AudioOutputDeathReason::BackendFailure)
        );
        assert_eq!(context.state(), AudioContextState::Closed);
    }
}

#[test]
fn dropped_production_shutdown_observer_does_not_cancel_slot_reuse() {
    let (service, session, probe) = service(111);
    let bus = session.script_bus();
    let mut held: Vec<_> = (0..INPUT_CAPACITY - 1)
        .map(|_| bus.try_reserve_input().unwrap())
        .collect();
    let (entered, _entered_rx) = mpsc::sync_channel(1);
    let (dropped, dropped_rx) = mpsc::sync_channel(1);
    let (shutdown, shutdown_rx) = mpsc::sync_channel(1);
    let hook = Arc::new(TestRenderHook {
        entered,
        release: Arc::new(Barrier::new(1)),
        dropped,
        shutdown,
        block_once: AtomicBool::new(false),
        panic_on_drop: false,
    });
    let factory = ScriptBusAudioOutputFactory::with_render_hook(bus.clone(), hook);
    let output: Arc<dyn AudioOutputFactory> =
        Arc::new(DropShutdownObserverFactory { inner: factory });
    let context = AudioContext::builder(output)
        .options(AudioContextOptions {
            sample_rate: Some(TEST_RATE_F32),
            ..AudioContextOptions::default()
        })
        .build()
        .expect("wrapped production adapter starts");
    let graph = attach_constant(&context, 0.25, 1.0);
    render_until(&probe, 0.25);
    drop(graph);
    drop(context);

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut shutdown_seen = false;
    let replacement = loop {
        let _ = probe.render();
        shutdown_seen |= shutdown_rx.try_recv().is_ok();
        match bus.try_reserve_input() {
            Ok(reservation) => break reservation,
            Err(MixerControlError::InputCapacity | MixerControlError::Saturated) => {
                assert!(
                    Instant::now() < deadline,
                    "dropped observer cancelled accepted slot cleanup"
                );
                thread::yield_now();
            }
            Err(error) => panic!("unexpected replacement failure: {error:?}"),
        }
    };
    shutdown_seen |= shutdown_rx.try_recv().is_ok();
    assert!(shutdown_seen, "production shutdown was not committed");
    let callback_drop_thread = dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("callback shell cleanup remained independently owned");
    assert_ne!(callback_drop_thread, thread::current().id());

    assert!(block_on(replacement.abort()).unwrap().is_clean());
    retire_reservations(std::mem::take(&mut held));
    assert!(service.shutdown().clean);
}

struct DropProbe(Arc<AtomicUsize>);

impl MixerInput for DropProbe {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        output.fill(MixerFrame::ZERO);
        MixerInputStatus::Active
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

#[test]
fn rejected_start_returns_exact_cleanup_and_preboxed_publication_has_no_gap() {
    let (first_service, session, _probe) = service(104);
    let bus = session.script_bus();
    let reservation = bus.try_reserve_input().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    assert!(first_service.shutdown().clean);

    let failure = reservation
        .start_preboxed(Box::new(DropProbe(drops.clone())))
        .unwrap_err();
    let MixerInputStartFailure::Rejected(failure) = failure else {
        panic!("stopped owner must reject before installation")
    };
    let (error, reservation, source) = failure.into_parts();
    let failure = abort_start(control_error(error), reservation, source);
    let (_, shutdown) = failure.into_parts();
    assert!(block_on(shutdown).is_ok());
    assert_eq!(drops.load(Ordering::Acquire), 1);

    let (service, session, _probe) = service(105);
    let running = session
        .script_bus()
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(ConstantInput(0.0)))
        .unwrap();
    let owner = Box::new(ScriptBusRunningOutput {
        input: None,
        render_hook: None,
    });
    let owner = assert_no_alloc::assert_no_alloc(|| publish_running(owner, running));
    assert!(block_on(owner.shutdown()).is_ok());
    assert!(service.shutdown().clean);
}

struct BlockingInput {
    entered: mpsc::SyncSender<()>,
    release: Arc<Barrier>,
}

impl MixerInput for BlockingInput {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        self.entered.send(()).unwrap();
        self.release.wait();
        output.fill(MixerFrame::from_mono(0.25));
        MixerInputStatus::Active
    }
}

#[derive(Default)]
struct WakeProbe(AtomicBool);

impl Wake for WakeProbe {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn active_render_keeps_adapter_shutdown_pending_then_wakes() {
    let (service, session, probe) = service(106);
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let running = session
        .script_bus()
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(BlockingInput {
            entered,
            release: release.clone(),
        }))
        .unwrap();
    let render = thread::spawn(move || probe.render());
    entered_rx.recv().unwrap();

    let owner: Box<dyn RunningAudioOutput> = Box::new(ScriptBusRunningOutput {
        input: Some(running),
        render_hook: None,
    });
    let mut shutdown = Box::pin(owner.shutdown());
    let wake = Arc::new(WakeProbe::default());
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        Pin::new(&mut shutdown).poll(&mut cx),
        Poll::Pending
    ));

    release.wait();
    render.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !wake.0.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "adapter shutdown was not woken");
        thread::yield_now();
    }
    assert!(matches!(
        Pin::new(&mut shutdown).poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    assert!(service.shutdown().clean);
}

struct PanickingDestructor;

impl MixerInput for PanickingDestructor {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        output.fill(MixerFrame::ZERO);
        MixerInputStatus::Active
    }
}

impl Drop for PanickingDestructor {
    fn drop(&mut self) {
        panic!("hostile callback destructor");
    }
}

#[test]
fn hostile_callback_destructor_is_an_honest_shutdown_error() {
    let (service, session, _probe) = service(107);
    let running = session
        .script_bus()
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(PanickingDestructor))
        .unwrap();
    let owner: Box<dyn RunningAudioOutput> = Box::new(ScriptBusRunningOutput {
        input: Some(running),
        render_hook: None,
    });
    let error = block_on(owner.shutdown()).unwrap_err();
    assert_eq!(error.kind(), AudioOutputErrorKind::Shutdown);
    assert!(!service.shutdown().clean);
}

#[test]
fn real_callback_shell_destructor_panic_is_unconfirmed_and_quarantined() {
    let (service, session, probe) = service(113);
    let bus = session.script_bus();
    let held: Vec<_> = (0..INPUT_CAPACITY - 1)
        .map(|_| bus.try_reserve_input().unwrap())
        .collect();
    let (entered, _entered_rx) = mpsc::sync_channel(1);
    let (dropped, dropped_rx) = mpsc::sync_channel(1);
    let (shutdown, shutdown_rx) = mpsc::sync_channel(1);
    let hook = Arc::new(TestRenderHook {
        entered,
        release: Arc::new(Barrier::new(1)),
        dropped,
        shutdown,
        block_once: AtomicBool::new(false),
        panic_on_drop: true,
    });
    let factory = ScriptBusAudioOutputFactory::with_render_hook(bus.clone(), hook);
    let context = hosted_context(&factory);
    let _graph = attach_constant(&context, 0.25, 1.0);
    render_until(&probe, 0.25);

    let receipt = context.request_close().expect("close request is admitted");
    let outcome = thread::scope(|scope| {
        let close = scope.spawn(|| receipt.wait());
        while !close.is_finished() {
            let _ = probe.render();
            thread::yield_now();
        }
        close.join().unwrap()
    });
    assert!(shutdown_rx.recv_timeout(Duration::from_secs(2)).is_ok());
    assert!(matches!(
        outcome,
        AudioContextShutdownOutcome::Unconfirmed { .. }
    ));
    let callback_drop_thread = dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("hostile callback shell destructor ran on the cleanup worker");
    assert_ne!(callback_drop_thread, thread::current().id());
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));

    retire_reservations(held);
    assert!(!service.shutdown().clean);
}
