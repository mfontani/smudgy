use std::{
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use kira::backend::Renderer;

use super::*;

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

#[derive(Clone)]
struct TestBackendSettings {
    probe: RenderProbe,
    fail_setup: bool,
}

struct TestBackend {
    probe: RenderProbe,
}

impl Backend for TestBackend {
    type Settings = TestBackendSettings;
    type Error = &'static str;

    fn setup(
        settings: Self::Settings,
        _internal_buffer_size: usize,
    ) -> Result<(Self, u32), Self::Error> {
        if settings.fail_setup {
            return Err("forced setup failure");
        }
        Ok((
            Self {
                probe: settings.probe,
            },
            48_000,
        ))
    }

    fn start(&mut self, renderer: Renderer) -> Result<(), Self::Error> {
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
    }
}

struct ConstantInput {
    frame: Frame,
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl MixerInput for ConstantInput {
    fn render(&mut self, output: &mut [Frame]) -> MixerInputStatus {
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

fn constant(
    value: f32,
) -> (
    MixerInputOwner,
    MixerInputSound,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (owner, sound) = MixerInputOwner::new(ConstantInput {
        frame: Frame::from_mono(value),
        calls: calls.clone(),
        drops: drops.clone(),
    });
    (owner, sound, calls, drops)
}

fn settings(probe: RenderProbe) -> TestBackendSettings {
    TestBackendSettings {
        probe,
        fail_setup: false,
    }
}

fn assert_samples(output: &[f32], expected: f32) {
    for &sample in output {
        assert!(
            (sample - expected).abs() < 1.0e-6,
            "expected {expected}, got {sample}"
        );
    }
}

#[test]
fn service_builds_fixed_hierarchy_and_joins_backend_before_success() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone())).unwrap();
    let session = AudioSessionId(7);
    service.add_session(session).unwrap();

    let mut owners = Vec::new();
    for (bus, value) in [
        (SessionBus::Script, 0.125),
        (SessionBus::Native, 0.25),
        (SessionBus::Speech, 0.5),
    ] {
        let (owner, sound, _, _) = constant(value);
        service.play(session, bus, sound).unwrap();
        assert!(owner.start());
        owners.push(owner);
    }

    let mut output = [0.0; INTERNAL_BUFFER_FRAMES * 2];
    probe.render(&mut output);
    assert_samples(&output, 0.875);

    let shutdown = service.shutdown();
    assert!(shutdown.clean);
    assert!(
        !probe.is_live(),
        "joined owner must drop the backend renderer"
    );
    for owner in owners {
        assert!(owner.try_retire().unwrap().is_clean());
    }
}

#[test]
fn prestart_is_silent_and_steady_state_render_allocates_nothing() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone())).unwrap();
    let session = AudioSessionId(9);
    service.add_session(session).unwrap();
    let (owner, sound, calls, drops) = constant(0.375);
    service.play(session, SessionBus::Script, sound).unwrap();

    let mut output = [1.0; INTERNAL_BUFFER_FRAMES * 2];
    probe.render(&mut output);
    assert_samples(&output, 0.0);
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    assert!(owner.start());
    probe.render(&mut output);
    assert_samples(&output, 0.375);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    assert_no_alloc::assert_no_alloc(|| probe.render(&mut output));
    assert_samples(&output, 0.375);
    assert_eq!(calls.load(Ordering::Relaxed), 2);

    assert!(service.shutdown().clean);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert!(owner.try_retire().unwrap().is_clean());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn logical_suspend_is_independent_and_close_dominates_resume() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone())).unwrap();
    let session = AudioSessionId(10);
    service.add_session(session).unwrap();
    let (script_owner, script_sound, _, _) = constant(0.125);
    let (native_owner, native_sound, _, _) = constant(0.25);
    service
        .play(session, SessionBus::Script, script_sound)
        .unwrap();
    service
        .play(session, SessionBus::Native, native_sound)
        .unwrap();
    assert!(script_owner.start());
    assert!(native_owner.start());

    let mut output = [0.0; INTERNAL_BUFFER_FRAMES * 2];
    probe.render(&mut output);
    assert_samples(&output, 0.375);

    assert!(script_owner.suspend());
    assert!(script_owner.is_suspended());
    probe.render(&mut output);
    assert_samples(&output, 0.25);

    assert!(script_owner.resume());
    assert!(!script_owner.is_suspended());
    probe.render(&mut output);
    assert_samples(&output, 0.375);

    native_owner.close();
    assert!(!native_owner.resume());
    probe.render(&mut output);
    assert_samples(&output, 0.125);
    assert!(native_owner.try_retire().unwrap().is_clean());

    assert!(service.shutdown().clean);
    assert!(script_owner.try_retire().unwrap().is_clean());
}

