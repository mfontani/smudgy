//! Optional direct system-output adapter.
//!
//! CPAL and the generic driver seam stay private. The public wrapper exposes
//! only Smudgy-owned formats, handles, and stable error classifications.

use std::{
    cell::UnsafeCell,
    fmt,
    mem::ManuallyDrop,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use cpal::{
    BufferSize, SampleFormat, StreamConfig, SupportedBufferSize,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use super::{
    AudioSessionId, DriverFailureSignal, JoinedOutputDriver, JoinedRenderer,
    MAX_PHYSICAL_CALLBACK_FRAMES, MixerControlError, MixerFormat, MixerOutputFailure, MixerService,
    MixerSessionOwner, MixerSessionRegistrar, MixerShutdown, MixerStartError, PhysicalOutputFormat,
    PhysicalSampleFormat,
};

const PHYSICAL_CHANNELS: usize = 2;
const PHYSICAL_SCRATCH_SAMPLES: usize = MAX_PHYSICAL_CALLBACK_FRAMES * PHYSICAL_CHANNELS;
const CALLBACK_RETIREMENT_POLL: Duration = Duration::from_millis(1);
static SYSTEM_OUTPUT_LEASED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
struct TestProofHook {
    before_unwrap_entered: std::sync::mpsc::SyncSender<()>,
    before_unwrap_release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    unwrap_attempted: std::sync::mpsc::SyncSender<bool>,
    unwrap_notified: AtomicBool,
    late_upgrade_entered: std::sync::mpsc::SyncSender<()>,
    late_upgrade_release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

#[cfg(test)]
impl TestProofHook {
    fn wait(gate: &(std::sync::Mutex<bool>, std::sync::Condvar)) {
        let (lock, ready) = gate;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = ready.wait(released).unwrap();
        }
    }

    fn before_unwrap(&self) {
        let _ = self.before_unwrap_entered.send(());
        Self::wait(&self.before_unwrap_release);
    }

    fn after_unwrap(&self, proven: bool) {
        if self
            .unwrap_notified
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.unwrap_attempted.send(proven);
        }
    }

    fn after_late_upgrade(&self) {
        let _ = self.late_upgrade_entered.send(());
        Self::wait(&self.late_upgrade_release);
    }
}

/// Stable stage at which direct system-output setup failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemOutputOperation {
    /// Acquire the process lease or create the native host.
    Setup,
    /// Find the default output device.
    Enumerate,
    /// Select an exact supported device configuration.
    Plan,
    /// Build the physical stream.
    Build,
    /// Start the physical stream.
    Play,
    /// Retire a native object or stream.
    Drop,
}

/// Stable, CPAL-independent category for a system-output failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemOutputErrorKind {
    /// No usable default output device or host is currently available.
    DeviceUnavailable,
    /// The process output is already leased or the default device is busy.
    OutputInUse,
    /// The default device has no exact supported mixer format.
    UnsupportedFormat,
    /// The platform output backend failed.
    BackendFailure,
    /// The backend violated the bounded callback or ownership protocol.
    Protocol,
}

/// Stable CPAL-free system-output error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemOutputError {
    kind: SystemOutputErrorKind,
    operation: SystemOutputOperation,
    detail: String,
}

impl SystemOutputError {
    fn new(
        kind: SystemOutputErrorKind,
        operation: SystemOutputOperation,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            operation,
            detail: detail.into(),
        }
    }

    /// Stable failure category.
    #[must_use]
    pub const fn kind(&self) -> SystemOutputErrorKind {
        self.kind
    }

    /// Stable operation that failed.
    #[must_use]
    pub const fn operation(&self) -> SystemOutputOperation {
        self.operation
    }

    /// Human-readable platform detail with no stable parsing contract.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for SystemOutputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "system output {:?} failed ({:?}): {}",
            self.operation, self.kind, self.detail
        )
    }
}

impl std::error::Error for SystemOutputError {}

/// Failure returned while starting [`SystemMixerService`].
pub type SystemMixerStartError = MixerStartError<SystemOutputError>;

/// Exact, cloneable cause for a physical mixer that was unavailable during
/// application startup.
///
/// This value is diagnostic only. In particular,
/// [`MixerStartError::CleanupUncertain`] preserves the fact that the failed
/// physical startup did not prove cleanup; it must never be interpreted as a
/// successful physical shutdown or retried in-process.
#[derive(Debug, Clone)]
pub struct SystemMixerUnavailable(Arc<SystemMixerStartError>);

