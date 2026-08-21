//! Hosted Web Audio authorities and output adapter for Smudgy's shared mixer.

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
#[cfg(test)]
use std::sync::{Barrier, mpsc};
#[cfg(test)]
use std::thread::{self, ThreadId};

pub use deno_audio::AudioHostLimits;
use deno_audio::{
    AudioExtensionOptions, AudioHost, AudioHostUsage, AudioOutputConfig, AudioOutputDeathReason,
    AudioOutputEndpointShutdown, AudioOutputError, AudioOutputErrorKind, AudioOutputEventSink,
    AudioOutputFactory, AudioOutputRequest, AudioOutputStartFailure, AudioPermissions,
    AudioRenderCallback, AudioRenderFormat, AudioRenderStatus, PreparedAudioOutput,
    RunningAudioOutput, SystemAudioOutput,
};
use deno_error::JsErrorBox;
use smudgy_audio::{
    AudioSessionId, MixerControlError, MixerFailureObserver, MixerFrame, MixerInput,
    MixerInputReservation, MixerInputShutdown, MixerInputStartFailure, MixerInputStatus,
    MixerNativeBusHandle, MixerOutputFailure, MixerRetirementError, MixerScriptBusHandle,
    MixerSessionOwner, MixerSessionRetirement, MixerSpeechBusHandle, RunningMixerInput,
};

const CHANNELS: usize = 2;
const FRAMES: usize = 128;
const INTERLEAVED_SAMPLES: usize = CHANNELS * FRAMES;

static NEXT_SCOPE_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct LifecycleGate {
    open: bool,
}

impl LifecycleGate {
    const fn new() -> Self {
        Self { open: true }
    }
}

fn lock_gate(gate: &Mutex<LifecycleGate>) -> MutexGuard<'_, LifecycleGate> {
    gate.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ApplicationAudioState {
    host: Arc<AudioHost>,
    gate: Mutex<LifecycleGate>,
    #[cfg(test)]
    prepare_hook: Mutex<Option<Arc<TestPrepareHook>>>,
}

impl fmt::Debug for ApplicationAudioState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationAudioState")
            .field("host", &self.host)
            .field("open", &lock_gate(&self.gate).open)
            .finish_non_exhaustive()
    }
}

/// Unique owner of application-wide Web Audio admission.
///
/// The owner is deliberately not cloneable. Its cloneable registrar can mint
/// session authorities while this owner remains open; sealing or dropping the
/// owner rejects later registration and audible online-context preparation.
pub struct ApplicationAudioOwner {
    state: Arc<ApplicationAudioState>,
}

impl fmt::Debug for ApplicationAudioOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationAudioOwner")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl ApplicationAudioOwner {
    /// Creates one bounded application accounting and lifecycle domain.
    #[must_use]
    pub fn new(limits: AudioHostLimits) -> Self {
        Self {
            state: Arc::new(ApplicationAudioState {
                host: Arc::new(AudioHost::new(limits)),
                gate: Mutex::new(LifecycleGate::new()),
                #[cfg(test)]
                prepare_hook: Mutex::new(None),
            }),
        }
    }

    /// Returns a cloneable session registrar sharing this exact host and gate.
    #[must_use]
    pub fn registrar(&self) -> ApplicationAudioRegistrar {
        ApplicationAudioRegistrar {
            state: Arc::clone(&self.state),
        }
    }

    /// Returns a point-in-time, non-authoritative aggregate usage snapshot.
    #[must_use]
    pub fn usage(&self) -> AudioHostUsage {
        self.state.host.usage()
    }

    /// Absorbingly rejects new sessions and online output preparation.
    ///
    /// Returns `true` only for the transition from open to sealed.
    pub fn seal(&mut self) -> bool {
        let mut gate = lock_gate(&self.state.gate);
        let was_open = gate.open;
        gate.open = false;
        was_open
    }
}

