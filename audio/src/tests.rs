use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

use futures::executor::block_on;
use kira::backend::Renderer;

use super::*;

const TEST_RATE: u32 = 48_000;

#[test]
fn mixer_frame_owns_stereo_samples_and_maps_to_the_backend() {
    let frames = [MixerFrame::new(0.25, -0.5), MixerFrame::from_mono(0.75)];
    assert!((frames[0].left() - 0.25).abs() < f32::EPSILON);
    assert!((frames[0].right() + 0.5).abs() < f32::EPSILON);
    assert_eq!(MixerFrame::ZERO, MixerFrame::new(0.0, 0.0));

    let mut backend = [KiraFrame::ZERO; 2];
    copy_mixer_frames(&mut backend, &frames);
    assert_eq!(backend[0], KiraFrame::new(0.25, -0.5));
    assert_eq!(backend[1], KiraFrame::from_mono(0.75));
}

#[derive(Clone, Default)]
struct RenderProbe {
    renderer: Arc<Mutex<Option<Renderer>>>,
}

impl RenderProbe {
    fn render(&self, output: &mut [f32]) {
        let mut renderer = self.renderer.lock().expect("renderer lock poisoned");
        let renderer = renderer.as_mut().expect("renderer is not live");
        renderer.on_start_processing();
        renderer.process(output, 2);
    }

