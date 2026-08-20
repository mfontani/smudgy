//! Hosted Web Audio output adapter for Smudgy's shared script mixer bus.

use std::panic::{self, AssertUnwindSafe};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
#[cfg(test)]
use std::sync::{Barrier, mpsc};
#[cfg(test)]
use std::thread::{self, ThreadId};

use deno_audio::{
    AudioOutputConfig, AudioOutputDeathReason, AudioOutputEndpointShutdown, AudioOutputError,
    AudioOutputErrorKind, AudioOutputEventSink, AudioOutputFactory, AudioOutputRequest,
    AudioOutputStartFailure, AudioRenderCallback, AudioRenderFormat, AudioRenderStatus,
    PreparedAudioOutput, RunningAudioOutput,
};
use smudgy_audio::{
    MixerControlError, MixerFailureObserver, MixerFrame, MixerInput, MixerInputReservation,
    MixerInputShutdown, MixerInputStartFailure, MixerInputStatus, MixerOutputFailure,
    MixerRetirementError, MixerScriptBusHandle, RunningMixerInput,
};

const CHANNELS: usize = 2;
const FRAMES: usize = 128;
const INTERLEAVED_SAMPLES: usize = CHANNELS * FRAMES;

/// Creates one private hosted Web Audio output on a session's Script mixer bus.
///
/// Clones retain only the scoped bus control handle. The application-owned
/// mixer service and its physical backend remain outside this adapter.
#[derive(Clone, Debug)]
pub struct ScriptBusAudioOutputFactory {
    bus: MixerScriptBusHandle,
    #[cfg(test)]
    render_hook: Option<Arc<TestRenderHook>>,
    #[cfg(test)]
    panic_start: bool,
}

impl ScriptBusAudioOutputFactory {
    /// Binds hosted Web Audio context construction to one scoped Script bus.
    #[must_use]
    pub const fn new(bus: MixerScriptBusHandle) -> Self {
        Self {
            bus,
            #[cfg(test)]
            render_hook: None,
            #[cfg(test)]
            panic_start: false,
        }
    }

    #[cfg(test)]
    fn with_render_hook(bus: MixerScriptBusHandle, render_hook: Arc<TestRenderHook>) -> Self {
        Self {
            bus,
            render_hook: Some(render_hook),
            panic_start: false,
        }
    }

    #[cfg(test)]
    fn with_start_panic(bus: MixerScriptBusHandle, render_hook: Arc<TestRenderHook>) -> Self {
        Self {
            bus,
            render_hook: Some(render_hook),
            panic_start: true,
        }
    }

    #[cfg(test)]
    fn preboxed_input(&self) -> Box<CallbackMixerInput> {
        Box::new(CallbackMixerInput::with_render_hook(
            self.render_hook.clone(),
        ))
    }

    fn prepare_parts(
        &self,
        sink_id: &str,
        requested_sample_rate: Option<f32>,
        number_of_channels: usize,
    ) -> Result<Box<ScriptBusPreparedOutput>, AudioOutputError> {
        if !sink_id.is_empty() {
            return Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "the shared Smudgy mixer supports only the default output sink",
            ));
        }
        if number_of_channels != CHANNELS {
            return Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "the shared Smudgy mixer requires stereo output",
            ));
        }

        let mixer_format = self.bus.format().map_err(control_error)?;
        // Web Audio publishes sample rates as f32 while the mixer publishes an
        // integer device rate. AudioRenderFormat validates the converted value.
        #[allow(clippy::cast_precision_loss)]
        let sample_rate = mixer_format.sample_rate() as f32;
        #[allow(clippy::float_cmp)] // the output contract requires an exact requested rate
        if requested_sample_rate.is_some_and(|requested| requested != sample_rate) {
            return Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "the requested sample rate does not match the shared Smudgy mixer",
            ));
        }
        if mixer_format.number_of_channels() != CHANNELS
            || mixer_format.max_frames_per_callback() != FRAMES
        {
            return Err(output_error(
                AudioOutputErrorKind::BackendSpecific,
                "the shared Smudgy mixer published an incompatible logical format",
            ));
        }

        let format = AudioRenderFormat::new(sample_rate, CHANNELS, FRAMES)?;
        let config = AudioOutputConfig::new(
            format,
            String::new(),
            128.0 / f64::from(mixer_format.sample_rate()),
        )?;

        // Every heap object, including the outer trait object, callback
        // scratch, and running owner, exists before the bounded reservation
        // mutates capacity. Reservation insertion is then infallible.
        #[cfg(test)]
        let input = self.preboxed_input();
        #[cfg(not(test))]
        let input = Box::new(CallbackMixerInput::new());
        let mut prepared = Box::new(ScriptBusPreparedOutput {
            config,
            reservation: None,
            input: Some(input),
            running: Some(Box::new(ScriptBusRunningOutput {
                input: None,
                #[cfg(test)]
                render_hook: self.render_hook.clone(),
            })),
            #[cfg(test)]
            panic_start: self.panic_start,
        });
        let reservation = self.bus.try_reserve_input().map_err(control_error)?;
        prepared.reservation = Some(reservation);
        Ok(prepared)
    }
}

