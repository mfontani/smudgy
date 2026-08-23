use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use super::*;
use crate::{MixerFrame, MixerInput, MixerInputStatus, MixerStartupFailure};
use futures::executor::block_on;

const TEST_RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FakeFailure {
    #[default]
    None,
    SetupError,
    SetupPanic,
    EnumerateError,
    EnumeratePanic,
    PlanError,
    PlanPanic,
    BuildError,
    BuildPanic,
    DeathDuringBuild,
    PlayError,
    PlayPanic,
    DeathDuringPlay,
    DropPanic,
}

#[derive(Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "orthogonal fake callbacks and destructor faults must compose in one test plan"
)]
struct FakeConfig {
    ranges: Vec<OutputConfigRange>,
    failure: FakeFailure,
    callback_during_build: bool,
    callback_during_play: bool,
    retain_callback_after_drop: bool,
    host_drop_panics: bool,
    device_drop_panics: bool,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            ranges: vec![range(
                2,
                TEST_RATE,
                TEST_RATE,
                DriverSampleFormat::F32,
                OutputBufferRange::Range { min: 64, max: 512 },
            )],
            failure: FakeFailure::None,
            callback_during_build: false,
            callback_during_play: false,
            retain_callback_after_drop: false,
            host_drop_panics: false,
            device_drop_panics: false,
        }
    }
}

#[derive(Default)]
struct FakeCallbacks {
    data: Option<OutputDataCallback>,
    error: Option<OutputErrorCallback>,
}

struct FakeState {
    config: FakeConfig,
    callbacks: Mutex<FakeCallbacks>,
    lifecycle: Mutex<Vec<(&'static str, String)>>,
    provisional_build_silent: AtomicBool,
    provisional_play_silent: AtomicBool,
    build_count: AtomicUsize,
    play_count: AtomicUsize,
    stream_drop_count: AtomicUsize,
}

impl FakeState {
    fn new(config: FakeConfig) -> Self {
        Self {
            config,
            callbacks: Mutex::new(FakeCallbacks::default()),
            lifecycle: Mutex::new(Vec::new()),
            provisional_build_silent: AtomicBool::new(false),
            provisional_play_silent: AtomicBool::new(false),
            build_count: AtomicUsize::new(0),
            play_count: AtomicUsize::new(0),
            stream_drop_count: AtomicUsize::new(0),
        }
    }

    fn record(&self, operation: &'static str) {
        self.lifecycle.lock().unwrap().push((
            operation,
            thread::current().name().unwrap_or("unnamed").to_owned(),
        ));
    }

    fn invoke_f32(&self, samples: usize, initial: f32) -> Vec<f32> {
        let mut callback = self.callbacks.lock().unwrap().data.take().unwrap();
        let mut output = vec![initial; samples];
        callback(Some(OutputBuffer::F32(&mut output)));
        self.callbacks.lock().unwrap().data = Some(callback);
        output
    }

    fn invoke_i16(&self, samples: usize, initial: i16) -> Vec<i16> {
        let mut callback = self.callbacks.lock().unwrap().data.take().unwrap();
        let mut output = vec![initial; samples];
        callback(Some(OutputBuffer::I16(&mut output)));
        self.callbacks.lock().unwrap().data = Some(callback);
        output
    }

    fn invoke_u16(&self, samples: usize, initial: u16) -> Vec<u16> {
        let mut callback = self.callbacks.lock().unwrap().data.take().unwrap();
        let mut output = vec![initial; samples];
        callback(Some(OutputBuffer::U16(&mut output)));
        self.callbacks.lock().unwrap().data = Some(callback);
        output
    }

    fn invoke_provisional(&self) -> bool {
        match self
            .config
            .ranges
            .iter()
            .find(|range| {
                range.channels == 2
                    && range.min_sample_rate <= TEST_RATE
                    && TEST_RATE <= range.max_sample_rate
            })
            .map_or(DriverSampleFormat::F32, |range| range.sample_format)
        {
            DriverSampleFormat::F32 | DriverSampleFormat::Other => {
                self.invoke_f32(8, 1.0).iter().all(|sample| *sample == 0.0)
            }
            DriverSampleFormat::I16 => self.invoke_i16(8, 1).iter().all(|sample| *sample == 0),
            DriverSampleFormat::U16 => self
                .invoke_u16(8, 1)
                .iter()
                .all(|sample| *sample == 1 << 15),
        }
    }
}

#[derive(Clone)]
struct FakeFactory(Arc<FakeState>);

struct FakeHost(Arc<FakeState>);
struct FakeDevice(Arc<FakeState>);
struct FakeStream(Arc<FakeState>);

impl HostFactory for FakeFactory {
    type Host = FakeHost;