    fn is_live(&self) -> bool {
        self.renderer
            .lock()
            .expect("renderer lock poisoned")
            .is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestBackendError {
    Setup,
    Start,
}

#[derive(Clone)]
struct TestBackendSettings {
    probe: RenderProbe,
    actual_rate: u32,
    fail_setup: bool,
    fail_start: bool,
    backend_drops: Arc<AtomicUsize>,
}

struct TestBackend {
    probe: RenderProbe,
    fail_start: bool,
    backend_drops: Arc<AtomicUsize>,
}

impl Backend for TestBackend {
    type Settings = TestBackendSettings;
    type Error = TestBackendError;

    fn setup(
        settings: Self::Settings,
        _internal_buffer_size: usize,
    ) -> Result<(Self, u32), Self::Error> {
        if settings.fail_setup {
            return Err(TestBackendError::Setup);
        }
        Ok((
            Self {
                probe: settings.probe,
                fail_start: settings.fail_start,
                backend_drops: settings.backend_drops,
            },
            settings.actual_rate,
        ))
    }

    fn start(&mut self, renderer: Renderer) -> Result<(), Self::Error> {
        if self.fail_start {
            return Err(TestBackendError::Start);
        }
        let replaced = self
            .probe
            .renderer
            .lock()
            .expect("renderer lock poisoned")
            .replace(renderer);
        assert!(replaced.is_none());
        Ok(())
    }
}

impl Drop for TestBackend {
    fn drop(&mut self) {
        self.probe
            .renderer
            .lock()
            .expect("renderer lock poisoned")
            .take();
        self.backend_drops.fetch_add(1, Ordering::Relaxed);
    }
}

fn settings(probe: RenderProbe) -> TestBackendSettings {
    TestBackendSettings {
        probe,
        actual_rate: TEST_RATE,
        fail_setup: false,
        fail_start: false,
        backend_drops: Arc::new(AtomicUsize::new(0)),
    }
}

struct ConstantInput {
    frame: MixerFrame,
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl MixerInput for ConstantInput {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        self.calls.fetch_add(1, Ordering::Relaxed);
        output.fill(self.frame);
        MixerInputStatus::Active
    }
}

impl Drop for ConstantInput {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

fn constant(value: f32) -> (Box<dyn MixerInput>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    (
        Box::new(ConstantInput {
            frame: MixerFrame::from_mono(value),
            calls: calls.clone(),
            drops: drops.clone(),
        }),
        calls,
        drops,
    )
}

fn start_reserved(
    reservation: MixerInputReservation,
    source: Box<dyn MixerInput>,
) -> RunningMixerInput {
    reservation
        .start_preboxed(source)
        .expect("admitted start must succeed")
}

fn assert_samples(output: &[f32], expected: f32) {
    for &sample in output {
        assert!(
            (sample - expected).abs() < 1.0e-6,
            "expected {expected}, got {sample}"
        );
    }
}

fn eventually(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::yield_now();
    }
}

fn test_control(
    commands: SyncSender<OwnerCommand>,
    retirements: SyncSender<RetirementRequest>,
) -> Arc<ControlInner> {
    Arc::new(ControlInner {
        gate: Mutex::new(GateState {
            sealed: false,
            accepting_retirements: true,
            start_admissions: 0,
        }),
        gate_drained: Condvar::new(),
        commands,
        retirements,
        format: MixerFormat {
            sample_rate: TEST_RATE,
        },
    })
}

#[test]
fn join_authority_is_not_send_but_scoped_bus_handles_are_send() {
    trait AmbiguousIfSend<Marker> {
        fn assert_not_send() {}
    }
    fn assert_send<T: Send>() {}

    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
    let _ = <MixerService<TestBackend> as AmbiguousIfSend<_>>::assert_not_send;
    assert_send::<MixerScriptBusHandle>();
}

#[test]
fn service_publishes_exact_format_and_joins_backend() {
    let probe = RenderProbe::default();
    let backend_drops = Arc::new(AtomicUsize::new(0));
    let service = MixerService::<TestBackend>::start(
        TestBackendSettings {
            backend_drops: backend_drops.clone(),
            ..settings(probe.clone())
        },
        TEST_RATE,
    )
    .unwrap();
    assert_eq!(service.format().sample_rate(), TEST_RATE);
    assert_eq!(service.format().number_of_channels(), 2);
    assert_eq!(
        service.format().max_frames_per_callback(),
        INTERNAL_BUFFER_FRAMES
    );
    assert!(probe.is_live());

    assert!(service.shutdown().clean);
    assert!(!probe.is_live());
    assert_eq!(backend_drops.load(Ordering::Relaxed), 1);
}

#[test]
fn permanent_slots_mix_two_script_inputs_and_native_with_independent_gains() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone()), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(7)).unwrap();
    let script = session.script_bus();
    let native = session.native_bus();

    let (first, _, _) = constant(0.125);
    let (second, _, _) = constant(0.25);
    let (tone, _, _) = constant(0.5);
    let first = start_reserved(script.try_reserve_input().unwrap(), first);
    let second = start_reserved(script.try_reserve_input().unwrap(), second);
    let tone = start_reserved(native.try_reserve_input().unwrap(), tone);

    script.set_gain(0.5).unwrap();
    native.set_gain(0.25).unwrap();
    let mut output = [0.0; INTERNAL_BUFFER_FRAMES * 2];
    // Kira has one already-rendered internal quantum when the owner accepts
    // the track commands; the following quantum observes the new bus gains.
    probe.render(&mut output);
    probe.render(&mut output);
    assert_samples(&output, 0.3125);

    assert!(first.suspend());
    probe.render(&mut output);
    assert_samples(&output, 0.25);
    assert!(first.resume());
    probe.render(&mut output);
    assert_samples(&output, 0.3125);

    assert!(block_on(second.shutdown()).unwrap().is_clean());
    probe.render(&mut output);
    assert_samples(&output, 0.1875);
    assert!(block_on(first.shutdown()).unwrap().is_clean());
    assert!(block_on(tone.shutdown()).unwrap().is_clean());
    assert!(service.shutdown().clean);
}

#[test]
fn reservation_is_real_prestart_silence_and_render_allocates_nothing() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone()), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(9)).unwrap();
    let reservation = session.script_bus().try_reserve_input().unwrap();

    let mut output = [1.0; INTERNAL_BUFFER_FRAMES * 2];
    assert_no_alloc::assert_no_alloc(|| probe.render(&mut output));
    assert_samples(&output, 0.0);

    let (source, calls, drops) = constant(0.375);
    let running = reservation.start_preboxed(source).unwrap();
    assert_no_alloc::assert_no_alloc(|| probe.render(&mut output));
    assert_samples(&output, 0.375);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_no_alloc::assert_no_alloc(|| probe.render(&mut output));
    assert_eq!(calls.load(Ordering::Relaxed), 2);

    assert!(block_on(running.shutdown()).unwrap().is_clean());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(service.shutdown().clean);
}