impl SystemMixerUnavailable {
    /// Returns the exact failed startup result, including platform source and
    /// any cleanup-uncertain primary cause.
    #[must_use]
    pub fn error(&self) -> &SystemMixerStartError {
        &self.0
    }

    /// Whether the failed startup reported proven physical cleanup.
    #[must_use]
    pub fn cleanup_proven(&self) -> bool {
        !matches!(
            &*self.0,
            MixerStartError::CleanupUncertain(_) | MixerStartError::OwnerStopped
        )
    }
}

impl fmt::Display for SystemMixerUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for SystemMixerUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error())
    }
}

impl From<SystemMixerStartError> for SystemMixerUnavailable {
    fn from(error: SystemMixerStartError) -> Self {
        Self(Arc::new(error))
    }
}

/// One non-generic process system-output service.
///
/// The value is thread-affine because the contained mixer service is
/// thread-affine. Only the default output device is considered.
/// [`shutdown`](Self::shutdown) is the proof-bearing joined path. Ordinary
/// `Drop` requests autonomous cleanup but cannot report or synchronously prove
/// its outcome; an uncertain cleanup deliberately retains the process lease.
pub struct SystemMixerService(MixerService);

impl fmt::Debug for SystemMixerService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SystemMixerService")
            .field("format", &self.format())
            .field("physical_output_format", &self.physical_output_format())
            .finish_non_exhaustive()
    }
}

impl SystemMixerService {
    /// Start the sole process mixer on the default physical output device.
    ///
    /// # Errors
    ///
    /// Returns a stable mixer or system-output startup failure. An uncertain
    /// prior retirement deliberately retains the process lease. An active
    /// service, or an ordinarily dropped service still retiring on its owner,
    /// returns [`SystemOutputErrorKind::OutputInUse`].
    pub fn start(sample_rate: u32) -> Result<Self, SystemMixerStartError> {
        let lease =
            OutputLease::acquire(&SYSTEM_OUTPUT_LEASED).map_err(MixerStartError::Backend)?;
        MixerService::start_with_driver::<CpalOutputDriver<SystemHostFactory>>(
            SystemDriverSettings {
                factory: SystemHostFactory,
                lease,
                sample_rate,
                #[cfg(test)]
                proof_hook: None,
            },
            sample_rate,
        )
        .map(Self)
    }

    /// Verified logical mixer format.
    #[must_use]
    pub fn format(&self) -> MixerFormat {
        self.0.format()
    }

    /// Exact selected physical output format and buffer hint.
    #[must_use]
    pub fn physical_output_format(&self) -> PhysicalOutputFormat {
        self.0.physical_output_format()
    }

    /// Returns a weak, cloneable session-registration authority.
    ///
    /// The registrar never owns this thread-affine service or its unique
    /// physical-output join authority.
    #[must_use]
    pub fn session_registrar(&self) -> MixerSessionRegistrar {
        self.0.session_registrar()
    }

    /// Add one fully preinstalled session subtree.
    ///
    /// # Errors
    ///
    /// Returns a bounded control or topology error.
    pub fn add_session(&self, id: AudioSessionId) -> Result<MixerSessionOwner, MixerControlError> {
        self.0.add_session(id)
    }

    /// Seal production and join the system-output owner.
    #[must_use]
    pub fn shutdown(self) -> MixerShutdown {
        self.0.shutdown()
    }
}

struct OutputLease {
    flag: &'static AtomicBool,
}

impl OutputLease {
    fn acquire(flag: &'static AtomicBool) -> Result<Self, SystemOutputError> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                SystemOutputError::new(
                    SystemOutputErrorKind::OutputInUse,
                    SystemOutputOperation::Setup,
                    "the process system output is already leased",
                )
            })?;
        Ok(Self { flag })
    }
}

impl Drop for OutputLease {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone)]
struct HostFailure {
    kind: SystemOutputErrorKind,
    detail: String,
}

impl HostFailure {
    fn new(kind: SystemOutputErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

trait HostFactory: Send + 'static {
    type Host: OutputHost;

    fn create(self) -> Result<Self::Host, HostFailure>;
}

trait OutputHost {
    type Device: OutputDevice;