    fn create(self) -> Result<Self::Host, HostFailure> {
        self.0.record("host-create");
        match self.0.config.failure {
            FakeFailure::SetupError => Err(fake_failure("setup error")),
            FakeFailure::SetupPanic => panic!("setup panic"),
            _ => Ok(FakeHost(self.0)),
        }
    }
}

impl OutputHost for FakeHost {
    type Device = FakeDevice;

    fn default_output_device(&self) -> Result<Option<Self::Device>, HostFailure> {
        self.0.record("default-device");
        match self.0.config.failure {
            FakeFailure::EnumerateError => Err(fake_failure("enumerate error")),
            FakeFailure::EnumeratePanic => panic!("enumerate panic"),
            _ => Ok(Some(FakeDevice(Arc::clone(&self.0)))),
        }
    }
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        self.0.record("host-drop");
        assert!(!self.0.config.host_drop_panics, "host drop panic");
    }
}

impl OutputDevice for FakeDevice {
    type Stream = FakeStream;

    fn supported_output_configs(&self) -> Result<Vec<OutputConfigRange>, HostFailure> {
        self.0.record("supported-configs");
        match self.0.config.failure {
            FakeFailure::PlanError => Err(fake_failure("plan error")),
            FakeFailure::PlanPanic => panic!("plan panic"),
            _ => Ok(self.0.config.ranges.clone()),
        }
    }

    fn build_output_stream(
        &self,
        _config: OutputStreamConfig,
        data: OutputDataCallback,
        error: OutputErrorCallback,
    ) -> Result<Self::Stream, HostFailure> {
        self.0.record("stream-build");
        self.0.build_count.fetch_add(1, Ordering::AcqRel);
        match self.0.config.failure {
            FakeFailure::BuildError => return Err(fake_failure("build error")),
            FakeFailure::BuildPanic => panic!("build panic"),
            _ => {}
        }
        *self.0.callbacks.lock().unwrap() = FakeCallbacks {
            data: Some(data),
            error: Some(error),
        };
        if self.0.config.failure == FakeFailure::DeathDuringBuild {
            self.0.callbacks.lock().unwrap().error.as_mut().unwrap()();
        }
        if self.0.config.callback_during_build {
            let silent = self.0.invoke_provisional();
            self.0
                .provisional_build_silent
                .store(silent, Ordering::Release);
        }
        Ok(FakeStream(Arc::clone(&self.0)))
    }
}

impl Drop for FakeDevice {
    fn drop(&mut self) {
        self.0.record("device-drop");
        assert!(!self.0.config.device_drop_panics, "device drop panic");
    }
}

impl OutputStream for FakeStream {
    fn play(&self) -> Result<(), HostFailure> {
        self.0.record("stream-play");
        self.0.play_count.fetch_add(1, Ordering::AcqRel);
        if self.0.config.callback_during_play {
            let silent = self.0.invoke_provisional();
            self.0
                .provisional_play_silent
                .store(silent, Ordering::Release);
        }
        match self.0.config.failure {
            FakeFailure::PlayError => Err(fake_failure("play error")),
            FakeFailure::PlayPanic => panic!("play panic"),
            FakeFailure::DeathDuringPlay => {
                self.0.callbacks.lock().unwrap().error.as_mut().unwrap()();
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.0.record("stream-drop");
        self.0.stream_drop_count.fetch_add(1, Ordering::AcqRel);
        if !self.0.config.retain_callback_after_drop {
            *self.0.callbacks.lock().unwrap() = FakeCallbacks::default();
        }
        assert_ne!(self.0.config.failure, FakeFailure::DropPanic, "drop panic");
    }
}

fn fake_failure(detail: &str) -> HostFailure {
    HostFailure::new(SystemOutputErrorKind::BackendFailure, detail)
}

fn range(
    channels: usize,
    min_sample_rate: u32,
    max_sample_rate: u32,
    sample_format: DriverSampleFormat,
    buffer_size: OutputBufferRange,
) -> OutputConfigRange {
    OutputConfigRange {
        channels,
        min_sample_rate,
        max_sample_rate,
        sample_format,
        buffer_size,
    }
}

struct FakeAttempt {
    result: Result<MixerService, MixerStartError<SystemOutputError>>,
    state: Arc<FakeState>,
}

fn attempt_fake(
    config: FakeConfig,
    sample_rate: u32,
    lease_flag: &'static AtomicBool,
) -> FakeAttempt {
    attempt_fake_with_hook(config, sample_rate, lease_flag, None)
}

fn attempt_fake_with_hook(
    config: FakeConfig,
    sample_rate: u32,
    lease_flag: &'static AtomicBool,
    proof_hook: Option<Arc<TestProofHook>>,
) -> FakeAttempt {
    let state = Arc::new(FakeState::new(config));
    let lease = match OutputLease::acquire(lease_flag) {
        Ok(lease) => lease,
        Err(error) => {
            return FakeAttempt {
                result: Err(MixerStartError::Backend(error)),
                state,
            };
        }
    };
    let result = MixerService::start_with_driver::<CpalOutputDriver<FakeFactory>>(
        SystemDriverSettings {
            factory: FakeFactory(Arc::clone(&state)),
            lease,
            sample_rate,
            proof_hook,
        },
        sample_rate,
    );
    FakeAttempt { result, state }
}

fn fresh_lease_flag() -> &'static AtomicBool {
    Box::leak(Box::new(AtomicBool::new(false)))
}

fn start_fake(config: FakeConfig) -> (MixerService, Arc<FakeState>) {
    let attempt = attempt_fake(config, TEST_RATE, fresh_lease_flag());
    (attempt.result.unwrap(), attempt.state)
}

#[derive(Default)]
struct ConstantInput(f32);

impl MixerInput for ConstantInput {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        output.fill(MixerFrame::from_mono(self.0));
        MixerInputStatus::Active
    }
}