#[test]
fn exact_capacity_rejects_without_mutation_and_reuses_after_retirement() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start_with_limits(
        settings(probe),
        MixerFormat {
            sample_rate: TEST_RATE,
        },
        1,
        1,
    )
    .unwrap();
    let session = service.add_session(AudioSessionId(1)).unwrap();
    assert!(matches!(
        service.add_session(AudioSessionId(1)),
        Err(MixerControlError::DuplicateSession)
    ));
    assert!(matches!(
        service.add_session(AudioSessionId(2)),
        Err(MixerControlError::SessionCapacity)
    ));

    let reservation = session.script_bus().try_reserve_input().unwrap();
    assert!(matches!(
        session.script_bus().try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));
    assert!(block_on(reservation.abort()).unwrap().is_clean());

    let reused = session.script_bus().try_reserve_input().unwrap();
    assert!(block_on(reused.abort()).unwrap().is_clean());
    assert!(service.shutdown().clean);
}

#[test]
fn dropped_shutdown_observer_does_not_cancel_cleanup_or_reuse() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start_with_limits(
        settings(probe),
        MixerFormat {
            sample_rate: TEST_RATE,
        },
        1,
        1,
    )
    .unwrap();
    let session = service.add_session(AudioSessionId(2)).unwrap();
    let bus = session.script_bus();
    let (source, _, drops) = constant(0.25);
    let running = start_reserved(bus.try_reserve_input().unwrap(), source);
    drop(running.shutdown());

    eventually(|| drops.load(Ordering::Acquire) == 1);
    let reused = loop {
        match bus.try_reserve_input() {
            Ok(reservation) => break reservation,
            Err(MixerControlError::InputCapacity) => thread::yield_now(),
            Err(error) => panic!("unexpected reserve error: {error:?}"),
        }
    };
    assert!(block_on(reused.abort()).unwrap().is_clean());
    assert!(service.shutdown().clean);
}

#[test]
fn bounded_control_full_and_disconnect_are_typed_without_mutation() {
    let (full_commands, full_receiver) = mpsc::sync_channel(0);
    let (full_retirements, _full_retirement_receiver) = mpsc::sync_channel(1);
    let full_control = test_control(full_commands, full_retirements);
    let handle = MixerBusHandle {
        control: Arc::downgrade(&full_control),
        session: SessionKey {
            id: AudioSessionId(30),
            generation: 1,
        },
        bus: SessionBus::Script,
    };
    assert_eq!(handle.set_gain(0.5), Err(MixerControlError::Saturated));
    drop(full_receiver);
    assert_eq!(handle.set_gain(0.5), Err(MixerControlError::OwnerStopped));
}

#[test]
fn dropped_reserve_response_restores_the_same_physical_capacity() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start_with_limits(
        settings(probe),
        MixerFormat {
            sample_rate: TEST_RATE,
        },
        1,
        1,
    )
    .unwrap();
    let session = service.add_session(AudioSessionId(31)).unwrap();
    let control = service.control.as_ref().unwrap();
    let (response, abandoned) = mpsc::sync_channel(1);
    control
        .commands
        .send(OwnerCommand::Reserve(
            session.key,
            SessionBus::Script,
            response,
        ))
        .unwrap();
    drop(abandoned);

    let reservation = session.script_bus().try_reserve_input().unwrap();
    assert!(reservation.generation > 1);
    assert!(block_on(reservation.abort()).unwrap().is_clean());
    assert!(service.shutdown().clean);
}

#[test]
fn stale_generation_cannot_control_a_reused_slot() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start_with_limits(
        settings(probe),
        MixerFormat {
            sample_rate: TEST_RATE,
        },
        1,
        1,
    )
    .unwrap();
    let session = service.add_session(AudioSessionId(32)).unwrap();
    let bus = session.script_bus();
    let first = bus.try_reserve_input().unwrap();
    let stale_slot = Arc::clone(&first.slot);
    let stale_generation = first.generation;
    assert!(block_on(first.abort()).unwrap().is_clean());

    let second = bus.try_reserve_input().unwrap();
    assert_eq!(second.slot.address.index, stale_slot.address.index);
    assert_ne!(second.generation, stale_generation);
    assert!(!stale_slot.set_suspended(stale_generation, true));
    assert!(!stale_slot.close(stale_generation, false));
    assert!(block_on(second.abort()).unwrap().is_clean());
    assert!(service.shutdown().clean);
}

struct BlockingInput {
    entered: mpsc::SyncSender<thread::ThreadId>,
    release: Arc<Barrier>,
    drops: Arc<AtomicUsize>,
}