impl Drop for ApplicationAudioOwner {
    fn drop(&mut self) {
        self.seal();
    }
}

/// Cloneable authority that registers exact mixer sessions with one app host.
#[derive(Clone)]
pub struct ApplicationAudioRegistrar {
    state: Arc<ApplicationAudioState>,
}

impl fmt::Debug for ApplicationAudioRegistrar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationAudioRegistrar")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// Stable classification for a rejected session registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionAudioRegistrationError {
    /// The unique application owner has sealed registration.
    ApplicationSealed,
    /// The process-wide non-wrapping scope identity space is exhausted.
    GenerationExhausted,
}

/// Registration failure that returns the unconsumed mixer-session owner.
pub struct SessionAudioRegistrationFailure {
    error: SessionAudioRegistrationError,
    owner: MixerSessionOwner,
}

impl fmt::Debug for SessionAudioRegistrationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionAudioRegistrationFailure")
            .field("error", &self.error)
            .field("owner", &self.owner)
            .finish()
    }
}

impl SessionAudioRegistrationFailure {
    /// Stable reason registration was rejected.
    #[must_use]
    pub const fn error(&self) -> SessionAudioRegistrationError {
        self.error
    }

    /// Returns the exact unconsumed mixer-session owner.
    #[must_use]
    pub fn into_owner(self) -> MixerSessionOwner {
        self.owner
    }
}

impl ApplicationAudioRegistrar {
    /// Consumes one exact mixer session and publishes its scoped authorities.
    ///
    /// # Errors
    ///
    /// Returns the owner unchanged when application admission is sealed.
    pub fn register_session(
        &self,
        owner: MixerSessionOwner,
    ) -> Result<SessionAudioRegistration, SessionAudioRegistrationFailure> {
        let app_gate = lock_gate(&self.state.gate);
        if !app_gate.open {
            return Err(SessionAudioRegistrationFailure {
                error: SessionAudioRegistrationError::ApplicationSealed,
                owner,
            });
        }

        let session_id = owner.session_id();
        let script = owner.script_bus();
        let native = owner.native_bus();
        let speech = owner.speech_bus();
        let session_gate = Arc::new(Mutex::new(LifecycleGate::new()));
        let permissions: Arc<dyn AudioPermissions> = Arc::new(SessionAudioPermissions {
            app: Arc::clone(&self.state),
            session: Arc::clone(&session_gate),
        });
        let output: Arc<dyn AudioOutputFactory> = Arc::new(GatedSessionAudioOutputFactory {
            app: Arc::clone(&self.state),
            session: Arc::clone(&session_gate),
            delegate: SessionAudioOutputFactory::new(script),
        });
        let Ok(generation) =
            NEXT_SCOPE_GENERATION.fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
        else {
            return Err(SessionAudioRegistrationFailure {
                error: SessionAudioRegistrationError::GenerationExhausted,
                owner,
            });
        };
        let scope = SessionAudioScope {
            inner: Arc::new(SessionAudioScopeInner {
                session_id,
                generation,
                app: Arc::clone(&self.state),
                session_gate,
                permissions,
                output,
            }),
        };
        drop(app_gate);

        Ok(SessionAudioRegistration {
            owner: Some(owner),
            native,
            speech,
            scope,
        })
    }