#[test]
fn capacities_reject_before_mutating_an_existing_topology() {
    let probe = RenderProbe::default();
    let mut mixer = MixerCore::<TestBackend>::with_limits(settings(probe), 1, 1).unwrap();
    let session = AudioSessionId(1);
    assert_eq!(mixer.add_session(session), Ok(()));
    assert_eq!(
        mixer.add_session(session),
        Err(MixerMutationError::DuplicateSession)
    );
    assert_eq!(
        mixer.add_session(AudioSessionId(2)),
        Err(MixerMutationError::SessionCapacity)
    );

    let (first_owner, first_sound, _, _) = constant(0.1);
    let (second_owner, second_sound, _, _) = constant(0.2);
    assert_eq!(mixer.play(session, SessionBus::Native, first_sound), Ok(()));
    assert_eq!(
        mixer.play(session, SessionBus::Native, second_sound),
        Err(MixerMutationError::InputCapacity)
    );
    assert_eq!(
        mixer.remove_session(AudioSessionId(3)),
        Err(MixerMutationError::UnknownSession)
    );
    assert_eq!(mixer.remove_session(session), Ok(()));
    drop(mixer);
    assert!(first_owner.try_retire().unwrap().is_clean());
    assert!(second_owner.try_retire().unwrap().is_clean());
}

struct BlockingInput {
    entered: mpsc::SyncSender<thread::ThreadId>,
    release: Arc<Barrier>,
    drops: Arc<AtomicUsize>,
}

impl MixerInput for BlockingInput {
    fn render(&mut self, output: &mut [Frame]) -> MixerInputStatus {
        self.entered.send(thread::current().id()).unwrap();
        self.release.wait();
        output.fill(Frame::from_mono(0.25));
        MixerInputStatus::Active
    }
}

impl Drop for BlockingInput {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn active_render_prevents_early_callback_destruction() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone())).unwrap();
    let session = AudioSessionId(3);
    service.add_session(session).unwrap();

    let (entered_sender, entered_receiver) = mpsc::sync_channel(1);
    let release = Arc::new(Barrier::new(2));
    let drops = Arc::new(AtomicUsize::new(0));
    let (owner, sound) = MixerInputOwner::new(BlockingInput {
        entered: entered_sender,
        release: release.clone(),
        drops: drops.clone(),
    });
    service.play(session, SessionBus::Script, sound).unwrap();
    assert!(owner.start());

    let render_probe = probe.clone();
    let render = thread::spawn(move || {
        let mut output = [0.0; INTERNAL_BUFFER_FRAMES * 2];
        render_probe.render(&mut output);
        output
    });
    let render_thread = entered_receiver.recv().unwrap();
    let owner = owner
        .try_retire()
        .expect_err("active Arc must retain callback");
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    release.wait();
    let output = render.join().unwrap();
    assert_samples(&output, 0.25);
    assert_ne!(render_thread, thread::current().id());

    assert!(service.shutdown().clean);
    assert!(owner.try_retire().unwrap().is_clean());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

struct PanickingInput {
    drops: Arc<AtomicUsize>,
}

impl MixerInput for PanickingInput {
    fn render(&mut self, _output: &mut [Frame]) -> MixerInputStatus {
        panic!("hostile input")
    }
}

impl Drop for PanickingInput {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn input_panic_is_silenced_and_destroyed_only_during_retirement() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe.clone())).unwrap();
    let session = AudioSessionId(4);
    service.add_session(session).unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let (owner, sound) = MixerInputOwner::new(PanickingInput {
        drops: drops.clone(),
    });
    service.play(session, SessionBus::Script, sound).unwrap();
    assert!(owner.start());

    let mut output = [1.0; INTERNAL_BUFFER_FRAMES * 2];
    probe.render(&mut output);
    assert_samples(&output, 0.0);
    assert!(owner.is_failed());
    assert_eq!(drops.load(Ordering::Relaxed), 0);

    assert!(service.shutdown().clean);
    let retirement = owner.try_retire().unwrap();
    assert!(retirement.failed_before_retirement);
    assert!(!retirement.source_destructor_panicked);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

struct PanickingDestructor;

impl MixerInput for PanickingDestructor {
    fn render(&mut self, output: &mut [Frame]) -> MixerInputStatus {
        output.fill(Frame::ZERO);
        MixerInputStatus::Active
    }
}

impl Drop for PanickingDestructor {
    fn drop(&mut self) {
        panic!("hostile destructor")
    }
}

#[test]
fn hostile_source_destructor_is_contained_off_render() {
    let probe = RenderProbe::default();
    let service = MixerService::<TestBackend>::start(settings(probe)).unwrap();
    let session = AudioSessionId(5);
    service.add_session(session).unwrap();
    let (owner, sound) = MixerInputOwner::new(PanickingDestructor);
    service.play(session, SessionBus::Speech, sound).unwrap();
    assert!(owner.start());
    assert!(service.shutdown().clean);

    let retirement = owner.try_retire().unwrap();
    assert!(!retirement.failed_before_retirement);
    assert!(retirement.source_destructor_panicked);
    assert!(!retirement.is_clean());
}

#[test]
fn backend_start_failure_is_joined_and_typed() {
    let probe = RenderProbe::default();
    let error = MixerService::<TestBackend>::start(TestBackendSettings {
        probe: probe.clone(),
        fail_setup: true,
    })
    .unwrap_err();
    assert!(matches!(
        error,
        MixerStartError::Backend("forced setup failure")
    ));
    assert!(!probe.is_live());
}