    fn default_output_device(&self) -> Result<Option<Self::Device>, HostFailure>;
}

trait OutputDevice: 'static {
    type Stream: OutputStream;

    fn supported_output_configs(&self) -> Result<Vec<OutputConfigRange>, HostFailure>;
    fn build_output_stream(
        &self,
        config: OutputStreamConfig,
        data: OutputDataCallback,
        error: OutputErrorCallback,
    ) -> Result<Self::Stream, HostFailure>;
}

trait OutputStream: 'static {
    fn play(&self) -> Result<(), HostFailure>;
}

type OutputDataCallback = Box<dyn for<'a> FnMut(Option<OutputBuffer<'a>>) + Send + 'static>;
type OutputErrorCallback = Box<dyn FnMut() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverSampleFormat {
    F32,
    I16,
    U16,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputBufferRange {
    Range { min: usize, max: usize },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputConfigRange {
    channels: usize,
    min_sample_rate: u32,
    max_sample_rate: u32,
    sample_format: DriverSampleFormat,
    buffer_size: OutputBufferRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputBufferRequest {
    Default,
    Fixed(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputStreamConfig {
    channels: usize,
    sample_rate: u32,
    sample_format: DriverSampleFormat,
    buffer_size: OutputBufferRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputPlan {
    stream: OutputStreamConfig,
    physical: PhysicalOutputFormat,
}

fn plan_output(
    ranges: impl IntoIterator<Item = OutputConfigRange>,
    sample_rate: u32,
) -> Result<OutputPlan, HostFailure> {
    let mut selected: Option<(u8, OutputPlan)> = None;
    for range in ranges {
        if range.channels != PHYSICAL_CHANNELS
            || sample_rate < range.min_sample_rate
            || sample_rate > range.max_sample_rate
        {
            continue;
        }
        let (rank, physical_sample_format) = match range.sample_format {
            DriverSampleFormat::F32 => (0, PhysicalSampleFormat::F32),
            DriverSampleFormat::I16 => (1, PhysicalSampleFormat::I16),
            DriverSampleFormat::U16 => (2, PhysicalSampleFormat::U16),
            DriverSampleFormat::Other => continue,
        };
        let (buffer_size, buffer_frames_hint) = match range.buffer_size {
            OutputBufferRange::Unknown => (OutputBufferRequest::Default, None),
            OutputBufferRange::Range { min, max }
                if min > 0 && min <= max && min <= MAX_PHYSICAL_CALLBACK_FRAMES =>
            {
                let frames = super::INTERNAL_BUFFER_FRAMES.clamp(min, max);
                (OutputBufferRequest::Fixed(frames), Some(frames))
            }
            OutputBufferRange::Range { .. } => continue,
        };
        let plan = OutputPlan {
            stream: OutputStreamConfig {
                channels: PHYSICAL_CHANNELS,
                sample_rate,
                sample_format: range.sample_format,
                buffer_size,
            },
            physical: PhysicalOutputFormat {
                sample_rate,
                channels: PHYSICAL_CHANNELS,
                sample_format: physical_sample_format,
                buffer_frames_hint,
            },
        };
        if selected.as_ref().is_none_or(|(best, _)| rank < *best) {
            selected = Some((rank, plan));
        }
    }
    selected.map(|(_, plan)| plan).ok_or_else(|| {
        HostFailure::new(
            SystemOutputErrorKind::UnsupportedFormat,
            "the default device has no exact stereo PCM configuration at the requested rate",
        )
    })
}

enum OutputBuffer<'a> {
    F32(&'a mut [f32]),
    I16(&'a mut [i16]),
    U16(&'a mut [u16]),
}

impl OutputBuffer<'_> {
    fn silence(&mut self) {
        match self {
            Self::F32(output) => output.fill(0.0),
            Self::I16(output) => output.fill(0),
            Self::U16(output) => output.fill(1 << 15),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::F32(output) => output.len(),
            Self::I16(output) => output.len(),
            Self::U16(output) => output.len(),
        }
    }

    fn sample_format(&self) -> DriverSampleFormat {
        match self {
            Self::F32(_) => DriverSampleFormat::F32,
            Self::I16(_) => DriverSampleFormat::I16,
            Self::U16(_) => DriverSampleFormat::U16,
        }
    }

    fn copy_from_f32(&mut self, source: &[f32]) {
        match self {
            Self::F32(output) => output.copy_from_slice(source),
            Self::I16(output) => {
                for (output, sample) in output.iter_mut().zip(source.iter().copied()) {
                    *output = cpal::Sample::from_sample(sample);
                }
            }
            Self::U16(output) => {
                for (output, sample) in output.iter_mut().zip(source.iter().copied()) {
                    *output = cpal::Sample::from_sample(sample);
                }
            }
        }
    }
}

struct CallbackState {
    revoked: AtomicBool,
    active: AtomicUsize,
    renderer: UnsafeCell<Option<JoinedRenderer>>,
    scratch: UnsafeCell<Box<[f32]>>,
    failures: DriverFailureSignal,
    sample_format: DriverSampleFormat,
    #[cfg(test)]
    proof_hook: Option<Arc<TestProofHook>>,
}

// SAFETY: callback entry is exclusive via `active`. The owner accesses the
// cells only before publishing a stream or after revocation, stream destruction,
// zero active callbacks, and unique strong ownership.
unsafe impl Sync for CallbackState {}

impl CallbackState {
    fn new(
        failures: DriverFailureSignal,
        sample_format: DriverSampleFormat,
        #[cfg(test)] proof_hook: Option<Arc<TestProofHook>>,
    ) -> Self {
        Self {
            revoked: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            renderer: UnsafeCell::new(None),
            scratch: UnsafeCell::new(vec![0.0; PHYSICAL_SCRATCH_SAMPLES].into_boxed_slice()),
            failures,
            sample_format,
            #[cfg(test)]
            proof_hook,
        }
    }

    fn install_renderer(&self, renderer: JoinedRenderer) -> bool {
        if self.revoked.load(Ordering::Acquire) || self.active.load(Ordering::Acquire) != 0 {
            return false;
        }
        // SAFETY: no stream exists yet, so the owner has exclusive access.
        let slot = unsafe { &mut *self.renderer.get() };
        if slot.is_some() {
            return false;
        }
        *slot = Some(renderer);
        true
    }

    fn enter(&self) -> Option<ActiveCallback<'_>> {
        if self.revoked.load(Ordering::Acquire) {
            return None;
        }
        if self
            .active
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.failures.report(MixerOutputFailure::BackendFailure);
            return None;
        }
        if self.revoked.load(Ordering::Acquire) {
            self.active.store(0, Ordering::Release);
            return None;
        }
        Some(ActiveCallback {
            active: &self.active,
        })
    }

    fn render_silenced(&self, output: &mut OutputBuffer<'_>) {
        let samples = output.len();
        if output.sample_format() != self.sample_format
            || samples == 0
            || !samples.is_multiple_of(PHYSICAL_CHANNELS)
            || samples > PHYSICAL_SCRATCH_SAMPLES
        {
            self.failures
                .report(MixerOutputFailure::InvalidCallbackGeometry);
            return;
        }
        // SAFETY: the active-entry CAS provides sole callback access.
        let renderer = unsafe { &mut *self.renderer.get() };
        let Some(renderer) = renderer.as_mut() else {
            self.failures.report(MixerOutputFailure::BackendFailure);
            return;
        };
        // SAFETY: the active-entry CAS provides sole callback access.
        let scratch = unsafe { &mut *self.scratch.get() };
        let scratch = &mut scratch[..samples];
        if renderer.render(scratch, PHYSICAL_CHANNELS).is_ok() {
            output.copy_from_f32(scratch);
        }
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn pause_after_late_upgrade(&self) {
        if let Some(hook) = self.proof_hook.as_ref() {
            hook.after_late_upgrade();
        }
    }
}

struct ActiveCallback<'a> {
    active: &'a AtomicUsize,
}

impl Drop for ActiveCallback<'_> {
    fn drop(&mut self) {
        self.active.store(0, Ordering::Release);
    }
}

fn callback_for(state: &Arc<CallbackState>) -> OutputDataCallback {
    let state = Arc::downgrade(state);
    Box::new(move |output| {
        let Some(mut output) = output else {
            if let Some(state) = state.upgrade() {
                state
                    .failures
                    .report(MixerOutputFailure::InvalidCallbackGeometry);
            }
            return;
        };
        // Establish the correct raw-format equilibrium before any fallible
        // operation or state upgrade.
        output.silence();
        let Some(state) = state.upgrade() else {
            return;
        };
        #[cfg(test)]
        state.pause_after_late_upgrade();
        let Some(_active) = state.enter() else {
            return;
        };
        let rendered = catch_unwind(AssertUnwindSafe(|| state.render_silenced(&mut output)));
        if let Err(payload) = rendered {
            output.silence();
            state.failures.report(MixerOutputFailure::RendererPanicked);
            std::mem::forget(payload);
        }
    })
}

fn error_callback_for(signal: &DriverFailureSignal) -> OutputErrorCallback {
    let signal = signal.clone();
    Box::new(move || {
        // Runtime platform details are intentionally not allocated or
        // transported from this callback. Classification is conservative.
        signal.report(MixerOutputFailure::BackendFailure);
    })
}

struct SystemDriverSettings<F> {
    factory: F,
    lease: OutputLease,
    sample_rate: u32,
    #[cfg(test)]
    proof_hook: Option<Arc<TestProofHook>>,
}

struct CpalOutputDriver<F: HostFactory> {
    device: Option<ManuallyDrop<<F::Host as OutputHost>::Device>>,
    stream: Option<ManuallyDrop<<<F::Host as OutputHost>::Device as OutputDevice>::Stream>>,
    plan: OutputPlan,
    callback: ManuallyDrop<Option<Arc<CallbackState>>>,
    retired_callback: ManuallyDrop<Option<CallbackState>>,
    lease: ManuallyDrop<OutputLease>,
    retired: bool,
    cleanup_uncertain: bool,
}

fn system_error(operation: SystemOutputOperation, failure: HostFailure) -> SystemOutputError {
    SystemOutputError::new(failure.kind, operation, failure.detail)
}

fn contain_host_call<T>(
    operation: SystemOutputOperation,
    call: impl FnOnce() -> Result<T, HostFailure>,
) -> Result<T, SystemOutputError> {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(result) => result.map_err(|error| system_error(operation, error)),
        Err(payload) => {
            std::mem::forget(payload);
            Err(SystemOutputError::new(
                SystemOutputErrorKind::BackendFailure,
                operation,
                "the native audio backend unwound",
            ))
        }
    }
}

fn drop_owned<T>(
    owned: &mut Option<ManuallyDrop<T>>,
    detail: &'static str,
) -> Result<(), SystemOutputError> {
    let Some(mut value) = owned.take() else {
        return Ok(());
    };
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        ManuallyDrop::drop(&mut value);
    })) {
        Ok(()) => Ok(()),
        Err(payload) => {
            std::mem::forget(payload);
            Err(SystemOutputError::new(
                SystemOutputErrorKind::BackendFailure,
                SystemOutputOperation::Drop,
                detail,
            ))
        }
    }
}