fn install_constant(
    service: &MixerService,
    value: f32,
) -> (crate::MixerSessionOwner, crate::RunningMixerInput) {
    let session = service.add_session(AudioSessionId(1)).unwrap();
    let running = session
        .script_bus()
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(ConstantInput(value)))
        .unwrap();
    (session, running)
}

#[test]
fn planner_requires_exact_rate_stereo_pcm_and_clamps_the_hint() {
    let ranges = [
        range(
            1,
            TEST_RATE,
            TEST_RATE,
            DriverSampleFormat::F32,
            OutputBufferRange::Unknown,
        ),
        range(
            2,
            44_100,
            44_100,
            DriverSampleFormat::F32,
            OutputBufferRange::Unknown,
        ),
        range(
            2,
            TEST_RATE,
            TEST_RATE,
            DriverSampleFormat::Other,
            OutputBufferRange::Unknown,
        ),
        range(
            2,
            TEST_RATE,
            TEST_RATE,
            DriverSampleFormat::I16,
            OutputBufferRange::Range { min: 256, max: 512 },
        ),
        range(
            2,
            TEST_RATE,
            TEST_RATE,
            DriverSampleFormat::F32,
            OutputBufferRange::Range { min: 32, max: 64 },
        ),
    ];
    let plan = plan_output(ranges, TEST_RATE).unwrap();
    assert_eq!(plan.stream.sample_format, DriverSampleFormat::F32);
    assert_eq!(plan.stream.buffer_size, OutputBufferRequest::Fixed(64));
    assert_eq!(plan.physical.sample_format(), PhysicalSampleFormat::F32);
    assert_eq!(plan.physical.buffer_frames_hint(), Some(64));

    let i16 = plan_output([ranges[3]], TEST_RATE).unwrap();
    assert_eq!(i16.physical.buffer_frames_hint(), Some(256));
    assert!(plan_output([ranges[0]], TEST_RATE).is_err());
    assert!(plan_output([ranges[1]], TEST_RATE).is_err());
    assert!(plan_output([ranges[2]], TEST_RATE).is_err());
    assert!(
        plan_output(
            [range(
                2,
                TEST_RATE,
                TEST_RATE,
                DriverSampleFormat::F32,
                OutputBufferRange::Range {
                    min: MAX_PHYSICAL_CALLBACK_FRAMES + 1,
                    max: MAX_PHYSICAL_CALLBACK_FRAMES + 2,
                },
            )],
            TEST_RATE,
        )
        .is_err()
    );
}

#[test]
fn unknown_buffer_size_publishes_no_hint() {
    let plan = plan_output(
        [range(
            2,
            TEST_RATE,
            TEST_RATE,
            DriverSampleFormat::U16,
            OutputBufferRange::Unknown,
        )],
        TEST_RATE,
    )
    .unwrap();
    assert_eq!(plan.stream.buffer_size, OutputBufferRequest::Default);
    assert_eq!(plan.physical.buffer_frames_hint(), None);
}

#[test]
fn every_supported_pcm_encoding_renders_non_hint_sizes() {
    for format in [
        DriverSampleFormat::F32,
        DriverSampleFormat::I16,
        DriverSampleFormat::U16,
    ] {
        let (service, state) = start_fake(FakeConfig {
            ranges: vec![range(
                2,
                TEST_RATE,
                TEST_RATE,
                format,
                OutputBufferRange::Range { min: 64, max: 512 },
            )],
            ..FakeConfig::default()
        });
        let (session, running) = install_constant(&service, 0.25);
        match format {
            DriverSampleFormat::F32 => {
                let output = state.invoke_f32(514, 9.0);
                assert!(output.iter().all(|sample| (*sample - 0.25).abs() < 0.0001));
            }
            DriverSampleFormat::I16 => {
                let output = state.invoke_i16(514, 9);
                assert!(
                    output
                        .iter()
                        .all(|sample| *sample > 8_000 && *sample < 8_300)
                );
            }
            DriverSampleFormat::U16 => {
                let output = state.invoke_u16(514, 9);
                assert!(
                    output
                        .iter()
                        .all(|sample| *sample > 40_000 && *sample < 41_100)
                );
            }
            DriverSampleFormat::Other => unreachable!(),
        }
        let shutdown = running.shutdown();
        // A final physical quantum publishes the render-side close bit.
        match format {
            DriverSampleFormat::F32 => drop(state.invoke_f32(2, 9.0)),
            DriverSampleFormat::I16 => drop(state.invoke_i16(2, 9)),
            DriverSampleFormat::U16 => drop(state.invoke_u16(2, 9)),
            DriverSampleFormat::Other => unreachable!(),
        }
        assert!(block_on(shutdown).unwrap().is_clean());
        drop(session);
        assert!(service.shutdown().clean);
    }
}