    /// Publishes one real Web Audio session scope without a physical mixer.
    ///
    /// The exact system-start cause is retained by the default-sink factory.
    /// `sinkId: "none"` remains a joinable system-silent endpoint; no mixer
    /// session, bus, slot, or physical format is created by this path.
    ///
    /// # Errors
    ///
    /// Returns a stable registration error when application admission is
    /// sealed or the non-wrapping scope identity space is exhausted.
    pub fn register_unavailable_session(
        &self,
        session_id: AudioSessionId,
        cause: UnavailableAudioOutputCause,
    ) -> Result<UnavailableSessionAudioRegistration, SessionAudioRegistrationError> {
        let app_gate = lock_gate(&self.state.gate);
        if !app_gate.open {
            return Err(SessionAudioRegistrationError::ApplicationSealed);
        }

        let session_gate = Arc::new(Mutex::new(LifecycleGate::new()));
        let permissions: Arc<dyn AudioPermissions> = Arc::new(SessionAudioPermissions {
            app: Arc::clone(&self.state),
            session: Arc::clone(&session_gate),
        });
        let output: Arc<dyn AudioOutputFactory> = Arc::new(GatedUnavailableAudioOutputFactory {
            app: Arc::clone(&self.state),
            session: Arc::clone(&session_gate),
            delegate: UnavailableSessionAudioOutputFactory::new(cause),
        });
        let Ok(generation) =
            NEXT_SCOPE_GENERATION.fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
        else {
            return Err(SessionAudioRegistrationError::GenerationExhausted);
        };
        let scope = SessionAudioScope {
            inner: Arc::new(SessionAudioScopeInner {
                session_id,
                generation,
                app: Arc::clone(&self.state),
                session_gate,
                permissions,
                output,
            }),
        };
        drop(app_gate);

        Ok(UnavailableSessionAudioRegistration { scope })
    }
}

struct SessionAudioScopeInner {
    session_id: AudioSessionId,
    generation: u64,
    app: Arc<ApplicationAudioState>,
    session_gate: Arc<Mutex<LifecycleGate>>,
    permissions: Arc<dyn AudioPermissions>,
    output: Arc<dyn AudioOutputFactory>,
}

/// Opaque cloneable session audio authority passed to script runtimes.
///
/// Clones share one application host, one exact session generation and its
/// playback gate. They cannot retire the mixer session or access Native/Speech
/// buses. `OfflineAudioContext` is still governed by shared and per-isolate
/// quotas but is device-free and does not consult lifecycle permissions in the
/// pinned `deno_audio` revision.
#[derive(Clone)]
pub struct SessionAudioScope {
    inner: Arc<SessionAudioScopeInner>,
}

impl fmt::Debug for SessionAudioScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionAudioScope")
            .field("session_id", &self.inner.session_id)
            .field("generation", &self.inner.generation)
            .finish_non_exhaustive()
    }
}

impl SessionAudioScope {
    /// Stable numeric session id used by core before thread publication.
    #[must_use]
    pub fn session_id(&self) -> u64 {
        self.inner.session_id.0
    }

    /// Builds fresh isolate options over this scope's unchanged authorities.
    #[must_use]
    pub fn extension_options(&self) -> AudioExtensionOptions {
        AudioExtensionOptions::new(Arc::clone(&self.inner.app.host))
            .permissions(Arc::clone(&self.inner.permissions))
            .output_factory(Arc::clone(&self.inner.output))
    }
}

/// Unique lifetime owner for one registered mixer-session generation.
pub struct SessionAudioRegistration {
    owner: Option<MixerSessionOwner>,
    native: MixerNativeBusHandle,
    speech: MixerSpeechBusHandle,
    scope: SessionAudioScope,
}