fn mark_cleanup_uncertain(failures: &DriverFailureSignal) {
    if let Some(status) = failures.0.upgrade() {
        status.finish_retirement(false);
    }
}

fn cleanup_failed_setup<H, D>(
    host: &mut Option<ManuallyDrop<H>>,
    device: &mut Option<ManuallyDrop<D>>,
    lease: &mut Option<ManuallyDrop<OutputLease>>,
    failures: &DriverFailureSignal,
    cause: SystemOutputError,
    mut uncertain: bool,
) -> SystemOutputError {
    for result in [
        drop_owned(device, "the native output device destructor unwound"),
        drop_owned(host, "the native audio host destructor unwound"),
    ] {
        if result.is_err() {
            uncertain = true;
        }
    }
    if uncertain {
        mark_cleanup_uncertain(failures);
    } else if let Err(error) = drop_owned(lease, "the process output lease destructor unwound") {
        mark_cleanup_uncertain(failures);
        return error;
    }
    cause
}

impl<F: HostFactory> JoinedOutputDriver for CpalOutputDriver<F> {
    type Settings = SystemDriverSettings<F>;
    type Error = SystemOutputError;

    #[allow(
        clippy::too_many_lines,
        reason = "setup spells out every staged host/device cleanup edge and ownership transfer"
    )]
    fn setup(
        settings: Self::Settings,
        _internal_buffer_size: usize,
        failures: DriverFailureSignal,
    ) -> Result<(Self, PhysicalOutputFormat), Self::Error> {
        let SystemDriverSettings {
            factory,
            lease,
            sample_rate,
            #[cfg(test)]
            proof_hook,
        } = settings;
        let mut lease = Some(ManuallyDrop::new(lease));
        let mut host = None;
        let mut device = None;
        match contain_host_call(SystemOutputOperation::Setup, || factory.create()) {
            Ok(value) => host = Some(ManuallyDrop::new(value)),
            Err(cause) => {
                return Err(cleanup_failed_setup::<
                    F::Host,
                    <F::Host as OutputHost>::Device,
                >(
                    &mut host,
                    &mut device,
                    &mut lease,
                    &failures,
                    cause,
                    false,
                ));
            }
        }
        let selected = contain_host_call(SystemOutputOperation::Enumerate, || {
            host.as_ref()
                .expect("host is staged during enumeration")
                .default_output_device()
        });
        match selected {
            Ok(Some(value)) => device = Some(ManuallyDrop::new(value)),
            Ok(None) => {
                let cause = SystemOutputError::new(
                    SystemOutputErrorKind::DeviceUnavailable,
                    SystemOutputOperation::Enumerate,
                    "the native audio host has no default output device",
                );
                return Err(cleanup_failed_setup(
                    &mut host,
                    &mut device,
                    &mut lease,
                    &failures,
                    cause,
                    false,
                ));
            }
            Err(cause) => {
                return Err(cleanup_failed_setup(
                    &mut host,
                    &mut device,
                    &mut lease,
                    &failures,
                    cause,
                    false,
                ));
            }
        }
        let ranges = match contain_host_call(SystemOutputOperation::Plan, || {
            device
                .as_ref()
                .expect("device is staged during planning")
                .supported_output_configs()
        }) {
            Ok(ranges) => ranges,
            Err(cause) => {
                return Err(cleanup_failed_setup(
                    &mut host,
                    &mut device,
                    &mut lease,
                    &failures,
                    cause,
                    false,
                ));
            }
        };
        let plan = match plan_output(ranges, sample_rate)
            .map_err(|error| system_error(SystemOutputOperation::Plan, error))
        {
            Ok(plan) => plan,
            Err(cause) => {
                return Err(cleanup_failed_setup(
                    &mut host,
                    &mut device,
                    &mut lease,
                    &failures,
                    cause,
                    false,
                ));
            }
        };
        if let Err(cause) = drop_owned(&mut host, "the native audio host destructor unwound") {
            return Err(cleanup_failed_setup(
                &mut host,
                &mut device,
                &mut lease,
                &failures,
                cause,
                true,
            ));
        }
        let physical = plan.physical;
        Ok((
            Self {
                device,
                stream: None,
                plan,
                callback: ManuallyDrop::new(Some(Arc::new(CallbackState::new(
                    failures,
                    plan.stream.sample_format,
                    #[cfg(test)]
                    proof_hook,
                )))),
                retired_callback: ManuallyDrop::new(None),
                lease: lease
                    .take()
                    .expect("lease transfers into the published driver"),
                retired: false,
                cleanup_uncertain: false,
            },
            physical,
        ))
    }

    fn start(&mut self, renderer: JoinedRenderer) -> Result<(), Self::Error> {
        let callback = self.callback.as_ref().ok_or_else(|| {
            SystemOutputError::new(
                SystemOutputErrorKind::Protocol,
                SystemOutputOperation::Build,
                "the physical callback state was already retired",
            )
        })?;
        if !callback.install_renderer(renderer) {
            return Err(SystemOutputError::new(
                SystemOutputErrorKind::Protocol,
                SystemOutputOperation::Build,
                "the physical callback renderer was already installed or revoked",
            ));
        }
        let device = self.device.as_ref().ok_or_else(|| {
            SystemOutputError::new(
                SystemOutputErrorKind::Protocol,
                SystemOutputOperation::Build,
                "the output device was already retired",
            )
        })?;
        let stream = contain_host_call(SystemOutputOperation::Build, || {
            device.build_output_stream(
                self.plan.stream,
                callback_for(callback),
                error_callback_for(&callback.failures),
            )
        })?;
        self.stream = Some(ManuallyDrop::new(stream));
        self.drop_device()?;
        Ok(())
    }

    fn play(&mut self) -> Result<(), Self::Error> {
        let stream = self.stream.as_ref().ok_or_else(|| {
            SystemOutputError::new(
                SystemOutputErrorKind::Protocol,
                SystemOutputOperation::Play,
                "the output stream was not built",
            )
        })?;
        contain_host_call(SystemOutputOperation::Play, || stream.play())
    }

    fn close_and_join(&mut self) -> bool {
        let Some(callback) = self.callback.as_ref() else {
            return false;
        };
        callback.revoke();
        if let Some(mut stream) = self.stream.take() {
            let dropped = catch_unwind(AssertUnwindSafe(|| unsafe {
                ManuallyDrop::drop(&mut stream);
            }));
            if let Err(payload) = dropped {
                std::mem::forget(payload);
                self.callback
                    .as_ref()
                    .expect("callback remains owned after stream drop panic")
                    .failures
                    .report(MixerOutputFailure::BackendFailure);
                return false;
            }
        }
        if self.drop_device().is_err() || self.cleanup_uncertain {
            self.callback
                .as_ref()
                .expect("callback remains owned after device drop panic")
                .failures
                .report(MixerOutputFailure::BackendFailure);
            return false;
        }
        #[cfg(test)]
        if let Some(hook) = self
            .callback
            .as_ref()
            .expect("callback remains owned until atomic unwrap")
            .proof_hook
            .as_ref()
        {
            hook.before_unwrap();
        }
        let callback = self
            .callback
            .take()
            .expect("callback state is present until retirement proof");
        let callback = {
            let mut callback = callback;
            loop {
                match Arc::try_unwrap(callback) {
                    Ok(callback) => break callback,
                    Err(returned) => {
                        #[cfg(test)]
                        if let Some(hook) = returned.proof_hook.as_ref() {
                            hook.after_unwrap(false);
                        }
                        callback = returned;
                        // Successful stream destruction prevents new native
                        // callback entry. A preexisting strong callback owner
                        // is proof-bearing work, not a wall-clock failure, so
                        // shutdown waits without spinning or inventing a
                        // timeout-based quarantine.
                        thread::sleep(CALLBACK_RETIREMENT_POLL);
                    }
                }
            }
        };
        #[cfg(test)]
        if let Some(hook) = callback.proof_hook.as_ref() {
            hook.after_unwrap(true);
        }
        *self.retired_callback = Some(callback);
        self.retired = true;
        true
    }
}