#[test]
fn callback_establishes_equilibrium_and_rejects_invalid_geometry_without_allocating() {
    for samples in [0, 1, PHYSICAL_SCRATCH_SAMPLES + 2] {
        let (service, state) = start_fake(FakeConfig::default());
        let mut output = vec![9.0; samples];
        let mut callback = state.callbacks.lock().unwrap().data.take().unwrap();
        assert_no_alloc::assert_no_alloc(|| callback(Some(OutputBuffer::F32(&mut output))));
        assert!(output.iter().all(|sample| *sample == 0.0));
        state.callbacks.lock().unwrap().data = Some(callback);
        let shutdown = service.shutdown();
        assert!(shutdown.clean);
        assert_eq!(
            shutdown.failure,
            Some(MixerOutputFailure::InvalidCallbackGeometry)
        );
    }

    let (service, state) = start_fake(FakeConfig {
        ranges: vec![range(
            2,
            TEST_RATE,
            TEST_RATE,
            DriverSampleFormat::U16,
            OutputBufferRange::Unknown,
        )],
        ..FakeConfig::default()
    });
    let output = state.invoke_u16(1, 7);
    assert_eq!(output, [1 << 15]);
    assert!(service.shutdown().clean);

    let (service, state) = start_fake(FakeConfig::default());
    let output = state.invoke_u16(2, 7);
    assert_eq!(output, [1 << 15; 2]);
    let shutdown = service.shutdown();
    assert!(shutdown.clean);
    assert_eq!(
        shutdown.failure,
        Some(MixerOutputFailure::InvalidCallbackGeometry)
    );
}

#[test]
fn raw_cpal_dispatch_latches_wrong_format_without_typed_expect() {
    let status = Arc::new(crate::DriverStatus::new());
    let state = Arc::new(CallbackState::new(
        DriverFailureSignal(Arc::downgrade(&status)),
        DriverSampleFormat::F32,
        None,
    ));
    let mut callback = callback_for(&state);
    let mut samples = [7_i16; 4];
    // SAFETY: the pointer, length, and declared sample format exactly describe
    // the live `samples` array for the duration of this call.
    let mut raw = unsafe {
        cpal::Data::from_parts(
            samples.as_mut_ptr().cast::<()>(),
            samples.len(),
            SampleFormat::I16,
        )
    };
    assert_no_alloc::assert_no_alloc(|| dispatch_raw_output(&mut raw, &mut callback));
    assert_eq!(samples, [0; 4]);
    assert_eq!(
        status.failure(),
        Some(MixerOutputFailure::InvalidCallbackGeometry)
    );

    let status = Arc::new(crate::DriverStatus::new());
    let state = Arc::new(CallbackState::new(
        DriverFailureSignal(Arc::downgrade(&status)),
        DriverSampleFormat::F32,
        None,
    ));
    let mut callback = callback_for(&state);
    let mut opaque_prefill = [0x69_u8; 8];
    // SAFETY: the pointer, length, and declared sample format exactly describe
    // the live opaque buffer for the duration of this call.
    let mut raw = unsafe {
        cpal::Data::from_parts(
            opaque_prefill.as_mut_ptr().cast::<()>(),
            opaque_prefill.len(),
            SampleFormat::DsdU8,
        )
    };
    assert_no_alloc::assert_no_alloc(|| dispatch_raw_output(&mut raw, &mut callback));
    assert_eq!(opaque_prefill, [0x69; 8]);
    assert_eq!(
        status.failure(),
        Some(MixerOutputFailure::InvalidCallbackGeometry)
    );
}

#[test]
fn maximum_valid_callback_is_allocation_free() {
    let (service, state) = start_fake(FakeConfig::default());
    let (session, running) = install_constant(&service, 0.125);
    let mut output = vec![9.0; PHYSICAL_SCRATCH_SAMPLES];
    let mut callback = state.callbacks.lock().unwrap().data.take().unwrap();
    assert_no_alloc::assert_no_alloc(|| callback(Some(OutputBuffer::F32(&mut output))));
    assert!(output.iter().all(|sample| (*sample - 0.125).abs() < 0.0001));
    state.callbacks.lock().unwrap().data = Some(callback);
    let shutdown = running.shutdown();
    drop(state.invoke_f32(2, 9.0));
    assert!(block_on(shutdown).unwrap().is_clean());
    drop(session);
    assert!(service.shutdown().clean);
}

