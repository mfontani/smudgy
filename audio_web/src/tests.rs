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
    test_support::{TestDriverConfig, TestDriverProbe, start_test_mixer},
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
    assert_factory::<ScriptBusAudioOutputFactory>();
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
fn render_copy_stop_panic_and_invalid_geometry_fail_closed_without_allocating() {
    let mut output = [MixerFrame::ZERO; FRAMES];
    let mut scratch = [0.0; INTERLEAVED_SAMPLES];
    let deaths = AtomicUsize::new(0);

    let status = assert_no_alloc::assert_no_alloc(|| {
        render_fixed(
            &mut output,
            &mut scratch,
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

    output.fill(MixerFrame::from_mono(1.0));
    let status = assert_no_alloc::assert_no_alloc(|| {
        render_fixed(
            &mut output,
            &mut scratch,
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
    inner: ScriptBusAudioOutputFactory,
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
        inner: ScriptBusAudioOutputFactory::new(session.script_bus()),
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