impl<F: HostFactory> CpalOutputDriver<F> {
    fn drop_device(&mut self) -> Result<(), SystemOutputError> {
        let result = drop_owned(
            &mut self.device,
            "the native output device destructor unwound",
        );
        if result.is_err() {
            self.cleanup_uncertain = true;
        }
        result
    }
}

impl<F: HostFactory> Drop for CpalOutputDriver<F> {
    fn drop(&mut self) {
        if !self.retired {
            // `MonitoredBackend` retains this entire driver on an unproven
            // close, so this path is only reachable after proof.
            return;
        }
        let callback = self.retired_callback.take();
        let callback_dropped = catch_unwind(AssertUnwindSafe(|| drop(callback)));
        if let Err(payload) = callback_dropped {
            std::mem::forget(payload);
            panic!("system callback state destructor unwound after retirement proof");
        }
        // SAFETY: the stream is destroyed and callback authority is gone.
        unsafe { ManuallyDrop::drop(&mut self.lease) };
    }
}

#[derive(Clone, Copy)]
struct SystemHostFactory;

struct SystemHost(cpal::Host);
struct SystemDevice(cpal::Device);
struct SystemStream(cpal::Stream);

impl HostFactory for SystemHostFactory {
    type Host = SystemHost;

    fn create(self) -> Result<Self::Host, HostFailure> {
        Ok(SystemHost(cpal::default_host()))
    }
}