impl fmt::Debug for SessionAudioRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionAudioRegistration")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl SessionAudioRegistration {
    /// Cloneable opaque authority for this exact session generation.
    #[must_use]
    pub fn scope(&self) -> SessionAudioScope {
        self.scope.clone()
    }

    /// Exact Native-bus authority retained for the UI/application owner.
    #[must_use]
    pub fn native_bus(&self) -> MixerNativeBusHandle {
        self.native.clone()
    }

    /// Exact Speech-bus authority retained for the UI/application owner.
    #[must_use]
    pub fn speech_bus(&self) -> MixerSpeechBusHandle {
        self.speech.clone()
    }

    /// Absorbingly rejects new online output preparation for this session.
    ///
    /// The application gate is acquired before the session gate, matching the
    /// permission and final factory checks. Returning therefore proves that an
    /// already-admitted `prepare` delegation has finished and no later one can
    /// begin. The mixer-session owner remains retained for explicit runtime
    /// join followed by [`retire`](Self::retire).
    ///
    /// Returns `true` only for the transition from open to sealed.
    pub fn seal(&mut self) -> bool {
        self.seal_session()
    }

    /// Seals online preparation before beginning exact mixer retirement.
    ///
    /// # Panics
    ///
    /// Panics only if the registration's private unique-owner invariant was
    /// corrupted inside this crate.
    #[must_use = "session retirement is complete only after awaiting this receipt"]
    pub fn retire(mut self) -> MixerSessionRetirement {
        let _ = self.seal();
        self.owner
            .take()
            .expect("session audio registration retires exactly once")
            .retire()
    }

    fn seal_session(&self) -> bool {
        let _app_gate = lock_gate(&self.scope.inner.app.gate);
        let mut session_gate = lock_gate(&self.scope.inner.session_gate);
        let was_open = session_gate.open;
        session_gate.open = false;
        was_open
    }
}

impl Drop for SessionAudioRegistration {
    fn drop(&mut self) {
        let _ = self.seal();
        // MixerSessionOwner::drop begins cancellation-independent retirement.
        self.owner.take();
    }
}

/// Unique mixer-free lifetime owner for one unavailable session generation.
pub struct UnavailableSessionAudioRegistration {
    scope: SessionAudioScope,
}

impl fmt::Debug for UnavailableSessionAudioRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnavailableSessionAudioRegistration")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Exact logical close receipt for a session that never owned mixer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnavailableSessionAudioRetirement {
    session_id: u64,
    generation: u64,
}

impl UnavailableSessionAudioRetirement {
    /// The exact mixer-free session identity whose admission was sealed.
    #[must_use]
    pub const fn session_id(self) -> u64 {
        self.session_id
    }

    /// Exact non-wrapping registration generation that was sealed.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

impl UnavailableSessionAudioRegistration {
    /// Cloneable opaque authority for this exact mixer-free generation.
    #[must_use]
    pub fn scope(&self) -> SessionAudioScope {
        self.scope.clone()
    }

    /// Absorbingly rejects new online output preparation for this session.
    pub fn seal(&mut self) -> bool {
        self.seal_session()
    }

    /// Seals this exact mixer-free generation and returns its logical receipt.
    #[must_use]
    pub fn retire(mut self) -> UnavailableSessionAudioRetirement {
        let _ = self.seal();
        UnavailableSessionAudioRetirement {
            session_id: self.scope.session_id(),
            generation: self.scope.inner.generation,
        }
    }

    fn seal_session(&self) -> bool {
        let _app_gate = lock_gate(&self.scope.inner.app.gate);
        let mut session_gate = lock_gate(&self.scope.inner.session_gate);
        let was_open = session_gate.open;
        session_gate.open = false;
        was_open
    }
}

impl Drop for UnavailableSessionAudioRegistration {
    fn drop(&mut self) {
        let _ = self.seal();
    }
}

struct SessionAudioPermissions {
    app: Arc<ApplicationAudioState>,
    session: Arc<Mutex<LifecycleGate>>,
}

impl AudioPermissions for SessionAudioPermissions {
    fn check_playback(&self, api_name: &'static str) -> Result<(), JsErrorBox> {
        let app = lock_gate(&self.app.gate);
        let session = lock_gate(&self.session);
        if app.open && session.open {
            Ok(())
        } else {
            Err(JsErrorBox::generic(format!(
                "InvalidStateError: {api_name} playback admission is closed"
            )))
        }
    }