impl MixerInput for BlockingInput {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        self.entered.send(thread::current().id()).unwrap();
        self.release.wait();
        output.fill(MixerFrame::from_mono(0.25));
        MixerInputStatus::Active
    }
}

impl Drop for BlockingInput {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct WakeProbe {
    wakes: AtomicUsize,
}

impl Wake for WakeProbe {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::Release);
    }
}

struct PanicWake;

impl Wake for PanicWake {
    fn wake(self: Arc<Self>) {
        panic!("hostile waker")
    }

    fn wake_by_ref(self: &Arc<Self>) {
        panic!("hostile waker")
    }
}

#[test]
fn active_render_keeps_shutdown_pending_then_owner_wakes_and_retires_off_render() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone()), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(3)).unwrap();
    let (entered_sender, entered_receiver) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let drops = Arc::new(AtomicUsize::new(0));
    let running = start_reserved(
        session.script_bus().try_reserve_input().unwrap(),
        Box::new(BlockingInput {
            entered: entered_sender,
            release: release.clone(),
            drops: drops.clone(),
        }),
    );

    let render_probe = probe.clone();
    let render = thread::spawn(move || {
        let mut output = [0.0; INTERNAL_BUFFER_FRAMES * 2];
        render_probe.render(&mut output);
        output
    });
    let render_thread = entered_receiver.recv().unwrap();
    let mut shutdown = Box::pin(running.shutdown());
    let wake_probe = Arc::new(WakeProbe::default());
    let waker = Waker::from(wake_probe.clone());
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Pending
    ));
    assert_eq!(drops.load(Ordering::Relaxed), 0);

    release.wait();
    assert_samples(&render.join().unwrap(), 0.25);
    eventually(|| wake_probe.wakes.load(Ordering::Acquire) > 0);
    let retirement = match shutdown.as_mut().poll(&mut context) {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => panic!("owner wake did not publish retirement"),
    };
    assert!(retirement.is_clean());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_ne!(render_thread, thread::current().id());
    assert!(service.shutdown().clean);
}

#[test]
fn hostile_completion_waker_cannot_kill_the_owner() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone()), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(35)).unwrap();
    let (entered_sender, entered_receiver) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let drops = Arc::new(AtomicUsize::new(0));
    let running = start_reserved(
        session.script_bus().try_reserve_input().unwrap(),
        Box::new(BlockingInput {
            entered: entered_sender,
            release: release.clone(),
            drops: drops.clone(),
        }),
    );
    let render_probe = probe.clone();
    let render = thread::spawn(move || {
        let mut output = [0.0; INTERNAL_BUFFER_FRAMES * 2];
        render_probe.render(&mut output);
    });
    entered_receiver.recv().unwrap();

    let mut shutdown = Box::pin(running.shutdown());
    let panic_waker = Waker::from(Arc::new(PanicWake));
    let mut panic_context = Context::from_waker(&panic_waker);
    assert!(matches!(
        shutdown.as_mut().poll(&mut panic_context),
        Poll::Pending
    ));
    release.wait();
    render.join().unwrap();
    eventually(|| drops.load(Ordering::Acquire) == 1);

    assert!(block_on(shutdown).unwrap().is_clean());
    assert!(service.shutdown().clean);
}

#[test]
fn service_shutdown_waits_for_active_render_then_forced_cleanup() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone()), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(33)).unwrap();
    let (entered_sender, entered_receiver) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let drops = Arc::new(AtomicUsize::new(0));
    let running = start_reserved(
        session.script_bus().try_reserve_input().unwrap(),
        Box::new(BlockingInput {
            entered: entered_sender,
            release: release.clone(),
            drops: drops.clone(),
        }),
    );
    let render_probe = probe.clone();
    let render = thread::spawn(move || {
        let mut output = [0.0; INTERNAL_BUFFER_FRAMES * 2];
        render_probe.render(&mut output);
    });
    entered_receiver.recv().unwrap();

    let release_for_thread = release.clone();
    let drops_for_thread = drops.clone();
    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        assert_eq!(drops_for_thread.load(Ordering::Relaxed), 0);
        release_for_thread.wait();
    });
    let started = Instant::now();
    let result = service.shutdown();
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert!(result.clean);
    releaser.join().unwrap();
    render.join().unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(block_on(running.shutdown()).unwrap().is_clean());
}