impl OutputHost for SystemHost {
    type Device = SystemDevice;

    fn default_output_device(&self) -> Result<Option<Self::Device>, HostFailure> {
        Ok(self.0.default_output_device().map(SystemDevice))
    }
}

impl OutputDevice for SystemDevice {
    type Stream = SystemStream;

    fn supported_output_configs(&self) -> Result<Vec<OutputConfigRange>, HostFailure> {
        let configs = self
            .0
            .supported_output_configs()
            .map_err(|error| host_failure_from_cpal(&error))?;
        Ok(configs
            .map(|config| OutputConfigRange {
                channels: usize::from(config.channels()),
                min_sample_rate: config.min_sample_rate(),
                max_sample_rate: config.max_sample_rate(),
                sample_format: match config.sample_format() {
                    SampleFormat::F32 => DriverSampleFormat::F32,
                    SampleFormat::I16 => DriverSampleFormat::I16,
                    SampleFormat::U16 => DriverSampleFormat::U16,
                    _ => DriverSampleFormat::Other,
                },
                buffer_size: match *config.buffer_size() {
                    SupportedBufferSize::Range { min, max } => OutputBufferRange::Range {
                        min: min as usize,
                        max: max as usize,
                    },
                    SupportedBufferSize::Unknown => OutputBufferRange::Unknown,
                },
            })
            .collect())
    }