#[test]
fn callbacks_during_build_and_play_are_provisionally_silent() {
    let (service, state) = start_fake(FakeConfig {
        callback_during_build: true,
        callback_during_play: true,
        ..FakeConfig::default()
    });
    assert!(state.provisional_build_silent.load(Ordering::Acquire));
    assert!(state.provisional_play_silent.load(Ordering::Acquire));
    assert!(service.shutdown().clean);
}

#[test]
fn host_failures_and_panics_have_stable_operations() {
    let cases = [
        (FakeFailure::SetupError, SystemOutputOperation::Setup),
        (FakeFailure::SetupPanic, SystemOutputOperation::Setup),
        (
            FakeFailure::EnumerateError,
            SystemOutputOperation::Enumerate,
        ),
        (
            FakeFailure::EnumeratePanic,
            SystemOutputOperation::Enumerate,
        ),
        (FakeFailure::PlanError, SystemOutputOperation::Plan),
        (FakeFailure::PlanPanic, SystemOutputOperation::Plan),
        (FakeFailure::BuildError, SystemOutputOperation::Build),
        (FakeFailure::BuildPanic, SystemOutputOperation::Build),
        (FakeFailure::PlayError, SystemOutputOperation::Play),
        (FakeFailure::PlayPanic, SystemOutputOperation::Play),
    ];
    for (failure, operation) in cases {
        let attempt = attempt_fake(
            FakeConfig {
                failure,
                ..FakeConfig::default()
            },
            TEST_RATE,
            fresh_lease_flag(),
        );
        let error = attempt.result.unwrap_err();
        let backend = match error {
            MixerStartError::Backend(error)
            | MixerStartError::CleanupUncertain(MixerStartupFailure::Backend(error)) => error,
            other => panic!("unexpected startup failure: {other:?}"),
        };
        assert_eq!(backend.operation(), operation);
        assert_eq!(backend.kind(), SystemOutputErrorKind::BackendFailure);
        assert!(!backend.detail().is_empty());
    }
}

#[test]
fn unavailable_cause_preserves_exact_start_error_and_cleanup_truth() {
    let thread = SystemMixerUnavailable::from(MixerStartError::Thread(
        std::io::Error::from_raw_os_error(5),
    ));
    assert!(thread.cleanup_proven());
    match thread.error() {
        MixerStartError::Thread(error) => assert_eq!(error.raw_os_error(), Some(5)),
        other => panic!("exact thread cause was not retained: {other:?}"),
    }

    let protocol = SystemOutputError::new(
        SystemOutputErrorKind::Protocol,
        SystemOutputOperation::Build,
        "deterministic protocol failure",
    );
    let uncertain = SystemMixerUnavailable::from(MixerStartError::CleanupUncertain(
        MixerStartupFailure::Backend(protocol),
    ));
    assert!(!uncertain.cleanup_proven());
    assert!(matches!(
        uncertain.error(),
        MixerStartError::CleanupUncertain(MixerStartupFailure::Backend(error))
            if error.kind() == SystemOutputErrorKind::Protocol
                && error.operation() == SystemOutputOperation::Build
                && error.detail() == "deterministic protocol failure"
    ));

    let owner_stopped = SystemMixerUnavailable::from(MixerStartError::OwnerStopped);
    assert!(!owner_stopped.cleanup_proven());
    assert!(matches!(
        owner_stopped.error(),
        MixerStartError::OwnerStopped
    ));
}

fn cleanup_uncertain_backend(error: MixerStartError<SystemOutputError>) -> SystemOutputError {
    match error {
        MixerStartError::CleanupUncertain(MixerStartupFailure::Backend(error)) => error,
        other => panic!("expected cleanup-uncertain backend failure, got {other:?}"),
    }
}

#[test]
fn staged_host_destructor_panics_preserve_primary_cause_and_retain_lease() {
    for (failure, operation) in [
        (
            FakeFailure::EnumerateError,
            SystemOutputOperation::Enumerate,
        ),
        (FakeFailure::PlanError, SystemOutputOperation::Plan),
    ] {
        let flag = fresh_lease_flag();
        let error = attempt_fake(
            FakeConfig {
                failure,
                host_drop_panics: true,
                ..FakeConfig::default()
            },
            TEST_RATE,
            flag,
        )
        .result
        .unwrap_err();
        assert_eq!(cleanup_uncertain_backend(error).operation(), operation);
        assert!(flag.load(Ordering::Acquire));
    }

    let flag = fresh_lease_flag();
    let error = attempt_fake(
        FakeConfig {
            host_drop_panics: true,
            ..FakeConfig::default()
        },
        TEST_RATE,
        flag,
    )
    .result
    .unwrap_err();
    assert_eq!(
        cleanup_uncertain_backend(error).operation(),
        SystemOutputOperation::Drop
    );
    assert!(flag.load(Ordering::Acquire));
    assert!(matches!(
        attempt_fake(FakeConfig::default(), TEST_RATE, flag).result,
        Err(MixerStartError::Backend(ref error))
            if error.kind() == SystemOutputErrorKind::OutputInUse
    ));
}