impl AudioOutputFactory for ScriptBusAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        self.prepare_parts(
            request.sink_id(),
            request.requested_sample_rate(),
            request.number_of_channels(),
        )
        .map(|prepared| prepared as Box<dyn PreparedAudioOutput>)
    }
}

struct ScriptBusPreparedOutput {
    config: AudioOutputConfig,
    reservation: Option<MixerInputReservation>,
    input: Option<Box<CallbackMixerInput>>,
    running: Option<Box<ScriptBusRunningOutput>>,
    #[cfg(test)]
    panic_start: bool,
}

impl PreparedAudioOutput for ScriptBusPreparedOutput {
    fn config(&self) -> &AudioOutputConfig {
        &self.config
    }

    fn start(
        mut self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        let reservation = self
            .reservation
            .take()
            .expect("prepared output owns one reservation");
        let mut input = self.input.take().expect("prepared output owns one input");
        let running = self
            .running
            .take()
            .expect("prepared output owns one preboxed running endpoint");
        debug_assert!(running.input.is_none());
        let expected = self.config.format();
        let actual = callback.format();
        input.install(callback, events.clone());
        let source: Box<dyn MixerInput> = input;

        if actual != expected {
            let _ = events.report_endpoint_death(AudioOutputDeathReason::CallbackProtocolViolation);
            return Err(abort_start(
                output_error(
                    AudioOutputErrorKind::BackendSpecific,
                    "the Web Audio callback format did not match the prepared mixer endpoint",
                ),
                reservation,
                source,
            ));
        }

        #[cfg(test)]
        let panic_start = self.panic_start;
        let mut pending_start = Some((reservation, source));
        let start = panic::catch_unwind(AssertUnwindSafe(|| {
            #[cfg(test)]
            assert!(!panic_start, "injected mixer start panic");
            let (reservation, source) = pending_start
                .take()
                .expect("start authorities are consumed exactly once");
            reservation.start_preboxed(source)
        }));
        match start {
            Ok(Ok(input)) => Ok(publish_running(running, input)),
            Ok(Err(MixerInputStartFailure::Rejected(failure))) => {
                let (error, reservation, source) = failure.into_parts();
                let reason = if error == MixerControlError::OwnerStopped {
                    AudioOutputDeathReason::FactoryShutdown
                } else {
                    AudioOutputDeathReason::BackendFailure
                };
                let _ = events.report_endpoint_death(reason);
                Err(abort_start(control_error(error), reservation, source))
            }
            Ok(Err(MixerInputStartFailure::Cleanup(shutdown))) => {
                let _ = events.report_endpoint_death(AudioOutputDeathReason::BackendFailure);
                Err(AudioOutputStartFailure::new(
                    output_error(
                        AudioOutputErrorKind::BackendSpecific,
                        "the mixer could not publish the installed Web Audio callback",
                    ),
                    endpoint_shutdown(shutdown),
                ))
            }
            Err(payload) => {
                std::mem::forget(payload);
                let _ = events.report_endpoint_death(AudioOutputDeathReason::BackendFailure);
                if let Some((reservation, source)) = pending_start.take() {
                    return Err(panicked_start_with_owned_cleanup(reservation, source));
                }
                // A panic after the authorities crossed into smudgy_audio can
                // no longer recover their exact cleanup observer. Their Drop
                // paths remain fail-closed, but this result must be explicitly
                // unconfirmed.
                Err(AudioOutputStartFailure::new(
                    output_error(
                        AudioOutputErrorKind::BackendSpecific,
                        "the mixer start transaction panicked",
                    ),
                    AudioOutputEndpointShutdown::ready(Err(output_error(
                        AudioOutputErrorKind::Shutdown,
                        "mixer start cleanup could not be confirmed after a panic",
                    ))),
                ))
            }
        }
    }