    fn build_output_stream(
        &self,
        config: OutputStreamConfig,
        mut data: OutputDataCallback,
        mut error: OutputErrorCallback,
    ) -> Result<Self::Stream, HostFailure> {
        let channels = u16::try_from(config.channels).map_err(|_| {
            HostFailure::new(
                SystemOutputErrorKind::Protocol,
                "physical channel count does not fit CPAL",
            )
        })?;
        let buffer_size = match config.buffer_size {
            OutputBufferRequest::Default => BufferSize::Default,
            OutputBufferRequest::Fixed(frames) => {
                BufferSize::Fixed(u32::try_from(frames).map_err(|_| {
                    HostFailure::new(
                        SystemOutputErrorKind::Protocol,
                        "physical buffer hint does not fit CPAL",
                    )
                })?)
            }
        };
        let stream_config = StreamConfig {
            channels,
            sample_rate: config.sample_rate,
            buffer_size,
        };
        let sample_format = match config.sample_format {
            DriverSampleFormat::F32 => SampleFormat::F32,
            DriverSampleFormat::I16 => SampleFormat::I16,
            DriverSampleFormat::U16 => SampleFormat::U16,
            DriverSampleFormat::Other => {
                return Err(HostFailure::new(
                    SystemOutputErrorKind::UnsupportedFormat,
                    "the planned sample format is not supported",
                ));
            }
        };
        let stream = self
            .0
            .build_output_stream_raw(
                stream_config,
                sample_format,
                move |raw, _| dispatch_raw_output(raw, &mut data),
                move |platform_error| {
                    error();
                    // A platform error may own an allocated detail string.
                    // Operational death is already latched; destruction stays
                    // off the native callback thread by retaining this value.
                    std::mem::forget(platform_error);
                },
                None,
            )
            .map_err(|error| host_failure_from_cpal(&error))?;
        Ok(SystemStream(stream))
    }
}

