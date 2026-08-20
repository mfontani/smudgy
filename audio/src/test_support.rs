//! Feature-gated fake physical output for cross-crate lifecycle tests.
//!
//! This module is an unstable, nondefault test ABI. It is not part of the
//! production/default API and deliberately exposes no Kira renderer, backend,
//! driver-status, or retirement-proof authority.

use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use super::{
    DriverFailureSignal, DriverRenderError, DriverStatus, JoinedOutputDriver, JoinedRenderer,
    MixerOutputFailure, MixerService, MixerStartError, PhysicalOutputFormat, PhysicalSampleFormat,
    lock_recover,
};

/// Physical sample encoding reported by the deterministic fake device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TestSampleFormat {
    /// Native 32-bit floating-point samples.
    #[default]
    F32,
    /// Native signed 16-bit integer samples.
    I16,
    /// Native unsigned 16-bit integer samples.
    U16,
}

impl From<TestSampleFormat> for PhysicalSampleFormat {
    fn from(value: TestSampleFormat) -> Self {
        match value {
            TestSampleFormat::F32 => Self::F32,
            TestSampleFormat::I16 => Self::I16,
            TestSampleFormat::U16 => Self::U16,
        }
    }
}

/// One deterministic fake-driver failure injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TestDriverFailure {
    /// No injected failure.
    #[default]
    None,
    /// Return an error while constructing the driver.
    SetupError,
    /// Panic while constructing the driver.
    SetupPanic,
    /// Return an error while accepting the renderer.
    StartError,
    /// Panic while accepting the renderer.
    StartPanic,
    /// Return an error while starting playback.
    PlayError,
    /// Panic while starting playback.
    PlayPanic,
    /// Report device death synchronously while starting playback.
    DeathDuringPlay,
    /// Fail the explicit close/join operation.
    CloseError,
    /// Panic during the explicit close/join operation.
    ClosePanic,
    /// Panic while destroying the already-closed driver shell.
    DropPanic,
}

/// Settings for the deterministic fake physical output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestDriverConfig {
    /// Sample rate returned by the fake device.
    pub actual_sample_rate: u32,
    /// Channel count in the negotiated fake physical format.
    pub actual_channels: usize,
    /// Sample encoding in the negotiated fake physical format.
    pub sample_format: TestSampleFormat,
    /// Driver-requested fake physical buffer-size hint, when present.
    pub buffer_frames_hint: Option<usize>,
    /// Optional deterministic lifecycle failure.
    pub failure: TestDriverFailure,
    /// Invoke one callback while the driver is still provisional.
    pub render_during_start: bool,
    /// Invoke one callback synchronously from physical `play`.
    pub render_during_play: bool,
}

impl Default for TestDriverConfig {
    fn default() -> Self {
        Self {
            actual_sample_rate: 48_000,
            actual_channels: 2,
            sample_format: TestSampleFormat::F32,
            buffer_frames_hint: Some(super::INTERNAL_BUFFER_FRAMES),
            failure: TestDriverFailure::None,
            render_during_start: false,
            render_during_play: false,
        }
    }
}

/// Stable fake-driver error returned through [`MixerStartError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestDriverError {
    /// Injected setup failure.
    Setup,
    /// Injected renderer-start failure.
    Start,
    /// Injected physical-play failure.
    Play,
}

/// Result of asking the fake physical callback to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestRenderError {
    /// No physical callback is currently owned by the driver.
    NotLive,
    /// Channel/buffer geometry was invalid and failed the output.
    InvalidGeometry,
    /// The contained Kira renderer unwound and failed the output.
    RendererPanicked,
}

struct TestDriverState {
    renderer: Mutex<Option<JoinedRenderer>>,
    signal: Mutex<Option<DriverFailureSignal>>,
    status: Mutex<Weak<DriverStatus>>,
    provisional_silent: AtomicBool,
    play_callback_silent: AtomicBool,
    setup_count: AtomicUsize,
    start_count: AtomicUsize,
    play_count: AtomicUsize,
    close_count: AtomicUsize,
    drop_count: AtomicUsize,
}