    fn abort(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        let reservation = self
            .reservation
            .take()
            .expect("prepared output owns one reservation");
        // This shell never received a callback, so its synchronous destruction
        // is not part of the proof-bearing retirement.
        self.input.take();
        self.running.take();
        endpoint_shutdown(reservation.abort())
    }
}

struct CallbackMixerInput {
    callback: Option<AudioRenderCallback>,
    events: Arc<OutputFailureReporter>,
    scratch: [f32; INTERLEAVED_SAMPLES],
    #[cfg(test)]
    render_hook: Option<Arc<TestRenderHook>>,
}

impl CallbackMixerInput {
    #[cfg(not(test))]
    fn new() -> Self {
        Self {
            callback: None,
            events: Arc::new(OutputFailureReporter::new()),
            scratch: [0.0; INTERLEAVED_SAMPLES],
            #[cfg(test)]
            render_hook: None,
        }
    }

    #[cfg(test)]
    fn with_render_hook(render_hook: Option<Arc<TestRenderHook>>) -> Self {
        Self {
            callback: None,
            events: Arc::new(OutputFailureReporter::new()),
            scratch: [0.0; INTERLEAVED_SAMPLES],
            render_hook,
        }
    }

    fn install(&mut self, callback: AudioRenderCallback, events: AudioOutputEventSink) {
        debug_assert!(self.callback.is_none());
        self.callback = Some(callback);
        self.events.install(events);
    }
}

struct OutputFailureReporter {
    events: OnceLock<AudioOutputEventSink>,
    reported: AtomicBool,
}

impl OutputFailureReporter {
    const fn new() -> Self {
        Self {
            events: OnceLock::new(),
            reported: AtomicBool::new(false),
        }
    }

    fn install(&self, events: AudioOutputEventSink) {
        assert!(
            self.events.set(events).is_ok(),
            "output event reporter is installed exactly once"
        );
    }

    fn report(&self, reason: AudioOutputDeathReason) {
        if let Some(events) = self.events.get()
            && !self.reported.swap(true, Ordering::AcqRel)
        {
            let _ = events.report_endpoint_death(reason);
        }
    }
}

impl MixerFailureObserver for OutputFailureReporter {
    fn output_failed(&self, _failure: MixerOutputFailure) {
        self.report(AudioOutputDeathReason::BackendFailure);
    }
}

impl MixerInput for CallbackMixerInput {
    fn output_failure_observer(&self) -> Option<Arc<dyn MixerFailureObserver>> {
        Some(Arc::clone(&self.events) as Arc<dyn MixerFailureObserver>)
    }

    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        let Some(callback) = &mut self.callback else {
            output.fill(MixerFrame::ZERO);
            return MixerInputStatus::Finished;
        };
        let events = &self.events;
        let status = render_fixed(
            output,
            &mut self.scratch,
            |scratch| callback.render_interleaved_f32(scratch),
            |reason| {
                events.report(reason);
            },
        );
        #[cfg(test)]
        if let Some(hook) = self
            .render_hook
            .as_ref()
            .filter(|hook| hook.block_once.swap(false, Ordering::AcqRel))
        {
            let _ = hook.entered.try_send(thread::current().id());
            hook.release.wait();
        }
        status
    }
}