fn dispatch_raw_output(raw: &mut cpal::Data, data: &mut OutputDataCallback) {
    match raw.sample_format() {
        SampleFormat::F32 => data(raw.as_slice_mut::<f32>().map(OutputBuffer::F32)),
        SampleFormat::I16 => data(raw.as_slice_mut::<i16>().map(OutputBuffer::I16)),
        SampleFormat::U16 => data(raw.as_slice_mut::<u16>().map(OutputBuffer::U16)),
        _ => {
            // CPAL's raw output contract pre-fills the entire buffer with the
            // format's equilibrium. Preserve that opaque equilibrium and let
            // the callback state latch the protocol mismatch without casting.
            data(None);
        }
    }
}

impl OutputStream for SystemStream {
    fn play(&self) -> Result<(), HostFailure> {
        self.0
            .play()
            .map_err(|error| host_failure_from_cpal(&error))
    }
}

fn host_failure_from_cpal(error: &cpal::Error) -> HostFailure {
    let kind = match error.kind() {
        cpal::ErrorKind::DeviceBusy => SystemOutputErrorKind::OutputInUse,
        cpal::ErrorKind::DeviceNotAvailable
        | cpal::ErrorKind::HostUnavailable
        | cpal::ErrorKind::PermissionDenied => SystemOutputErrorKind::DeviceUnavailable,
        cpal::ErrorKind::UnsupportedConfig | cpal::ErrorKind::UnsupportedOperation => {
            SystemOutputErrorKind::UnsupportedFormat
        }
        cpal::ErrorKind::InvalidInput => SystemOutputErrorKind::Protocol,
        _ => SystemOutputErrorKind::BackendFailure,
    };
    HostFailure::new(kind, error.to_string())
}

#[cfg(test)]
mod tests;