impl TestDriverState {
    fn new() -> Self {
        Self {
            renderer: Mutex::new(None),
            signal: Mutex::new(None),
            status: Mutex::new(Weak::new()),
            provisional_silent: AtomicBool::new(false),
            play_callback_silent: AtomicBool::new(false),
            setup_count: AtomicUsize::new(0),
            start_count: AtomicUsize::new(0),
            play_count: AtomicUsize::new(0),
            close_count: AtomicUsize::new(0),
            drop_count: AtomicUsize::new(0),
        }
    }
}

/// Narrow handle to the fake physical callback; it exposes no Kira or witness authority.
#[derive(Clone)]
pub struct TestDriverProbe(Arc<TestDriverState>);

impl std::fmt::Debug for TestDriverProbe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TestDriverProbe")
            .finish_non_exhaustive()
    }
}

impl TestDriverProbe {
    /// Render one interleaved callback using the requested channel count.
    ///
    /// # Errors
    ///
    /// Returns a typed missing-callback or geometry/render failure.
    pub fn render(&self, output: &mut [f32], channels: usize) -> Result<(), TestRenderError> {
        let mut renderer = lock_recover(&self.0.renderer);
        let renderer = renderer.as_mut().ok_or(TestRenderError::NotLive)?;
        renderer
            .render(output, channels)
            .map_err(|error| match error {
                DriverRenderError::InvalidGeometry => TestRenderError::InvalidGeometry,
                DriverRenderError::RendererPanicked => TestRenderError::RendererPanicked,
            })
    }

    /// Report an asynchronous device/backend death. The first close or error wins.
    #[must_use]
    pub fn fail_output(&self) -> bool {
        lock_recover(&self.0.signal)
            .as_ref()
            .is_some_and(|signal| signal.report(MixerOutputFailure::BackendFailure))
    }

    /// Inject an unwind on the sole command owner at its next bounded poll.
    pub fn panic_owner(&self) {
        if let Some(status) = lock_recover(&self.0.status).upgrade() {
            status.panic_owner.store(true, Ordering::Release);
        }
    }

    /// Whether the callback invoked during provisional start produced silence.
    #[must_use]
    pub fn provisional_callback_was_silent(&self) -> bool {
        self.0.provisional_silent.load(Ordering::Acquire)
    }

    /// Whether a callback invoked synchronously by `play` produced silence.
    #[must_use]
    pub fn play_callback_was_silent(&self) -> bool {
        self.0.play_callback_silent.load(Ordering::Acquire)
    }

    /// Number of fake physical setup calls.
    #[must_use]
    pub fn setup_count(&self) -> usize {
        self.0.setup_count.load(Ordering::Acquire)
    }

    /// Number of fake physical start calls.
    #[must_use]
    pub fn start_count(&self) -> usize {
        self.0.start_count.load(Ordering::Acquire)
    }

    /// Number of fake physical play calls.
    #[must_use]
    pub fn play_count(&self) -> usize {
        self.0.play_count.load(Ordering::Acquire)
    }

    /// Number of explicit fake close/join calls.
    #[must_use]
    pub fn close_count(&self) -> usize {
        self.0.close_count.load(Ordering::Acquire)
    }

    /// Number of fake driver-shell destructor calls.
    #[must_use]
    pub fn drop_count(&self) -> usize {
        self.0.drop_count.load(Ordering::Acquire)
    }
}

struct TestDriverSettings {
    config: TestDriverConfig,
    state: Arc<TestDriverState>,
}

struct TestDriver {
    config: TestDriverConfig,
    state: Arc<TestDriverState>,
}

impl JoinedOutputDriver for TestDriver {
    type Settings = TestDriverSettings;
    type Error = TestDriverError;