#[test]
fn staged_device_destructor_panics_preserve_primary_cause_and_retain_lease() {
    for (failure, operation) in [
        (FakeFailure::PlanError, SystemOutputOperation::Plan),
        (FakeFailure::PlanPanic, SystemOutputOperation::Plan),
        (FakeFailure::BuildError, SystemOutputOperation::Build),
    ] {
        let flag = fresh_lease_flag();
        let error = attempt_fake(
            FakeConfig {
                failure,
                device_drop_panics: true,
                ..FakeConfig::default()
            },
            TEST_RATE,
            flag,
        )
        .result
        .unwrap_err();
        assert_eq!(cleanup_uncertain_backend(error).operation(), operation);
        assert!(flag.load(Ordering::Acquire));
    }

    let flag = fresh_lease_flag();
    let error = attempt_fake(
        FakeConfig {
            device_drop_panics: true,
            ..FakeConfig::default()
        },
        TEST_RATE,
        flag,
    )
    .result
    .unwrap_err();
    assert_eq!(
        cleanup_uncertain_backend(error).operation(),
        SystemOutputOperation::Drop
    );
    assert!(flag.load(Ordering::Acquire));
}

#[test]
fn death_during_play_is_operational_not_a_false_start_success() {
    for failure in [FakeFailure::DeathDuringBuild, FakeFailure::DeathDuringPlay] {
        let attempt = attempt_fake(
            FakeConfig {
                failure,
                ..FakeConfig::default()
            },
            TEST_RATE,
            fresh_lease_flag(),
        );
        assert!(matches!(
            attempt.result.unwrap_err(),
            MixerStartError::DriverFailed(MixerOutputFailure::BackendFailure)
        ));
    }
}

#[test]
fn host_device_build_play_and_drop_are_owner_thread_affine() {
    let (service, state) = start_fake(FakeConfig::default());
    assert!(service.shutdown().clean);
    let lifecycle = state.lifecycle.lock().unwrap();
    for (operation, thread_name) in lifecycle.iter() {
        assert_eq!(
            thread_name, "smudgy-audio-owner",
            "{operation} ran on the wrong thread"
        );
    }
    for required in [
        "host-create",
        "default-device",
        "supported-configs",
        "stream-build",
        "stream-play",
        "stream-drop",
        "device-drop",
        "host-drop",
    ] {
        assert!(
            lifecycle
                .iter()
                .any(|(operation, _)| *operation == required)
        );
    }
}

struct BlockingInput {
    entered: Arc<AtomicBool>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}

impl MixerInput for BlockingInput {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        self.entered.store(true, Ordering::Release);
        let (lock, ready) = &*self.gate;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = ready.wait(released).unwrap();
        }
        output.fill(MixerFrame::ZERO);
        MixerInputStatus::Active
    }
}

#[test]
fn shutdown_waits_for_an_active_callback_before_releasing_the_lease() {
    let flag = fresh_lease_flag();
    let attempt = attempt_fake(FakeConfig::default(), TEST_RATE, flag);
    let service = attempt.result.unwrap();
    let state = attempt.state;
    let session = service.add_session(AudioSessionId(9)).unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let _running = session
        .script_bus()
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(BlockingInput {
            entered: Arc::clone(&entered),
            gate: Arc::clone(&gate),
        }))
        .unwrap();
    let callback_state = Arc::clone(&state);
    let callback = thread::spawn(move || drop(callback_state.invoke_f32(256, 1.0)));
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    let gate_release = Arc::clone(&gate);
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        let (lock, ready) = &*gate_release;
        *lock.lock().unwrap() = true;
        ready.notify_one();
    });
    let shutdown = service.shutdown();
    callback.join().unwrap();
    release.join().unwrap();
    assert!(shutdown.clean);
    assert_eq!(state.stream_drop_count.load(Ordering::Acquire), 1);
    assert!(!flag.load(Ordering::Acquire));
}