struct ReentrantDestructor {
    bus: MixerScriptBusHandle,
    result: mpsc::SyncSender<Result<(), MixerControlError>>,
}

impl MixerInput for ReentrantDestructor {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        output.fill(MixerFrame::ZERO);
        MixerInputStatus::Active
    }
}

impl Drop for ReentrantDestructor {
    fn drop(&mut self) {
        let _ = self.result.send(self.bus.set_gain(0.5));
    }
}

#[test]
fn callback_destructor_can_reenter_control_without_deadlocking_owner() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(34)).unwrap();
    let bus = session.script_bus();
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    let running = start_reserved(
        bus.try_reserve_input().unwrap(),
        Box::new(ReentrantDestructor {
            bus: bus.clone(),
            result: result_sender,
        }),
    );
    assert!(block_on(running.shutdown()).unwrap().is_clean());
    assert_eq!(
        result_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        Ok(())
    );
    assert!(service.shutdown().clean);
}

struct PanickingInput {
    drops: Arc<AtomicUsize>,
}

impl MixerInput for PanickingInput {
    fn render(&mut self, _output: &mut [MixerFrame]) -> MixerInputStatus {
        panic!("hostile input")
    }
}

impl Drop for PanickingInput {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn render_panic_fails_closed_and_is_destroyed_only_off_render() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone()), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(4)).unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let running = start_reserved(
        session.script_bus().try_reserve_input().unwrap(),
        Box::new(PanickingInput {
            drops: drops.clone(),
        }),
    );

    let mut output = [1.0; INTERNAL_BUFFER_FRAMES * 2];
    probe.render(&mut output);
    assert_samples(&output, 0.0);
    assert!(running.is_failed());
    assert_eq!(drops.load(Ordering::Relaxed), 0);

    let retirement = block_on(running.shutdown()).unwrap();
    assert!(retirement.failed_before_retirement);
    assert!(!retirement.source_destructor_panicked);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
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
        panic!("hostile destructor")
    }
}

#[test]
fn hostile_destructor_is_contained_and_quarantines_capacity() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start_with_limits(
        settings(probe),
        MixerFormat {
            sample_rate: TEST_RATE,
        },
        1,
        1,
    )
    .unwrap();
    let session = service.add_session(AudioSessionId(5)).unwrap();
    let bus = session.script_bus();
    let running = start_reserved(
        bus.try_reserve_input().unwrap(),
        Box::new(PanickingDestructor),
    );
    assert_eq!(
        block_on(running.shutdown()),
        Err(MixerRetirementError::SourceDestructorPanicked)
    );
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));
    assert!(
        !service.shutdown().clean,
        "a quarantined destructor cannot produce a clean service proof"
    );
}

#[test]
fn forced_hostile_destructor_makes_service_shutdown_unclean_and_wakes_owner() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(36)).unwrap();
    let running = start_reserved(
        session.script_bus().try_reserve_input().unwrap(),
        Box::new(PanickingDestructor),
    );

    assert!(!service.shutdown().clean);
    assert_eq!(
        block_on(running.shutdown()),
        Err(MixerRetirementError::SourceDestructorPanicked)
    );
}

#[test]
fn missing_required_payload_is_structural_and_never_recycled_cleanly() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start_with_limits(
        settings(probe),
        MixerFormat {
            sample_rate: TEST_RATE,
        },
        1,
        1,
    )
    .unwrap();
    let session = service.add_session(AudioSessionId(37)).unwrap();
    let bus = session.script_bus();
    let (source, _, drops) = constant(0.25);
    let running = start_reserved(bus.try_reserve_input().unwrap(), source);
    // SAFETY: this test has no renderer entry and deliberately simulates a
    // corrupt Running state whose HAS_PAYLOAD bit no longer matches storage.
    let stolen = unsafe { (**running.slot.payload.get()).take() }.unwrap();
    assert_eq!(
        block_on(running.shutdown()),
        Err(MixerRetirementError::Structural)
    );
    assert!(matches!(
        bus.try_reserve_input(),
        Err(MixerControlError::InputCapacity)
    ));
    drop(stolen);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(!service.shutdown().clean);
}