    fn setup(
        settings: Self::Settings,
        _internal_buffer_size: usize,
        failures: DriverFailureSignal,
    ) -> Result<(Self, PhysicalOutputFormat), Self::Error> {
        settings.state.setup_count.fetch_add(1, Ordering::AcqRel);
        assert!(
            settings.config.failure != TestDriverFailure::SetupPanic,
            "injected fake-driver setup panic"
        );
        if settings.config.failure == TestDriverFailure::SetupError {
            return Err(TestDriverError::Setup);
        }
        *lock_recover(&settings.state.status) = failures.0.clone();
        *lock_recover(&settings.state.signal) = Some(failures);
        let format = PhysicalOutputFormat {
            sample_rate: settings.config.actual_sample_rate,
            channels: settings.config.actual_channels,
            sample_format: settings.config.sample_format.into(),
            buffer_frames_hint: settings.config.buffer_frames_hint,
        };
        Ok((
            Self {
                config: settings.config,
                state: settings.state,
            },
            format,
        ))
    }

    fn start(&mut self, mut renderer: JoinedRenderer) -> Result<(), Self::Error> {
        self.state.start_count.fetch_add(1, Ordering::AcqRel);
        assert!(
            self.config.failure != TestDriverFailure::StartPanic,
            "injected fake-driver start panic"
        );
        if self.config.failure == TestDriverFailure::StartError {
            return Err(TestDriverError::Start);
        }
        if self.config.render_during_start {
            let mut provisional = [1.0; 8];
            let result = renderer.render(&mut provisional, 2);
            self.state.provisional_silent.store(
                result.is_ok() && provisional.iter().all(|sample| *sample == 0.0),
                Ordering::Release,
            );
        }
        *lock_recover(&self.state.renderer) = Some(renderer);
        Ok(())
    }

    fn play(&mut self) -> Result<(), Self::Error> {
        self.state.play_count.fetch_add(1, Ordering::AcqRel);
        if self.config.render_during_play {
            let mut provisional = [1.0; 8];
            let result = lock_recover(&self.state.renderer)
                .as_mut()
                .map(|renderer| renderer.render(&mut provisional, 2));
            self.state.play_callback_silent.store(
                matches!(result, Some(Ok(()))) && provisional.iter().all(|sample| *sample == 0.0),
                Ordering::Release,
            );
        }
        assert!(
            self.config.failure != TestDriverFailure::PlayPanic,
            "injected fake-driver play panic"
        );
        if self.config.failure == TestDriverFailure::PlayError {
            return Err(TestDriverError::Play);
        }
        if self.config.failure == TestDriverFailure::DeathDuringPlay {
            let _ = lock_recover(&self.state.signal)
                .as_ref()
                .expect("fake driver owns its failure signal")
                .report(MixerOutputFailure::BackendFailure);
        }
        Ok(())
    }

    fn close_and_join(&mut self) -> bool {
        self.state.close_count.fetch_add(1, Ordering::AcqRel);
        assert!(
            self.config.failure != TestDriverFailure::ClosePanic,
            "injected fake-driver close panic"
        );
        if self.config.failure == TestDriverFailure::CloseError {
            return false;
        }
        lock_recover(&self.state.renderer).take();
        true
    }
}

impl Drop for TestDriver {
    fn drop(&mut self) {
        self.state.drop_count.fetch_add(1, Ordering::AcqRel);
        assert!(
            self.config.failure != TestDriverFailure::DropPanic,
            "injected fake-driver destructor panic"
        );
    }
}

/// Start one mixer around the deterministic fake physical output.
///
/// # Errors
///
/// Returns the same typed startup errors as a production joined driver.
pub fn start_test_mixer(
    expected_sample_rate: u32,
    config: TestDriverConfig,
) -> Result<(MixerService, TestDriverProbe), MixerStartError<TestDriverError>> {
    let (result, probe) = attempt_test_mixer(expected_sample_rate, config);
    result.map(|service| (service, probe))
}