#[test]
fn stalled_active_callback_has_no_false_retirement_deadline() {
    let flag = fresh_lease_flag();
    let attempt = attempt_fake(FakeConfig::default(), TEST_RATE, flag);
    let service = attempt.result.unwrap();
    let state = attempt.state;
    let session = service.add_session(AudioSessionId(10)).unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let _running = session
        .script_bus()
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(BlockingInput {
            entered: Arc::clone(&entered),
            gate: Arc::clone(&gate),
        }))
        .unwrap();
    let callback_state = Arc::clone(&state);
    let callback = thread::spawn(move || drop(callback_state.invoke_f32(256, 1.0)));
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    let gate_release = Arc::clone(&gate);
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        let (lock, ready) = &*gate_release;
        *lock.lock().unwrap() = true;
        ready.notify_one();
    });
    let shutdown = service.shutdown();
    callback.join().unwrap();
    release.join().unwrap();
    assert!(shutdown.clean);
    assert!(!flag.load(Ordering::Acquire));
}

#[test]
fn late_weak_upgrade_cannot_move_callback_state_destruction_off_owner() {
    let flag = fresh_lease_flag();
    let (before_sender, before_receiver) = std::sync::mpsc::sync_channel(1);
    let before_release = Arc::new((Mutex::new(false), Condvar::new()));
    let (attempt_sender, attempt_receiver) = std::sync::mpsc::sync_channel(1);
    let (upgrade_sender, upgrade_receiver) = std::sync::mpsc::sync_channel(1);
    let upgrade_release = Arc::new((Mutex::new(false), Condvar::new()));
    let hook = Arc::new(TestProofHook {
        before_unwrap_entered: before_sender,
        before_unwrap_release: Arc::clone(&before_release),
        unwrap_attempted: attempt_sender,
        unwrap_notified: AtomicBool::new(false),
        late_upgrade_entered: upgrade_sender,
        late_upgrade_release: Arc::clone(&upgrade_release),
    });
    let attempt = attempt_fake_with_hook(
        FakeConfig {
            retain_callback_after_drop: true,
            ..FakeConfig::default()
        },
        TEST_RATE,
        flag,
        Some(hook),
    );
    let service = attempt.result.unwrap();
    let state = attempt.state;
    let coordinator = thread::spawn(move || {
        before_receiver.recv().unwrap();
        let callback = thread::spawn(move || drop(state.invoke_f32(2, 1.0)));
        upgrade_receiver.recv().unwrap();
        let (lock, ready) = &*before_release;
        *lock.lock().unwrap() = true;
        ready.notify_one();
        assert!(!attempt_receiver.recv().unwrap());
        let (lock, ready) = &*upgrade_release;
        *lock.lock().unwrap() = true;
        ready.notify_one();
        callback.join().unwrap();
    });
    let shutdown = service.shutdown();
    coordinator.join().unwrap();
    assert!(shutdown.clean);
    assert!(!flag.load(Ordering::Acquire));
}

#[test]
fn clean_shutdown_releases_singleton_for_immediate_repeat() {
    let flag = fresh_lease_flag();
    let first = attempt_fake(FakeConfig::default(), TEST_RATE, flag)
        .result
        .unwrap();
    assert!(first.shutdown().clean);
    assert!(!flag.load(Ordering::Acquire));
    let second = attempt_fake(FakeConfig::default(), TEST_RATE, flag)
        .result
        .unwrap();
    assert!(second.shutdown().clean);
    assert!(!flag.load(Ordering::Acquire));
}

#[test]
fn uncertain_stream_drop_quarantines_callback_state_and_singleton() {
    let flag = fresh_lease_flag();
    let service = attempt_fake(
        FakeConfig {
            failure: FakeFailure::DropPanic,
            ..FakeConfig::default()
        },
        TEST_RATE,
        flag,
    )
    .result
    .unwrap();
    let shutdown = service.shutdown();
    assert!(!shutdown.clean);
    assert!(flag.load(Ordering::Acquire));
    let error = attempt_fake(FakeConfig::default(), TEST_RATE, flag)
        .result
        .unwrap_err();
    assert!(matches!(
        error,
        MixerStartError::Backend(ref error)
            if error.operation() == SystemOutputOperation::Setup
                && error.kind() == SystemOutputErrorKind::OutputInUse
    ));
}

#[test]
fn runtime_error_callback_seals_admission_but_cleanup_remains_independent() {
    let (service, state) = start_fake(FakeConfig::default());
    state.callbacks.lock().unwrap().error.as_mut().unwrap()();
    assert_eq!(
        service.add_session(AudioSessionId(77)).unwrap_err(),
        MixerControlError::OwnerStopped
    );
    let shutdown = service.shutdown();
    assert!(shutdown.clean);
    assert_eq!(shutdown.failure, Some(MixerOutputFailure::BackendFailure));
}

struct ObservedInput {
    observer: Arc<ObservedFailure>,
}

impl MixerInput for ObservedInput {
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        output.fill(MixerFrame::ZERO);
        MixerInputStatus::Active
    }

    fn output_failure_observer(&self) -> Option<Arc<dyn crate::MixerFailureObserver>> {
        Some(self.observer.clone())
    }
}