fn standalone_running(
    control: &Arc<ControlInner>,
    drops: Arc<AtomicUsize>,
) -> (RunningMixerInput, Arc<InputSlot>, u64) {
    let slot = Arc::new(InputSlot::new(SlotAddress {
        session: SessionKey {
            id: AudioSessionId(38),
            generation: 1,
        },
        bus: SessionBus::Script,
        index: 0,
    }));
    let generation = slot.reserve().unwrap();
    assert!(
        slot.install(
            generation,
            Box::new(ConstantInput {
                frame: MixerFrame::ZERO,
                calls: Arc::new(AtomicUsize::new(0)),
                drops,
            }),
        )
        .is_ok()
    );
    assert!(slot.open(generation));
    (
        RunningMixerInput {
            slot: ManuallyDrop::new(Arc::clone(&slot)),
            generation,
            control: Arc::downgrade(control),
        },
        slot,
        generation,
    )
}

fn reclaim_retained_slot(shutdown: &mut MixerInputShutdown) -> Arc<InputSlot> {
    let slot = match &mut shutdown.state {
        ShutdownState::RetainedError {
            _slot: retained_slot,
            ..
        }
        | ShutdownState::Forced {
            slot: retained_slot,
            ..
        } => {
            // SAFETY: the test immediately marks the future Finished and takes
            // the sole retained authority exactly once for explicit cleanup.
            unsafe { ManuallyDrop::take(retained_slot) }
        }
        _ => panic!("shutdown did not retain strong slot authority"),
    };
    shutdown.state = ShutdownState::Finished;
    slot
}

#[test]
fn retirement_queue_full_and_disconnect_retain_exact_strong_authority() {
    for disconnected in [false, true] {
        let (commands, _command_receiver) = mpsc::sync_channel(1);
        let (retirements, retirement_receiver) = mpsc::sync_channel(0);
        if disconnected {
            drop(retirement_receiver);
        }
        let control = test_control(commands, retirements);
        let drops = Arc::new(AtomicUsize::new(0));
        let (running, slot, generation) = standalone_running(&control, drops.clone());
        let mut shutdown = running.shutdown();
        let safe_waker = Waker::from(Arc::new(WakeProbe::default()));
        let mut context = Context::from_waker(&safe_waker);
        if disconnected {
            assert!(matches!(
                Pin::new(&mut shutdown).poll(&mut context),
                Poll::Pending
            ));
        } else {
            assert!(matches!(
                Pin::new(&mut shutdown).poll(&mut context),
                Poll::Ready(Err(MixerRetirementError::QueueInvariant))
            ));
        }
        assert!(Arc::strong_count(&slot) >= 2);
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        let retained = reclaim_retained_slot(&mut shutdown);
        let mut prepared = retained.prepare_retirement(generation).unwrap();
        drop(prepared.source.take());
        let _ = retained.finish_terminal(generation, prepared.result);
        drop(retained);
        drop(slot);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn cleanup_queue_full_retains_callback_without_owner_thread_drop() {
    let slot = Arc::new(InputSlot::new(SlotAddress {
        session: SessionKey {
            id: AudioSessionId(39),
            generation: 1,
        },
        bus: SessionBus::Script,
        index: 0,
    }));
    let generation = slot.reserve().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    assert!(
        slot.install(
            generation,
            Box::new(ConstantInput {
                frame: MixerFrame::ZERO,
                calls: Arc::new(AtomicUsize::new(0)),
                drops: drops.clone(),
            }),
        )
        .is_ok()
    );
    assert!(slot.open(generation));
    assert!(slot.close(generation, false));
    let prepared = slot.prepare_retirement(generation).unwrap();
    let (completion, completed) = oneshot::channel();
    let job = CleanupJob {
        record: ReservedRecord {
            slot: Arc::clone(&slot),
            generation,
        },
        prepared,
        completion: Some(completion),
        terminal: false,
    };
    let (cleanup, _receiver) = mpsc::sync_channel(0);
    let Err(TrySendError::Full(job)) = cleanup.try_send(job) else {
        panic!("zero-capacity cleanup queue must reject without dropping the job");
    };
    retain_failed_cleanup_job(job);

    assert_eq!(
        block_on(completed).unwrap(),
        Err(MixerRetirementError::OwnerUncertain)
    );
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(
        slot_phase(slot.word.load(Ordering::Acquire)),
        SlotPhase::Quarantined
    );
}

#[test]
fn service_shutdown_force_cleans_live_endpoints_and_future_is_ready() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone()), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(6)).unwrap();
    let (source, _, drops) = constant(0.25);
    let running = start_reserved(session.script_bus().try_reserve_input().unwrap(), source);

    assert!(service.shutdown().clean);
    assert!(!probe.is_live());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(block_on(running.shutdown()).unwrap().is_clean());
}