#[cfg(test)]
impl Drop for CallbackMixerInput {
    fn drop(&mut self) {
        if let Some(hook) = self.render_hook.as_ref() {
            let _ = hook.dropped.try_send(thread::current().id());
            assert!(
                !hook.panic_on_drop,
                "injected callback shell destructor panic"
            );
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
struct TestRenderHook {
    entered: mpsc::SyncSender<ThreadId>,
    release: Arc<Barrier>,
    dropped: mpsc::SyncSender<ThreadId>,
    shutdown: mpsc::SyncSender<()>,
    block_once: AtomicBool,
    panic_on_drop: bool,
}

fn render_fixed(
    output: &mut [MixerFrame],
    scratch: &mut [f32; INTERLEAVED_SAMPLES],
    mut render: impl FnMut(&mut [f32]) -> AudioRenderStatus,
    mut report_death: impl FnMut(AudioOutputDeathReason),
) -> MixerInputStatus {
    output.fill(MixerFrame::ZERO);
    if output.is_empty() || output.len() > FRAMES {
        report_death(AudioOutputDeathReason::CallbackProtocolViolation);
        return MixerInputStatus::Finished;
    }

    let sample_count = output.len() * CHANNELS;
    match panic::catch_unwind(AssertUnwindSafe(|| render(&mut scratch[..sample_count]))) {
        Ok(AudioRenderStatus::Continue) => {
            for (frame, samples) in output
                .iter_mut()
                .zip(scratch[..sample_count].chunks_exact(2))
            {
                *frame = MixerFrame::new(samples[0], samples[1]);
            }
            MixerInputStatus::Active
        }
        Ok(_) => {
            scratch[..sample_count].fill(0.0);
            MixerInputStatus::Finished
        }
        Err(payload) => {
            std::mem::forget(payload);
            scratch[..sample_count].fill(0.0);
            report_death(AudioOutputDeathReason::CallbackPanicked);
            MixerInputStatus::Finished
        }
    }
}

struct ScriptBusRunningOutput {
    input: Option<RunningMixerInput>,
    #[cfg(test)]
    render_hook: Option<Arc<TestRenderHook>>,
}

impl RunningAudioOutput for ScriptBusRunningOutput {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        transition(self.input.as_ref(), false)
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        transition(self.input.as_ref(), true)
    }

    fn shutdown(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        #[cfg(test)]
        if let Some(hook) = self.render_hook.as_ref() {
            let _ = hook.shutdown.try_send(());
        }
        let Some(input) = self.input.take() else {
            return AudioOutputEndpointShutdown::ready(Err(output_error(
                AudioOutputErrorKind::Shutdown,
                "the Web Audio mixer endpoint had no running input",
            )));
        };
        endpoint_shutdown(input.shutdown())
    }
}

fn publish_running(
    mut owner: Box<ScriptBusRunningOutput>,
    input: RunningMixerInput,
) -> Box<dyn RunningAudioOutput> {
    owner.input = Some(input);
    owner
}

fn transition(input: Option<&RunningMixerInput>, suspended: bool) -> Result<(), AudioOutputError> {
    let Some(input) = input else {
        return Err(output_error(
            AudioOutputErrorKind::Shutdown,
            "the Web Audio mixer endpoint is already closed",
        ));
    };
    let applied = if suspended {
        input.suspend()
    } else {
        input.resume()
    };
    if applied {
        Ok(())
    } else if input.output_failure().is_some() {
        // The exact process-output cause is delivered by the preinstalled
        // failure observer. A close-time suspend after that global death is a
        // no-op, not a loss of callback-retirement proof.
        Ok(())
    } else if input.is_failed() {
        Err(output_error(
            AudioOutputErrorKind::CallbackDied,
            "the Web Audio mixer callback has failed closed",
        ))
    } else {
        Err(output_error(
            AudioOutputErrorKind::Shutdown,
            "the Web Audio mixer endpoint is closing",
        ))
    }
}

fn abort_start(
    error: AudioOutputError,
    reservation: MixerInputReservation,
    source: Box<dyn MixerInput>,
) -> AudioOutputStartFailure {
    let shutdown = reservation.abort();
    AudioOutputStartFailure::new(
        error,
        AudioOutputEndpointShutdown::from_future(async move {
            let retirement = shutdown.await;
            let dropped = panic::catch_unwind(AssertUnwindSafe(|| drop(source)));
            let destructor_panicked = match dropped {
                Ok(()) => false,
                Err(payload) => {
                    std::mem::forget(payload);
                    true
                }
            };
            if destructor_panicked {
                return Err(output_error(
                    AudioOutputErrorKind::Shutdown,
                    "the rejected Web Audio callback destructor panicked",
                ));
            }
            retirement_result(retirement)
        }),
    )
}

fn panicked_start_with_owned_cleanup(
    reservation: MixerInputReservation,
    source: Box<dyn MixerInput>,
) -> AudioOutputStartFailure {
    let shutdown = reservation.abort();
    AudioOutputStartFailure::new(
        output_error(
            AudioOutputErrorKind::BackendSpecific,
            "the mixer start transaction panicked",
        ),
        AudioOutputEndpointShutdown::from_future(async move {
            let retirement = shutdown.await;
            let dropped = panic::catch_unwind(AssertUnwindSafe(|| drop(source)));
            let destructor_panicked = match dropped {
                Ok(()) => false,
                Err(payload) => {
                    std::mem::forget(payload);
                    true
                }
            };
            let cleanup = retirement_result(retirement);
            let message = if destructor_panicked {
                "panicked start cleanup also encountered a callback destructor panic"
            } else if cleanup.is_err() {
                "panicked start cleanup could not be proven"
            } else {
                "panicked start was cleaned but remains an unconfirmed startup failure"
            };
            Err(output_error(AudioOutputErrorKind::Shutdown, message))
        }),
    )
}

fn endpoint_shutdown(shutdown: MixerInputShutdown) -> AudioOutputEndpointShutdown {
    AudioOutputEndpointShutdown::from_future(async move { retirement_result(shutdown.await) })
}

fn retirement_result(
    result: Result<smudgy_audio::MixerInputRetirement, MixerRetirementError>,
) -> Result<(), AudioOutputError> {
    match result {
        Ok(retirement) if retirement.source_destructor_panicked => Err(output_error(
            AudioOutputErrorKind::Shutdown,
            "the Web Audio mixer callback destructor panicked",
        )),
        Ok(retirement)
            if retirement.failed_before_retirement && retirement.output_failure.is_none() =>
        {
            Err(output_error(
                AudioOutputErrorKind::CallbackDied,
                "the Web Audio mixer callback failed before retirement",
            ))
        }
        // A global output cause was separately published through the exact
        // input's event sink. Proven off-render destruction confirms cleanup;
        // the operational failure remains in the context report.
        Ok(_) => Ok(()),
        Err(error) => Err(retirement_error(error)),
    }
}

fn control_error(error: MixerControlError) -> AudioOutputError {
    let kind = match error {
        MixerControlError::OwnerStopped => AudioOutputErrorKind::DeviceUnavailable,
        MixerControlError::InputCapacity | MixerControlError::SessionCapacity => {
            AudioOutputErrorKind::DeviceUnavailable
        }
        _ => AudioOutputErrorKind::BackendSpecific,
    };
    AudioOutputError::new(
        kind,
        format!("shared Smudgy mixer rejected output: {error:?}"),
    )
}

fn retirement_error(error: MixerRetirementError) -> AudioOutputError {
    AudioOutputError::new(
        AudioOutputErrorKind::Shutdown,
        format!("shared Smudgy mixer could not prove output retirement: {error:?}"),
    )
}

fn output_error(kind: AudioOutputErrorKind, message: &'static str) -> AudioOutputError {
    AudioOutputError::new(kind, message)
}

#[cfg(test)]
mod tests;