struct ObservedFailure {
    sender: Mutex<Option<std::sync::mpsc::SyncSender<(MixerOutputFailure, String)>>>,
}

impl crate::MixerFailureObserver for ObservedFailure {
    fn output_failed(&self, failure: MixerOutputFailure) {
        if let Some(sender) = self.sender.lock().unwrap().take() {
            let _ = sender.send((
                failure,
                thread::current().name().unwrap_or("unnamed").to_owned(),
            ));
        }
    }
}

#[test]
fn installed_observer_gets_exact_runtime_failure_off_owner_and_callback_threads() {
    let (service, state) = start_fake(FakeConfig::default());
    let session = service.add_session(AudioSessionId(78)).unwrap();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let observer = Arc::new(ObservedFailure {
        sender: Mutex::new(Some(sender)),
    });
    let _running = session
        .script_bus()
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(ObservedInput { observer }))
        .unwrap();
    state.callbacks.lock().unwrap().error.as_mut().unwrap()();
    let (failure, thread_name) = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(failure, MixerOutputFailure::BackendFailure);
    assert_eq!(thread_name, "smudgy-audio-cleanup");
    let shutdown = service.shutdown();
    assert!(shutdown.clean);
    assert_eq!(shutdown.failure, Some(MixerOutputFailure::BackendFailure));
}

#[test]
fn overlapping_callback_entry_is_refused_and_latched() {
    let status = Arc::new(crate::DriverStatus::new());
    let state = CallbackState::new(
        DriverFailureSignal(Arc::downgrade(&status)),
        DriverSampleFormat::F32,
        None,
    );
    let active = state.enter().unwrap();
    assert!(state.enter().is_none());
    assert_eq!(status.failure(), Some(MixerOutputFailure::BackendFailure));
    drop(active);
    assert_eq!(state.active.load(Ordering::Acquire), 0);
}

#[test]
fn ordinary_service_drop_releases_singleton_after_autonomous_proven_cleanup() {
    let flag = fresh_lease_flag();
    let service = attempt_fake(FakeConfig::default(), TEST_RATE, flag)
        .result
        .unwrap();
    drop(service);
    for _ in 0..1_000 {
        if !flag.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("autonomous owner cleanup did not release the proven singleton lease");
}

#[test]
fn public_errors_and_private_settings_have_the_intended_traits() {
    fn assert_error<T: std::error::Error + Send + Sync + 'static>() {}
    fn assert_send<T: Send>() {}
    trait AmbiguousIfSend<Marker> {
        fn assert_not_send() {}
    }
    trait AmbiguousIfSync<Marker> {
        fn assert_not_sync() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
    assert_error::<SystemOutputError>();
    assert_send::<SystemDriverSettings<FakeFactory>>();
    let _ = <SystemMixerService as AmbiguousIfSend<_>>::assert_not_send;
    let _ = <SystemMixerService as AmbiguousIfSync<_>>::assert_not_sync;
}

#[test]
fn system_wrapper_forwards_only_the_scoped_service_surface() {
    fn assert_clone_send_sync<T: Clone + Send + Sync>() {}
    assert_clone_send_sync::<crate::MixerSessionRegistrar>();

    let attempt = attempt_fake(FakeConfig::default(), TEST_RATE, fresh_lease_flag());
    let expected_physical = attempt.result.as_ref().unwrap().physical_output_format();
    let service = SystemMixerService(attempt.result.unwrap());
    assert_eq!(service.format().sample_rate(), TEST_RATE);
    assert_eq!(service.physical_output_format(), expected_physical);
    let session = service
        .session_registrar()
        .add_session(AudioSessionId(1234))
        .unwrap();
    assert_eq!(session.script_bus().format().unwrap(), service.format());
    assert!(service.shutdown().clean);
}

#[test]
#[ignore = "manual default-device lifecycle smoke"]
fn manual_default_device_silent_open_suspend_resume_close_and_repeat() {
    let service = SystemMixerService::start(TEST_RATE).unwrap();
    let session = service.add_session(AudioSessionId(999)).unwrap();
    let running = session
        .script_bus()
        .try_reserve_input()
        .unwrap()
        .start_preboxed(Box::new(ConstantInput(0.0)))
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    assert!(running.suspend());
    thread::sleep(Duration::from_millis(20));
    assert!(running.resume());
    thread::sleep(Duration::from_millis(50));
    assert!(block_on(running.shutdown()).unwrap().is_clean());
    let shutdown = service.shutdown();
    assert!(shutdown.clean);
    assert!(shutdown.failure.is_none());

    let repeat = SystemMixerService::start(TEST_RATE).unwrap();
    let shutdown = repeat.shutdown();
    assert!(shutdown.clean);
    assert!(shutdown.failure.is_none());
}