#[test]
fn sealed_owner_returns_source_and_reservation_for_honest_cleanup() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(8)).unwrap();
    let reservation = session.script_bus().try_reserve_input().unwrap();
    assert!(service.shutdown().clean);

    let (source, _, drops) = constant(0.5);
    let MixerInputStartFailure::Rejected(failure) = reservation.start_preboxed(source).unwrap_err()
    else {
        panic!("sealed owner must reject before installation");
    };
    assert_eq!(failure.error(), MixerControlError::OwnerStopped);
    let (error, reservation, source) = failure.into_parts();
    assert_eq!(error, MixerControlError::OwnerStopped);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(source);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(block_on(reservation.abort()).unwrap().is_clean());
}

#[test]
fn backend_failures_and_rate_mismatch_are_typed_and_joined() {
    let setup_probe = RenderProbe::default();
    let setup_error = MixerService::<TestBackend>::start(
        TestBackendSettings {
            fail_setup: true,
            ..settings(setup_probe.clone())
        },
        TEST_RATE,
    )
    .unwrap_err();
    assert!(matches!(
        setup_error,
        MixerStartError::Backend(TestBackendError::Setup)
    ));
    assert!(!setup_probe.is_live());

    let start_probe = RenderProbe::default();
    let start_drops = Arc::new(AtomicUsize::new(0));
    let start_error = MixerService::<TestBackend>::start(
        TestBackendSettings {
            fail_start: true,
            backend_drops: start_drops.clone(),
            ..settings(start_probe.clone())
        },
        TEST_RATE,
    )
    .unwrap_err();
    assert!(matches!(
        start_error,
        MixerStartError::Backend(TestBackendError::Start)
    ));
    assert!(!start_probe.is_live());
    assert_eq!(start_drops.load(Ordering::Relaxed), 1);

    let rate_probe = RenderProbe::default();
    let rate_drops = Arc::new(AtomicUsize::new(0));
    let rate_error = MixerService::<TestBackend>::start(
        TestBackendSettings {
            actual_rate: 44_100,
            backend_drops: rate_drops.clone(),
            ..settings(rate_probe.clone())
        },
        TEST_RATE,
    )
    .unwrap_err();
    assert!(matches!(
        rate_error,
        MixerStartError::SampleRateMismatch {
            expected: TEST_RATE,
            actual: 44_100
        }
    ));
    assert!(!rate_probe.is_live());
    assert_eq!(rate_drops.load(Ordering::Relaxed), 1);
}

#[test]
fn invalid_gain_is_rejected_before_owner_mutation() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(11)).unwrap();
    let bus = session.script_bus();
    assert_eq!(bus.set_gain(-0.1), Err(MixerControlError::InvalidGain));
    assert_eq!(bus.set_gain(f32::NAN), Err(MixerControlError::InvalidGain));
    assert_eq!(
        bus.set_gain(f32::INFINITY),
        Err(MixerControlError::InvalidGain)
    );
    assert!(service.shutdown().clean);
}

#[test]
fn close_is_absorbing_even_after_finished_callback() {
    struct OneShot(Arc<AtomicBool>);

    impl MixerInput for OneShot {
        fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
            output.fill(MixerFrame::from_mono(0.5));
            self.0.store(true, Ordering::Release);
            MixerInputStatus::Finished
        }
    }

    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone()), TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(12)).unwrap();
    let rendered = Arc::new(AtomicBool::new(false));
    let running = start_reserved(
        session.script_bus().try_reserve_input().unwrap(),
        Box::new(OneShot(rendered.clone())),
    );
    let mut output = [0.0; INTERNAL_BUFFER_FRAMES * 2];
    probe.render(&mut output);
    assert_samples(&output, 0.5);
    assert!(rendered.load(Ordering::Acquire));
    assert!(!running.resume());
    probe.render(&mut output);
    assert_samples(&output, 0.0);
    assert!(block_on(running.shutdown()).unwrap().is_clean());
    assert!(service.shutdown().clean);
}