    fn check_capture(&self, api_name: &'static str) -> Result<(), JsErrorBox> {
        Err(JsErrorBox::generic(format!(
            "NotAllowedError: {api_name} audio capture is not permitted"
        )))
    }
}

struct GatedSessionAudioOutputFactory {
    app: Arc<ApplicationAudioState>,
    session: Arc<Mutex<LifecycleGate>>,
    delegate: SessionAudioOutputFactory,
}

struct GatedUnavailableAudioOutputFactory {
    app: Arc<ApplicationAudioState>,
    session: Arc<Mutex<LifecycleGate>>,
    delegate: UnavailableSessionAudioOutputFactory,
}

impl AudioOutputFactory for GatedUnavailableAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let app = lock_gate(&self.app.gate);
        let session = lock_gate(&self.session);
        if !app.open || !session.open {
            return Err(output_error(
                AudioOutputErrorKind::Shutdown,
                "Smudgy Web Audio playback admission is closed",
            ));
        }
        #[cfg(test)]
        if let Some(hook) = self
            .app
            .prepare_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            hook.entered.wait();
            hook.release.wait();
        }
        self.delegate.prepare(request)
    }
}

/// Hardware-neutral diagnostic retained by a mixer-free default-sink route.
#[derive(Clone, Debug)]
pub struct UnavailableAudioOutputCause(Arc<str>);

impl UnavailableAudioOutputCause {
    #[must_use]
    pub fn new(detail: impl Into<Arc<str>>) -> Self {
        Self(detail.into())
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
struct UnavailableSessionAudioOutputFactory {
    cause: UnavailableAudioOutputCause,
    silent: SystemAudioOutput,
}

impl UnavailableSessionAudioOutputFactory {
    const fn new(cause: UnavailableAudioOutputCause) -> Self {
        Self {
            cause,
            silent: SystemAudioOutput::new(),
        }
    }
}

impl AudioOutputFactory for UnavailableSessionAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        match request.sink_id() {
            "none" => self.silent.prepare(request),
            "" => Err(AudioOutputError::new(
                AudioOutputErrorKind::DeviceUnavailable,
                format!(
                    "Smudgy physical audio is unavailable until restart: {}",
                    self.cause.detail()
                ),
            )),
            _ => Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "Smudgy Web Audio supports only the default and none output sinks",
            )),
        }
    }
}

impl AudioOutputFactory for GatedSessionAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let app = lock_gate(&self.app.gate);
        let session = lock_gate(&self.session);
        if !app.open || !session.open {
            return Err(output_error(
                AudioOutputErrorKind::Shutdown,
                "Smudgy Web Audio playback admission is closed",
            ));
        }
        #[cfg(test)]
        if let Some(hook) = self
            .app
            .prepare_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            hook.entered.wait();
            hook.release.wait();
        }
        // Keep both guards across the last check and delegate preparation so
        // application/session sealing cannot pass the permission check and
        // race mixer mutation.
        self.delegate.prepare(request)
    }
}

#[cfg(test)]
struct TestPrepareHook {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

/// Routes one session's Web Audio contexts to its shared Script bus or to a
/// private silent endpoint.
///
/// The default sink consumes one bounded Script-bus input. The exact `"none"`
/// sink delegates to [`SystemAudioOutput`]'s joinable silent implementation and
/// never mutates mixer capacity. Other sink identifiers are rejected before
/// either delegate is invoked.
#[derive(Clone, Debug)]
pub struct SessionAudioOutputFactory {
    script: ScriptBusAudioOutputFactory,
    silent: SystemAudioOutput,
}

impl SessionAudioOutputFactory {
    /// Binds context construction to one session's scoped Script bus.
    #[must_use]
    pub const fn new(bus: MixerScriptBusHandle) -> Self {
        Self {
            script: ScriptBusAudioOutputFactory::new(bus),
            silent: SystemAudioOutput::new(),
        }
    }
}

impl AudioOutputFactory for SessionAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        match request.sink_id() {
            "" => self.script.prepare(request),
            "none" => self.silent.prepare(request),
            _ => Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "Smudgy Web Audio supports only the default and none output sinks",
            )),
        }
    }
}

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