fn attempt_test_mixer(
    expected_sample_rate: u32,
    config: TestDriverConfig,
) -> (
    Result<MixerService, MixerStartError<TestDriverError>>,
    TestDriverProbe,
) {
    let state = Arc::new(TestDriverState::new());
    let probe = TestDriverProbe(Arc::clone(&state));
    let service = MixerService::start_with_driver::<TestDriver>(
        TestDriverSettings { config, state },
        expected_sample_rate,
    );
    (service, probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AudioSessionId, MAX_PHYSICAL_CALLBACK_FRAMES, MixerControlError, MixerFrame, MixerInput,
        MixerInputStatus, MixerOutputFailure, MixerStartupFailure, PhysicalSampleFormat,
    };

    struct LengthInput {
        lengths: Arc<[AtomicUsize; 3]>,
        calls: usize,
    }

    impl MixerInput for LengthInput {
        fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
            if let Some(length) = self.lengths.get(self.calls) {
                length.store(output.len(), Ordering::Release);
            }
            self.calls += 1;
            output.fill(MixerFrame::ZERO);
            MixerInputStatus::Active
        }
    }

    fn assert_start_failure(failure: TestDriverFailure) {
        let error = start_test_mixer(
            48_000,
            TestDriverConfig {
                failure,
                ..TestDriverConfig::default()
            },
        )
        .expect_err("injected startup failure must be returned");
        match failure {
            TestDriverFailure::SetupError => {
                assert!(matches!(
                    error,
                    MixerStartError::Backend(TestDriverError::Setup)
                ));
            }
            TestDriverFailure::StartError => {
                assert!(matches!(
                    error,
                    MixerStartError::Backend(TestDriverError::Start)
                ));
            }
            TestDriverFailure::PlayError => {
                assert!(matches!(
                    error,
                    MixerStartError::Backend(TestDriverError::Play)
                ));
            }
            TestDriverFailure::SetupPanic
            | TestDriverFailure::StartPanic
            | TestDriverFailure::PlayPanic
            | TestDriverFailure::DeathDuringPlay => {
                assert!(matches!(
                    error,
                    MixerStartError::DriverFailed(MixerOutputFailure::BackendFailure)
                ));
            }
            _ => panic!("not a startup failure: {failure:?}"),
        }
    }

    #[test]
    fn setup_start_play_errors_and_panics_are_joined_before_return() {
        for failure in [
            TestDriverFailure::SetupError,
            TestDriverFailure::SetupPanic,
            TestDriverFailure::StartError,
            TestDriverFailure::StartPanic,
            TestDriverFailure::PlayError,
            TestDriverFailure::PlayPanic,
            TestDriverFailure::DeathDuringPlay,
        ] {
            assert_start_failure(failure);
        }
    }

    #[test]
    fn callbacks_during_failed_or_panicking_play_are_provisional_silence() {
        for failure in [
            TestDriverFailure::PlayError,
            TestDriverFailure::PlayPanic,
            TestDriverFailure::DeathDuringPlay,
        ] {
            let (result, probe) = attempt_test_mixer(
                48_000,
                TestDriverConfig {
                    failure,
                    render_during_play: true,
                    ..TestDriverConfig::default()
                },
            );
            assert!(result.is_err(), "{failure:?}");
            assert!(probe.play_callback_was_silent(), "{failure:?}");
            assert_eq!(probe.close_count(), 1, "{failure:?}");
            assert_eq!(probe.drop_count(), 1, "{failure:?}");
        }
    }

    #[test]
    fn negotiated_rate_mismatch_uses_joined_close_before_driver_drop() {
        let (result, probe) = attempt_test_mixer(
            48_000,
            TestDriverConfig {
                actual_sample_rate: 44_100,
                ..TestDriverConfig::default()
            },
        );
        assert!(matches!(
            result,
            Err(MixerStartError::SampleRateMismatch {
                expected: 48_000,
                actual: 44_100
            })
        ));
        assert_eq!(probe.close_count(), 1);
        assert_eq!(probe.drop_count(), 1);
    }

    #[test]
    fn startup_format_failure_reports_cleanup_uncertainty_when_join_is_unproven() {
        for failure in [TestDriverFailure::CloseError, TestDriverFailure::ClosePanic] {
            let (result, probe) = attempt_test_mixer(
                48_000,
                TestDriverConfig {
                    actual_sample_rate: 44_100,
                    failure,
                    ..TestDriverConfig::default()
                },
            );
            assert!(matches!(
                result,
                Err(MixerStartError::CleanupUncertain(
                    MixerStartupFailure::SampleRateMismatch {
                        expected: 48_000,
                        actual: 44_100
                    }
                ))
            ));
            assert_eq!(probe.close_count(), 1, "{failure:?}");
            assert_eq!(probe.drop_count(), 0, "{failure:?}");

            let (result, probe) = attempt_test_mixer(
                48_000,
                TestDriverConfig {
                    actual_channels: 6,
                    failure,
                    ..TestDriverConfig::default()
                },
            );
            assert!(matches!(
                result,
                Err(MixerStartError::CleanupUncertain(
                    MixerStartupFailure::UnsupportedPhysicalFormat(format)
                )) if format.number_of_channels() == 6
            ));
            assert_eq!(probe.close_count(), 1, "{failure:?}");
            assert_eq!(probe.drop_count(), 0, "{failure:?}");
        }
    }

    #[test]
    fn provisional_callback_is_silent_and_one_physical_driver_is_joined() {
        let config = TestDriverConfig {
            sample_format: TestSampleFormat::I16,
            buffer_frames_hint: Some(512),
            render_during_start: true,
            ..TestDriverConfig::default()
        };
        let (service, probe) = start_test_mixer(48_000, config).unwrap();
        let physical = service.physical_output_format();
        assert_eq!(physical.sample_rate(), 48_000);
        assert_eq!(physical.number_of_channels(), 2);
        assert_eq!(physical.sample_format(), PhysicalSampleFormat::I16);
        assert_eq!(physical.buffer_frames_hint(), Some(512));
        assert_eq!(
            physical.max_frames_per_callback(),
            MAX_PHYSICAL_CALLBACK_FRAMES
        );
        assert!(probe.provisional_callback_was_silent());
        assert_eq!(probe.setup_count(), 1);
        assert_eq!(probe.start_count(), 1);
        assert_eq!(probe.play_count(), 1);
        assert!(service.shutdown().clean);
        assert_eq!(probe.close_count(), 1);
        assert_eq!(probe.drop_count(), 1);
    }

    #[test]
    fn unsupported_negotiated_channels_are_joined_before_rejection() {
        let (result, probe) = attempt_test_mixer(
            48_000,
            TestDriverConfig {
                actual_channels: 6,
                ..TestDriverConfig::default()
            },
        );
        assert!(matches!(
            result,
            Err(MixerStartError::UnsupportedPhysicalFormat(format))
                if format.number_of_channels() == 6
        ));
        assert_eq!(probe.close_count(), 1);
        assert_eq!(probe.drop_count(), 1);
    }

    #[test]
    fn close_and_driver_drop_failures_never_report_clean_join() {
        for failure in [
            TestDriverFailure::CloseError,
            TestDriverFailure::ClosePanic,
            TestDriverFailure::DropPanic,
        ] {
            let (service, probe) = start_test_mixer(
                48_000,
                TestDriverConfig {
                    failure,
                    ..TestDriverConfig::default()
                },
            )
            .unwrap();
            assert!(!service.shutdown().clean, "{failure:?}");
            assert_eq!(probe.close_count(), 1);
            let expected_drops = usize::from(failure == TestDriverFailure::DropPanic);
            assert_eq!(probe.drop_count(), expected_drops);
            if matches!(
                failure,
                TestDriverFailure::CloseError | TestDriverFailure::ClosePanic
            ) {
                let mut output = [1.0; 8];
                assert!(probe.render(&mut output, 2).is_ok());
                assert!(output.iter().all(|sample| *sample == 0.0));
            }
        }
    }

    #[test]
    fn invalid_callback_geometry_fails_closed_and_seals_control() {
        for (samples, channels) in [(0, 2), (3, 2), (4, 1)] {
            let (service, probe) = start_test_mixer(48_000, TestDriverConfig::default()).unwrap();
            let session = service.add_session(AudioSessionId(1)).unwrap();
            let mut output = vec![1.0; samples];
            assert_eq!(
                probe.render(&mut output, channels),
                Err(TestRenderError::InvalidGeometry)
            );
            assert!(output.iter().all(|sample| *sample == 0.0));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match session.script_bus().format() {
                    Err(MixerControlError::OwnerStopped) => break,
                    Ok(_) => {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("unexpected post-failure control result: {error:?}"),
                }
            }
            assert!(service.shutdown().clean);
        }
    }

    #[test]
    fn arbitrary_valid_callback_is_split_into_exact_fixed_quanta_without_allocating() {
        let (service, probe) = start_test_mixer(48_000, TestDriverConfig::default()).unwrap();
        let session = service.add_session(AudioSessionId(2)).unwrap();
        let lengths = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
        let running = session
            .script_bus()
            .try_reserve_input()
            .unwrap()
            .start_preboxed(Box::new(LengthInput {
                lengths: Arc::clone(&lengths),
                calls: 0,
            }))
            .unwrap();
        let mut output = vec![1.0; 257 * 2];
        assert_no_alloc::assert_no_alloc(|| probe.render(&mut output, 2)).unwrap();
        assert!(output.iter().all(|sample| *sample == 0.0));
        assert_eq!(
            lengths
                .each_ref()
                .map(|length| length.load(Ordering::Acquire)),
            [128, 128, 1]
        );
        drop(running);
        assert!(service.shutdown().clean);
    }

    #[test]
    fn physical_callback_hard_max_is_accepted_and_max_plus_one_fails_closed() {
        let (service, probe) = start_test_mixer(48_000, TestDriverConfig::default()).unwrap();
        let mut boundary = vec![1.0; MAX_PHYSICAL_CALLBACK_FRAMES * 2];
        assert_no_alloc::assert_no_alloc(|| probe.render(&mut boundary, 2)).unwrap();
        assert!(boundary.iter().all(|sample| *sample == 0.0));
        assert!(service.shutdown().clean);

        let (service, probe) = start_test_mixer(48_000, TestDriverConfig::default()).unwrap();
        let mut oversized = vec![1.0; (MAX_PHYSICAL_CALLBACK_FRAMES + 1) * 2];
        assert_eq!(
            probe.render(&mut oversized, 2),
            Err(TestRenderError::InvalidGeometry)
        );
        assert!(oversized.iter().all(|sample| *sample == 0.0));
        let shutdown = service.shutdown();
        assert!(shutdown.clean);
        assert_eq!(
            shutdown.failure,
            Some(MixerOutputFailure::InvalidCallbackGeometry)
        );
    }

    #[test]
    fn normal_close_wins_against_a_late_backend_error() {
        let (service, probe) = start_test_mixer(48_000, TestDriverConfig::default()).unwrap();
        service.driver_status.begin_close();
        assert!(!probe.fail_output());
        assert!(service.shutdown().clean);
    }

    #[test]
    fn backend_error_wins_against_a_late_normal_close() {
        let (service, probe) = start_test_mixer(48_000, TestDriverConfig::default()).unwrap();
        assert!(probe.fail_output());
        assert!(!service.driver_status.begin_close());
        assert_eq!(
            service.driver_status.failure(),
            Some(MixerOutputFailure::BackendFailure)
        );
        assert!(service.shutdown().clean);
    }
}
