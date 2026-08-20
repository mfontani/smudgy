//! Process-level audio mixing primitives for Smudgy.
//!
//! This crate deliberately has no Deno, V8, UI, package, or session-runtime
//! dependency. It owns the bounded physical-mixer topology that those layers
//! use through explicit adapters.

use std::{
    cell::UnsafeCell,
    collections::HashMap,
    convert::Infallible,
    fmt,
    future::Future,
    marker::PhantomData,
    mem::ManuallyDrop,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
    time::Duration,
};

use futures::channel::oneshot;
use kira::{
    AudioManager, AudioManagerSettings, Capacities, Decibels, Frame as KiraFrame, Tween,
    backend::{Backend, Renderer},
    sound::{Sound, SoundData},
    track::{MainTrackBuilder, TrackBuilder, TrackHandle},
};

/// The fixed number of frames in one internal mixer quantum.
pub const INTERNAL_BUFFER_FRAMES: usize = 128;
/// Maximum number of simultaneous sessions in the first mixer profile.
pub const MAX_SESSIONS: usize = 32;
/// Maximum number of simultaneous inputs on each session bus.
pub const INPUTS_PER_BUS: usize = 32;
/// Bounded number of pending ordinary control operations.
pub const CONTROL_QUEUE_CAPACITY: usize = 128;

const SESSION_BUS_COUNT: usize = 3;
const RETIREMENT_QUEUE_CAPACITY: usize = MAX_SESSIONS * SESSION_BUS_COUNT * INPUTS_PER_BUS;
const RETIREMENT_SCAN_INTERVAL: Duration = Duration::from_millis(10);

/// A stable session identity within one process mixer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioSessionId(pub u64);

/// A source category within a session's mixer subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionBus {
    /// Web Audio and other script-created playback.
    Script,
    /// Native protocol and client-interface sounds.
    Native,
    /// Speech and accessibility playback.
    Speech,
}

impl SessionBus {
    const fn ordinal(self) -> usize {
        match self {
            Self::Script => 0,
            Self::Native => 1,
            Self::Speech => 2,
        }
    }
}

/// Exact logical format mixed by one [`MixerService`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerFormat {
    sample_rate: u32,
}

impl MixerFormat {
    /// Sample rate verified against the backend during service startup.
    #[must_use]
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }
    /// The mixer is permanently stereo.
    #[must_use]
    pub const fn number_of_channels(self) -> usize {
        2
    }
    /// Maximum frames in one logical input invocation.
    #[must_use]
    pub const fn max_frames_per_callback(self) -> usize {
        INTERNAL_BUFFER_FRAMES
    }
}

/// One stereo sample exchanged with a [`MixerInput`].
///
/// This value belongs to `smudgy_audio`; mixer inputs do not depend on the
/// physical mixer's internal frame representation.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MixerFrame {
    left: f32,
    right: f32,
}

impl MixerFrame {
    /// A silent stereo frame.
    pub const ZERO: Self = Self::new(0.0, 0.0);

    /// Creates a frame from separate left and right samples.
    #[must_use]
    pub const fn new(left: f32, right: f32) -> Self {
        Self { left, right }
    }

    /// Creates a frame with the same sample in both channels.
    #[must_use]
    pub const fn from_mono(sample: f32) -> Self {
        Self::new(sample, sample)
    }

    /// Returns the left-channel sample.
    #[must_use]
    pub const fn left(self) -> f32 {
        self.left
    }

    /// Returns the right-channel sample.
    #[must_use]
    pub const fn right(self) -> f32 {
        self.right
    }
}

/// Whether an input has more audio to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerInputStatus {
    /// Keep the input active for the next quantum.
    Active,
    /// Close this logical input after the current quantum.
    Finished,
}

/// A real-time audio producer installed into one reserved mixer slot.
///
/// Implementations must overwrite the complete output slice and must not
/// allocate, block, or unwind. The slot contains an unexpected unwind and
/// fails closed so it cannot cross the backend callback boundary.
pub trait MixerInput: Send + 'static {
    /// Render the next stereo quantum.
    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus;
}

const PHASE_MASK: u64 = 0x0f;
const ACTIVE: u64 = 1 << 4;
const SUSPENDED: u64 = 1 << 5;
const FAILED: u64 = 1 << 6;
const HAS_PAYLOAD: u64 = 1 << 7;
const GENERATION_SHIFT: u32 = 8;
const MAX_GENERATION: u64 = u64::MAX >> GENERATION_SHIFT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
enum SlotPhase {
    Free = 0,
    Reserved = 1,
    Installed = 2,
    Running = 3,
    Closing = 4,
    Retiring = 5,
    ForcedClean = 6,
    Quarantined = 7,
}

const fn slot_word(generation: u64, phase: SlotPhase, flags: u64) -> u64 {
    (generation << GENERATION_SHIFT) | phase as u64 | flags
}

const fn slot_generation(word: u64) -> u64 {
    word >> GENERATION_SHIFT
}

fn slot_phase(word: u64) -> SlotPhase {
    match word & PHASE_MASK {
        0 => SlotPhase::Free,
        1 => SlotPhase::Reserved,
        2 => SlotPhase::Installed,
        3 => SlotPhase::Running,
        4 => SlotPhase::Closing,
        5 => SlotPhase::Retiring,
        6 => SlotPhase::ForcedClean,
        _ => SlotPhase::Quarantined,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SessionKey {
    id: AudioSessionId,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SlotAddress {
    session: SessionKey,
    bus: SessionBus,
    index: usize,
}

struct ForcedWaiter {
    generation: u64,
    result: Option<Result<MixerInputRetirement, MixerRetirementError>>,
    waker: Option<Waker>,
}

struct PreparedRetirement {
    source: Option<Box<dyn MixerInput>>,
    result: Result<MixerInputRetirement, MixerRetirementError>,
}

struct InputSlot {
    address: SlotAddress,
    word: AtomicU64,
    payload: UnsafeCell<ManuallyDrop<Option<Box<dyn MixerInput>>>>,
    forced: Mutex<ForcedWaiter>,
}

// The packed ACTIVE bit grants the render callback exclusive source access.
// Every Kira-side reference is Weak. Off-RT payload mutation is allowed only in
// phases in which ACTIVE cannot be newly acquired.
unsafe impl Sync for InputSlot {}

impl InputSlot {
    fn new(address: SlotAddress) -> Self {
        Self {
            address,
            word: AtomicU64::new(slot_word(1, SlotPhase::Free, 0)),
            payload: UnsafeCell::new(ManuallyDrop::new(None)),
            forced: Mutex::new(ForcedWaiter {
                generation: 1,
                result: None,
                waker: None,
            }),
        }
    }

    fn reserve(&self) -> Result<u64, MixerMutationError> {
        loop {
            let word = self.word.load(Ordering::Acquire);
            if slot_phase(word) != SlotPhase::Free {
                return Err(MixerMutationError::InternalInvariant);
            }
            let generation = slot_generation(word);
            if generation == 0 || generation > MAX_GENERATION {
                self.quarantine_word(word);
                return Err(MixerMutationError::GenerationExhausted);
            }
            if self
                .word
                .compare_exchange_weak(
                    word,
                    slot_word(generation, SlotPhase::Reserved, 0),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                let mut forced = lock_recover(&self.forced);
                forced.generation = generation;
                forced.result = None;
                forced.waker = None;
                return Ok(generation);
            }
        }
    }

    fn install(
        &self,
        generation: u64,
        source: Box<dyn MixerInput>,
    ) -> Result<(), Box<dyn MixerInput>> {
        let expected = slot_word(generation, SlotPhase::Reserved, 0);
        if self.word.load(Ordering::Acquire) != expected {
            return Err(source);
        }
        // SAFETY: Reserved cannot enter rendering. Start admission excludes the
        // service seal/forced-cleanup path until Installed is opened or dropped.
        let payload = unsafe { &mut **self.payload.get() };
        if payload.is_some() {
            self.quarantine_word(expected);
            return Err(source);
        }
        *payload = Some(source);
        if self
            .word
            .compare_exchange(
                expected,
                slot_word(generation, SlotPhase::Installed, HAS_PAYLOAD),
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(payload.take().expect("installed payload disappeared"));
        }
        Ok(())
    }

    fn open(&self, generation: u64) -> bool {
        self.word
            .compare_exchange(
                slot_word(generation, SlotPhase::Installed, HAS_PAYLOAD),
                slot_word(generation, SlotPhase::Running, HAS_PAYLOAD),
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn set_suspended(&self, generation: u64, suspended: bool) -> bool {
        loop {
            let word = self.word.load(Ordering::Acquire);
            if slot_generation(word) != generation || slot_phase(word) != SlotPhase::Running {
                return false;
            }
            let next = if suspended {
                word | SUSPENDED
            } else {
                word & !SUSPENDED
            };
            if next == word {
                return true;
            }
            if self
                .word
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn close(&self, generation: u64, failed: bool) -> bool {
        loop {
            let word = self.word.load(Ordering::Acquire);
            if slot_generation(word) != generation {
                return false;
            }
            match slot_phase(word) {
                SlotPhase::Reserved | SlotPhase::Installed | SlotPhase::Running => {}
                SlotPhase::Closing
                | SlotPhase::Retiring
                | SlotPhase::ForcedClean
                | SlotPhase::Quarantined => return true,
                SlotPhase::Free => return false,
            }
            let flags = (word & (ACTIVE | HAS_PAYLOAD))
                | if failed || word & FAILED != 0 {
                    FAILED
                } else {
                    0
                };
            let next = slot_word(generation, SlotPhase::Closing, flags);
            if self
                .word
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn try_enter(&self) -> SlotEntry {
        loop {
            let word = self.word.load(Ordering::Acquire);
            let generation = slot_generation(word);
            if slot_phase(word) != SlotPhase::Running || word & SUSPENDED != 0 {
                return SlotEntry::Silent;
            }
            if word & ACTIVE != 0 {
                let _ = self.close(generation, true);
                return SlotEntry::Silent;
            }
            if self
                .word
                .compare_exchange_weak(word, word | ACTIVE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return SlotEntry::Entered(generation);
            }
        }
    }

    fn is_retire_ready(&self, generation: u64) -> bool {
        let word = self.word.load(Ordering::Acquire);
        slot_generation(word) == generation
            && slot_phase(word) == SlotPhase::Closing
            && word & ACTIVE == 0
    }

    fn prepare_retirement(
        &self,
        generation: u64,
    ) -> Result<PreparedRetirement, MixerRetirementError> {
        let word = self.word.load(Ordering::Acquire);
        if slot_generation(word) != generation
            || slot_phase(word) != SlotPhase::Closing
            || word & ACTIVE != 0
        {
            return Err(MixerRetirementError::Structural);
        }
        if self
            .word
            .compare_exchange(
                word,
                slot_word(
                    generation,
                    SlotPhase::Retiring,
                    word & (FAILED | HAS_PAYLOAD),
                ),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(MixerRetirementError::Structural);
        }
        // SAFETY: Closing prevents future entry and ACTIVE==0 proves no current
        // renderer owns the payload.
        let source = unsafe { (**self.payload.get()).take() };
        let expected_payload = word & HAS_PAYLOAD != 0;
        let result = if expected_payload == source.is_some() {
            Ok(MixerInputRetirement {
                failed_before_retirement: word & FAILED != 0,
                source_destructor_panicked: false,
            })
        } else {
            Err(MixerRetirementError::Structural)
        };
        Ok(PreparedRetirement { source, result })
    }

    fn finish_reusable(
        &self,
        generation: u64,
        result: Result<MixerInputRetirement, MixerRetirementError>,
    ) -> Result<(MixerInputRetirement, u64), MixerRetirementError> {
        if let Err(error) = result {
            self.word.store(
                slot_word(generation, SlotPhase::Quarantined, FAILED),
                Ordering::Release,
            );
            return Err(error);
        }
        let retirement = result.expect("checked successful retirement");
        let word = self.word.load(Ordering::Acquire);
        if slot_generation(word) != generation || slot_phase(word) != SlotPhase::Retiring {
            self.quarantine_word(word);
            return Err(MixerRetirementError::Structural);
        }
        let Some(next_generation) = generation
            .checked_add(1)
            .filter(|next| *next <= MAX_GENERATION)
        else {
            self.word.store(
                slot_word(generation, SlotPhase::Quarantined, FAILED),
                Ordering::Release,
            );
            return Err(MixerRetirementError::GenerationExhausted);
        };
        self.word.store(
            slot_word(next_generation, SlotPhase::Free, 0),
            Ordering::Release,
        );
        Ok((retirement, next_generation))
    }

    fn finish_terminal(
        &self,
        generation: u64,
        result: Result<MixerInputRetirement, MixerRetirementError>,
    ) -> Result<MixerInputRetirement, MixerRetirementError> {
        let word = self.word.load(Ordering::Acquire);
        let result =
            if slot_generation(word) == generation && slot_phase(word) == SlotPhase::Retiring {
                result
            } else {
                Err(MixerRetirementError::Structural)
            };
        let flags = result.as_ref().map_or(FAILED, |retirement| {
            if retirement.failed_before_retirement {
                FAILED
            } else {
                0
            }
        });
        self.word.store(
            slot_word(
                generation,
                if result.is_ok() {
                    SlotPhase::ForcedClean
                } else {
                    SlotPhase::Quarantined
                },
                flags,
            ),
            Ordering::Release,
        );
        self.finish_forced(generation, result);
        result
    }

    fn prepare_forced_after_backend(&self) -> Option<(u64, PreparedRetirement)> {
        let word = self.word.load(Ordering::Acquire);
        let generation = slot_generation(word);
        match slot_phase(word) {
            SlotPhase::Free | SlotPhase::ForcedClean | SlotPhase::Retiring => return None,
            SlotPhase::Reserved | SlotPhase::Installed | SlotPhase::Running => {
                self.force_close_current();
                return self.prepare_forced_after_backend();
            }
            SlotPhase::Closing | SlotPhase::Quarantined => {}
        }
        if word & ACTIVE != 0 {
            self.force_quarantine();
            return None;
        }
        if self
            .word
            .compare_exchange(
                word,
                slot_word(
                    generation,
                    SlotPhase::Retiring,
                    word & (FAILED | HAS_PAYLOAD),
                ),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return self.prepare_forced_after_backend();
        }
        // SAFETY: the backend is gone, ACTIVE is clear, and Retiring excludes
        // all future render entry and payload mutation.
        let source = unsafe { (**self.payload.get()).take() };
        let expected_payload = word & HAS_PAYLOAD != 0;
        let result = if slot_phase(word) == SlotPhase::Quarantined {
            Err(MixerRetirementError::OwnerUncertain)
        } else if expected_payload == source.is_some() {
            Ok(MixerInputRetirement {
                failed_before_retirement: word & FAILED != 0,
                source_destructor_panicked: false,
            })
        } else {
            Err(MixerRetirementError::Structural)
        };
        Some((generation, PreparedRetirement { source, result }))
    }

    fn force_close_current(&self) {
        loop {
            let word = self.word.load(Ordering::Acquire);
            match slot_phase(word) {
                SlotPhase::Free
                | SlotPhase::ForcedClean
                | SlotPhase::Quarantined
                | SlotPhase::Retiring
                | SlotPhase::Closing => return,
                SlotPhase::Reserved | SlotPhase::Installed | SlotPhase::Running => {}
            }
            let next = slot_word(
                slot_generation(word),
                SlotPhase::Closing,
                word & (ACTIVE | FAILED | HAS_PAYLOAD),
            );
            if self
                .word
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn force_quarantine(&self) {
        let word = self.word.load(Ordering::Acquire);
        let generation = slot_generation(word);
        self.quarantine_word(word);
        self.finish_forced(generation, Err(MixerRetirementError::OwnerUncertain));
    }

    fn finish_forced(
        &self,
        generation: u64,
        result: Result<MixerInputRetirement, MixerRetirementError>,
    ) {
        let waker = {
            let mut forced = lock_recover(&self.forced);
            if forced.generation != generation {
                return;
            }
            forced.result = Some(result);
            forced.waker.take()
        };
        if let Some(waker) = waker {
            forget_panic(catch_unwind(AssertUnwindSafe(|| waker.wake())));
        }
    }

    fn poll_forced(
        &self,
        generation: u64,
        cx: &mut Context<'_>,
    ) -> Poll<Result<MixerInputRetirement, MixerRetirementError>> {
        let replacement = cx.waker().clone();
        let mut forced = lock_recover(&self.forced);
        if forced.generation != generation {
            return Poll::Ready(Err(MixerRetirementError::Structural));
        }
        if let Some(result) = forced.result {
            return Poll::Ready(result);
        }
        if forced
            .waker
            .as_ref()
            .is_none_or(|existing| !existing.will_wake(&replacement))
        {
            forced.waker = Some(replacement);
        }
        Poll::Pending
    }

    fn quarantine_word(&self, word: u64) {
        self.word.store(
            slot_word(
                slot_generation(word),
                SlotPhase::Quarantined,
                (word & (ACTIVE | HAS_PAYLOAD)) | FAILED,
            ),
            Ordering::Release,
        );
    }
}

enum SlotEntry {
    Silent,
    Entered(u64),
}

struct SlotRenderGuard<'a>(&'a InputSlot);

impl Drop for SlotRenderGuard<'_> {
    fn drop(&mut self) {
        self.0.word.fetch_and(!ACTIVE, Ordering::Release);
    }
}

struct SlotSoundData {
    slot: Weak<InputSlot>,
}

struct SlotSound {
    slot: Weak<InputSlot>,
}

impl SoundData for SlotSoundData {
    type Error = Infallible;
    type Handle = ();

    fn into_sound(self) -> Result<(Box<dyn Sound>, Self::Handle), Self::Error> {
        Ok((Box::new(SlotSound { slot: self.slot }), ()))
    }
}

impl Sound for SlotSound {
    fn process(&mut self, output: &mut [KiraFrame], _dt: f64, _info: &kira::info::Info) {
        output.fill(KiraFrame::ZERO);
        let Some(slot) = self.slot.upgrade() else {
            return;
        };
        let SlotEntry::Entered(generation) = slot.try_enter() else {
            return;
        };
        let _guard = SlotRenderGuard(&slot);
        // SAFETY: exact Running+ACTIVE grants exclusive payload access until
        // `_guard` clears ACTIVE with Release ordering.
        let source = unsafe { &mut **slot.payload.get() };
        let Some(source) = source.as_mut() else {
            let _ = slot.close(generation, true);
            return;
        };
        let mut rendered = [MixerFrame::ZERO; INTERNAL_BUFFER_FRAMES];
        let Some(rendered) = rendered.get_mut(..output.len()) else {
            let _ = slot.close(generation, true);
            return;
        };
        match catch_unwind(AssertUnwindSafe(|| source.render(rendered))) {
            Ok(MixerInputStatus::Active) => {
                copy_mixer_frames(output, rendered);
            }
            Ok(MixerInputStatus::Finished) => {
                copy_mixer_frames(output, rendered);
                let _ = slot.close(generation, false);
            }
            Err(payload) => {
                let _ = slot.close(generation, true);
                std::mem::forget(payload);
            }
        }
    }

    fn finished(&self) -> bool {
        false
    }
}

fn copy_mixer_frames(output: &mut [KiraFrame], rendered: &[MixerFrame]) {
    for (output, rendered) in output.iter_mut().zip(rendered) {
        *output = KiraFrame {
            left: rendered.left,
            right: rendered.right,
        };
    }
}

struct CheckedBackend<B>(B);
struct CheckedBackendSettings<S> {
    inner: S,
    expected_sample_rate: u32,
}
enum CheckedBackendError<E> {
    Backend(E),
    SampleRateMismatch { expected: u32, actual: u32 },
}

impl<B: Backend> Backend for CheckedBackend<B> {
    type Settings = CheckedBackendSettings<B::Settings>;
    type Error = CheckedBackendError<B::Error>;

    fn setup(
        settings: Self::Settings,
        internal_buffer_size: usize,
    ) -> Result<(Self, u32), Self::Error> {
        let (backend, actual) =
            B::setup(settings.inner, internal_buffer_size).map_err(CheckedBackendError::Backend)?;
        if actual != settings.expected_sample_rate {
            return Err(CheckedBackendError::SampleRateMismatch {
                expected: settings.expected_sample_rate,
                actual,
            });
        }
        Ok((Self(backend), actual))
    }

    fn start(&mut self, renderer: Renderer) -> Result<(), Self::Error> {
        self.0.start(renderer).map_err(CheckedBackendError::Backend)
    }
}

struct SlotPool {
    free: Vec<Arc<InputSlot>>,
    inventory: Vec<Weak<InputSlot>>,
}

impl SlotPool {
    fn reserve(&mut self) -> Result<ReservedRecord, MixerMutationError> {
        let slot = self.free.pop().ok_or(MixerMutationError::InputCapacity)?;
        let generation = slot.reserve()?;
        Ok(ReservedRecord { slot, generation })
    }

    fn restore(&mut self, record: ReservedRecord) -> Result<(), MixerMutationError> {
        let word = record.slot.word.load(Ordering::Acquire);
        if slot_phase(word) != SlotPhase::Reserved || slot_generation(word) != record.generation {
            record.slot.quarantine_word(word);
            return Err(MixerMutationError::InternalInvariant);
        }
        let next_generation = record
            .generation
            .checked_add(1)
            .filter(|next| *next <= MAX_GENERATION)
            .ok_or(MixerMutationError::GenerationExhausted)?;
        record.slot.word.store(
            slot_word(next_generation, SlotPhase::Free, 0),
            Ordering::Release,
        );
        self.free.push(record.slot);
        Ok(())
    }
}

struct SessionTracks {
    _root: TrackHandle,
    tracks: [TrackHandle; SESSION_BUS_COUNT],
    pools: [SlotPool; SESSION_BUS_COUNT],
    key: SessionKey,
}

impl SessionTracks {
    fn track_mut(&mut self, bus: SessionBus) -> &mut TrackHandle {
        &mut self.tracks[bus.ordinal()]
    }
    fn pool_mut(&mut self, bus: SessionBus) -> &mut SlotPool {
        &mut self.pools[bus.ordinal()]
    }
}

struct MixerCore<B: Backend> {
    manager: Option<AudioManager<CheckedBackend<B>>>,
    sessions: HashMap<AudioSessionId, SessionTracks>,
    max_sessions: usize,
    inputs_per_bus: usize,
    next_session_generation: u64,
    cleanup_clean: bool,
}

impl<B: Backend> MixerCore<B> {
    fn with_limits(
        backend_settings: B::Settings,
        format: MixerFormat,
        max_sessions: usize,
        inputs_per_bus: usize,
    ) -> Result<Self, MixerStartError<B::Error>> {
        let manager = AudioManager::new(AudioManagerSettings {
            capacities: Capacities {
                sub_track_capacity: max_sessions,
                send_track_capacity: 0,
                clock_capacity: 0,
                modulator_capacity: 0,
                listener_capacity: 0,
            },
            main_track_builder: MainTrackBuilder::new().sound_capacity(0),
            internal_buffer_size: INTERNAL_BUFFER_FRAMES,
            backend_settings: CheckedBackendSettings {
                inner: backend_settings,
                expected_sample_rate: format.sample_rate,
            },
        })
        .map_err(|error| match error {
            CheckedBackendError::Backend(error) => MixerStartError::Backend(error),
            CheckedBackendError::SampleRateMismatch { expected, actual } => {
                MixerStartError::SampleRateMismatch { expected, actual }
            }
        })?;
        Ok(Self {
            manager: Some(manager),
            sessions: HashMap::with_capacity(max_sessions),
            max_sessions,
            inputs_per_bus,
            next_session_generation: 1,
            cleanup_clean: true,
        })
    }

    fn add_session(&mut self, id: AudioSessionId) -> Result<SessionKey, MixerMutationError> {
        if self.sessions.contains_key(&id) {
            return Err(MixerMutationError::DuplicateSession);
        }
        if self.sessions.len() == self.max_sessions {
            return Err(MixerMutationError::SessionCapacity);
        }
        let generation = self.next_session_generation;
        self.next_session_generation = generation
            .checked_add(1)
            .ok_or(MixerMutationError::GenerationExhausted)?;
        let key = SessionKey { id, generation };
        let manager = self
            .manager
            .as_mut()
            .ok_or(MixerMutationError::InternalInvariant)?;
        let mut root = manager
            .add_sub_track(
                TrackBuilder::new()
                    .sound_capacity(0)
                    .sub_track_capacity(SESSION_BUS_COUNT),
            )
            .map_err(|_| MixerMutationError::InternalInvariant)?;

        let mut build_bus =
            |bus: SessionBus| -> Result<(TrackHandle, SlotPool), MixerMutationError> {
                let mut track = root
                    .add_sub_track(
                        TrackBuilder::new()
                            .sound_capacity(self.inputs_per_bus)
                            .sub_track_capacity(0),
                    )
                    .map_err(|_| MixerMutationError::InternalInvariant)?;
                let mut free = Vec::with_capacity(self.inputs_per_bus);
                let mut inventory = Vec::with_capacity(self.inputs_per_bus);
                for index in 0..self.inputs_per_bus {
                    let slot = Arc::new(InputSlot::new(SlotAddress {
                        session: key,
                        bus,
                        index,
                    }));
                    track
                        .play(SlotSoundData {
                            slot: Arc::downgrade(&slot),
                        })
                        .map_err(|_| MixerMutationError::InternalInvariant)?;
                    inventory.push(Arc::downgrade(&slot));
                    free.push(slot);
                }
                Ok((track, SlotPool { free, inventory }))
            };

        let (script, script_pool) = build_bus(SessionBus::Script)?;
        let (native, native_pool) = build_bus(SessionBus::Native)?;
        let (speech, speech_pool) = build_bus(SessionBus::Speech)?;
        self.sessions.insert(
            id,
            SessionTracks {
                _root: root,
                tracks: [script, native, speech],
                pools: [script_pool, native_pool, speech_pool],
                key,
            },
        );
        Ok(key)
    }

    fn reserve(
        &mut self,
        key: SessionKey,
        bus: SessionBus,
    ) -> Result<ReservedRecord, MixerMutationError> {
        self.sessions
            .get_mut(&key.id)
            .filter(|session| session.key == key)
            .ok_or(MixerMutationError::UnknownSession)?
            .pool_mut(bus)
            .reserve()
    }

    fn restore_reserved(&mut self, record: ReservedRecord) -> Result<(), MixerMutationError> {
        let address = record.slot.address;
        self.sessions
            .get_mut(&address.session.id)
            .filter(|session| session.key == address.session)
            .ok_or(MixerMutationError::UnknownSession)?
            .pool_mut(address.bus)
            .restore(record)
    }

    fn recycle(
        &mut self,
        record: ReservedRecord,
        next_generation: u64,
    ) -> Result<(), MixerRetirementError> {
        let address = record.slot.address;
        let word = record.slot.word.load(Ordering::Acquire);
        if slot_generation(word) != next_generation || slot_phase(word) != SlotPhase::Free {
            record.slot.quarantine_word(word);
            return Err(MixerRetirementError::Structural);
        }
        let Some(session) = self
            .sessions
            .get_mut(&address.session.id)
            .filter(|session| session.key == address.session)
        else {
            record.slot.quarantine_word(word);
            return Err(MixerRetirementError::Structural);
        };
        session.pool_mut(address.bus).free.push(record.slot);
        Ok(())
    }

    fn set_gain(
        &mut self,
        key: SessionKey,
        bus: SessionBus,
        linear: f32,
    ) -> Result<(), MixerMutationError> {
        let session = self
            .sessions
            .get_mut(&key.id)
            .filter(|session| session.key == key)
            .ok_or(MixerMutationError::UnknownSession)?;
        let decibels = if linear == 0.0 {
            Decibels::SILENCE
        } else {
            Decibels(20.0 * linear.log10())
        };
        session.track_mut(bus).set_volume(
            decibels,
            Tween {
                duration: Duration::ZERO,
                ..Tween::default()
            },
        );
        Ok(())
    }

    fn close_all_slots(&self) {
        self.for_each_slot(|slot| slot.force_close_current());
    }

    fn prepare_forced_jobs(&self) -> Vec<CleanupJob> {
        let mut jobs = Vec::new();
        self.for_each_slot(|slot| {
            if let Some((generation, prepared)) = slot.prepare_forced_after_backend() {
                jobs.push(CleanupJob {
                    record: ReservedRecord {
                        slot: Arc::clone(slot),
                        generation,
                    },
                    prepared,
                    completion: None,
                    terminal: true,
                });
            }
        });
        jobs
    }

    fn force_quarantine_all(&self) {
        self.for_each_slot(|slot| slot.force_quarantine());
    }

    fn for_each_slot(&self, mut action: impl FnMut(&Arc<InputSlot>)) {
        for session in self.sessions.values() {
            for pool in &session.pools {
                for slot in &pool.inventory {
                    if let Some(slot) = slot.upgrade() {
                        action(&slot);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MixerMutationError {
    DuplicateSession,
    SessionCapacity,
    UnknownSession,
    InputCapacity,
    GenerationExhausted,
    InternalInvariant,
}

struct ReservedRecord {
    slot: Arc<InputSlot>,
    generation: u64,
}

struct RetirementRequest {
    record: ReservedRecord,
    completion: oneshot::Sender<Result<MixerInputRetirement, MixerRetirementError>>,
}

struct CleanupJob {
    record: ReservedRecord,
    prepared: PreparedRetirement,
    completion: Option<oneshot::Sender<Result<MixerInputRetirement, MixerRetirementError>>>,
    terminal: bool,
}

struct CleanupResult {
    record: ReservedRecord,
    result: Result<MixerInputRetirement, MixerRetirementError>,
    completion: Option<oneshot::Sender<Result<MixerInputRetirement, MixerRetirementError>>>,
    terminal: bool,
}

enum OwnerCommand {
    AddSession(
        AudioSessionId,
        SyncSender<Result<SessionKey, MixerMutationError>>,
    ),
    Reserve(
        SessionKey,
        SessionBus,
        SyncSender<Result<ReservedRecord, MixerMutationError>>,
    ),
    SetGain(
        SessionKey,
        SessionBus,
        f32,
        SyncSender<Result<(), MixerMutationError>>,
    ),
    Shutdown,
}

struct GateState {
    sealed: bool,
    accepting_retirements: bool,
    start_admissions: usize,
}

struct ControlInner {
    gate: Mutex<GateState>,
    gate_drained: Condvar,
    commands: SyncSender<OwnerCommand>,
    retirements: SyncSender<RetirementRequest>,
    format: MixerFormat,
}

impl ControlInner {
    fn admit_start(self: &Arc<Self>) -> Result<StartAdmission, MixerControlError> {
        let mut gate = self
            .gate
            .lock()
            .map_err(|_| MixerControlError::OwnerStopped)?;
        if gate.sealed {
            return Err(MixerControlError::OwnerStopped);
        }
        gate.start_admissions = gate
            .start_admissions
            .checked_add(1)
            .ok_or(MixerControlError::InternalInvariant)?;
        drop(gate);
        Ok(StartAdmission {
            control: Arc::clone(self),
        })
    }

    fn submit_retirement(
        &self,
        request: RetirementRequest,
    ) -> Result<(), (MixerRetirementError, RetirementRequest)> {
        let Ok(gate) = self.gate.lock() else {
            return Err((MixerRetirementError::OwnerUncertain, request));
        };
        if !gate.accepting_retirements {
            return Err((MixerRetirementError::OwnerUncertain, request));
        }
        let result = match self.retirements.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(request)) => {
                Err((MixerRetirementError::QueueInvariant, request))
            }
            Err(TrySendError::Disconnected(request)) => {
                Err((MixerRetirementError::OwnerUncertain, request))
            }
        };
        drop(gate);
        result
    }

    fn seal(&self) -> Result<(), MixerControlError> {
        let mut gate = self
            .gate
            .lock()
            .map_err(|_| MixerControlError::OwnerStopped)?;
        gate.sealed = true;
        gate.accepting_retirements = false;
        while gate.start_admissions != 0 {
            gate = self
                .gate_drained
                .wait(gate)
                .map_err(|_| MixerControlError::OwnerStopped)?;
        }
        Ok(())
    }
}

struct StartAdmission {
    control: Arc<ControlInner>,
}

impl Drop for StartAdmission {
    fn drop(&mut self) {
        let mut gate = lock_recover(&self.control.gate);
        debug_assert!(gate.start_admissions > 0);
        gate.start_admissions = gate.start_admissions.saturating_sub(1);
        if gate.start_admissions == 0 {
            self.control.gate_drained.notify_all();
        }
    }
}

struct OwnerStatus {
    retired: AtomicBool,
    clean: AtomicBool,
}

impl OwnerStatus {
    fn new() -> Self {
        Self {
            retired: AtomicBool::new(false),
            clean: AtomicBool::new(false),
        }
    }
}

/// Errors reported by the bounded mixer control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerControlError {
    /// The fixed command queue is currently full.
    Saturated,
    /// The mixer owner has already stopped or begun sealing.
    OwnerStopped,
    /// The session identity is already present.
    DuplicateSession,
    /// The fixed session capacity has been reached.
    SessionCapacity,
    /// The requested session does not exist.
    UnknownSession,
    /// The selected bus has reached its fixed input capacity.
    InputCapacity,
    /// A finite nonnegative linear gain was not supplied.
    InvalidGain,
    /// A non-wrapping identity was exhausted.
    GenerationExhausted,
    /// A fixed-capacity invariant inside the prebuilt topology failed.
    InternalInvariant,
}

impl From<MixerMutationError> for MixerControlError {
    fn from(value: MixerMutationError) -> Self {
        match value {
            MixerMutationError::DuplicateSession => Self::DuplicateSession,
            MixerMutationError::SessionCapacity => Self::SessionCapacity,
            MixerMutationError::UnknownSession => Self::UnknownSession,
            MixerMutationError::InputCapacity => Self::InputCapacity,
            MixerMutationError::GenerationExhausted => Self::GenerationExhausted,
            MixerMutationError::InternalInvariant => Self::InternalInvariant,
        }
    }
}

/// Failure to construct the dedicated mixer owner.
#[derive(Debug)]
pub enum MixerStartError<E> {
    /// The requested fixed mixer sample rate was zero.
    InvalidSampleRate,
    /// The owner thread could not be created.
    Thread(std::io::Error),
    /// The selected Kira backend could not be created or started.
    Backend(E),
    /// The backend's actual sample rate did not match the fixed contract.
    SampleRateMismatch {
        /// Requested and published rate.
        expected: u32,
        /// Rate returned by `Backend::setup`.
        actual: u32,
    },
    /// The owner terminated before reporting its startup result.
    OwnerStopped,
}

/// Result of explicitly joining the mixer owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerShutdown {
    /// Whether Kira/backend retirement and forced input cleanup were proven.
    pub clean: bool,
}

/// Result of an off-render mixer-input retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerInputRetirement {
    /// Whether rendering or the source callback had failed closed.
    pub failed_before_retirement: bool,
    /// Whether the contained source's destructor panicked.
    pub source_destructor_panicked: bool,
}

impl MixerInputRetirement {
    /// Returns `true` when rendering and source destruction were both clean.
    #[must_use]
    pub const fn is_clean(self) -> bool {
        !self.failed_before_retirement && !self.source_destructor_panicked
    }
}

/// Failure to prove or complete one logical input retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerRetirementError {
    /// The fixed ownership queue invariant was violated.
    QueueInvariant,
    /// The mixer owner or backend retired without a clean proof.
    OwnerUncertain,
    /// A generation or phase invariant was violated.
    Structural,
    /// The non-wrapping slot generation was exhausted.
    GenerationExhausted,
    /// The source destructor panicked; the slot was quarantined.
    SourceDestructorPanicked,
}

enum ShutdownState {
    Accepted(oneshot::Receiver<Result<MixerInputRetirement, MixerRetirementError>>),
    Forced {
        slot: ManuallyDrop<Arc<InputSlot>>,
        generation: u64,
    },
    RetainedError {
        _slot: ManuallyDrop<Arc<InputSlot>>,
        error: MixerRetirementError,
    },
    Finished,
}

/// Single-owner future for an already-committed logical input retirement.
#[must_use = "mixer input retirement must be polled to observe its outcome"]
pub struct MixerInputShutdown {
    state: ShutdownState,
}

impl fmt::Debug for MixerInputShutdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MixerInputShutdown")
            .finish_non_exhaustive()
    }
}

impl Future for MixerInputShutdown {
    type Output = Result<MixerInputRetirement, MixerRetirementError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.state {
            ShutdownState::Accepted(receiver) => match Pin::new(receiver).poll(cx) {
                Poll::Ready(Ok(result)) => {
                    self.state = ShutdownState::Finished;
                    Poll::Ready(result)
                }
                Poll::Ready(Err(_)) => {
                    self.state = ShutdownState::Finished;
                    Poll::Ready(Err(MixerRetirementError::OwnerUncertain))
                }
                Poll::Pending => Poll::Pending,
            },
            ShutdownState::Forced { slot, generation } => match slot.poll_forced(*generation, cx) {
                Poll::Ready(result) => {
                    if matches!(
                        result,
                        Ok(_)
                            | Err(MixerRetirementError::SourceDestructorPanicked
                                | MixerRetirementError::GenerationExhausted)
                    ) {
                        // SAFETY: the source was removed on every contained terminal path.
                        unsafe { ManuallyDrop::drop(slot) };
                    }
                    self.state = ShutdownState::Finished;
                    Poll::Ready(result)
                }
                Poll::Pending => Poll::Pending,
            },
            ShutdownState::RetainedError { error, .. } => Poll::Ready(Err(*error)),
            ShutdownState::Finished => Poll::Ready(Err(MixerRetirementError::Structural)),
        }
    }
}

fn submit_shutdown(
    slot: Arc<InputSlot>,
    generation: u64,
    control: &Weak<ControlInner>,
) -> MixerInputShutdown {
    let initial = slot.word.load(Ordering::Acquire);
    if slot_generation(initial) == generation
        && matches!(
            slot_phase(initial),
            SlotPhase::Retiring | SlotPhase::ForcedClean | SlotPhase::Quarantined
        )
        && control.upgrade().is_none()
    {
        return MixerInputShutdown {
            state: ShutdownState::Forced {
                slot: ManuallyDrop::new(slot),
                generation,
            },
        };
    }
    if !slot.close(generation, false) {
        return MixerInputShutdown {
            state: ShutdownState::RetainedError {
                _slot: ManuallyDrop::new(slot),
                error: MixerRetirementError::Structural,
            },
        };
    }
    let word = slot.word.load(Ordering::Acquire);
    if slot_generation(word) != generation || slot_phase(word) != SlotPhase::Closing {
        return MixerInputShutdown {
            state: ShutdownState::RetainedError {
                _slot: ManuallyDrop::new(slot),
                error: MixerRetirementError::Structural,
            },
        };
    }
    let Some(control) = control.upgrade() else {
        return MixerInputShutdown {
            state: ShutdownState::Forced {
                slot: ManuallyDrop::new(slot),
                generation,
            },
        };
    };
    let (completion, receiver) = oneshot::channel();
    let request = RetirementRequest {
        record: ReservedRecord { slot, generation },
        completion,
    };
    match control.submit_retirement(request) {
        Ok(()) => MixerInputShutdown {
            state: ShutdownState::Accepted(receiver),
        },
        Err((MixerRetirementError::OwnerUncertain, request)) => MixerInputShutdown {
            state: ShutdownState::Forced {
                slot: ManuallyDrop::new(request.record.slot),
                generation,
            },
        },
        Err((error, request)) => MixerInputShutdown {
            state: ShutdownState::RetainedError {
                _slot: ManuallyDrop::new(request.record.slot),
                error,
            },
        },
    }
}

/// A real, generation-stamped mixer slot reserved before a callback exists.
pub struct MixerInputReservation {
    slot: ManuallyDrop<Arc<InputSlot>>,
    generation: u64,
    control: Weak<ControlInner>,
}

impl fmt::Debug for MixerInputReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MixerInputReservation")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl MixerInputReservation {
    /// Abort this silent reservation and return a waking retirement future.
    #[must_use = "aborting a reservation returns the proof-bearing cleanup future"]
    pub fn abort(mut self) -> MixerInputShutdown {
        // SAFETY: this consumes the only stored strong owner.
        let slot = unsafe { ManuallyDrop::take(&mut self.slot) };
        let generation = self.generation;
        let control = self.control.clone();
        std::mem::forget(self);
        submit_shutdown(slot, generation, &control)
    }

    /// Install a preallocated input while holding exact start admission.
    ///
    /// # Errors
    ///
    /// Returns both the unchanged reservation and source when the owner is
    /// sealed/gone or the exact slot cannot be installed.
    fn install_preboxed(
        self,
        source: Box<dyn MixerInput>,
    ) -> Result<InstalledMixerInput, MixerInputInstallFailure> {
        let Some(control) = self.control.upgrade() else {
            return Err(MixerInputInstallFailure::new(
                MixerControlError::OwnerStopped,
                self,
                source,
            ));
        };
        let admission = match control.admit_start() {
            Ok(admission) => admission,
            Err(error) => return Err(MixerInputInstallFailure::new(error, self, source)),
        };
        if let Err(source) = self.slot.install(self.generation, source) {
            return Err(MixerInputInstallFailure::new(
                MixerControlError::InternalInvariant,
                self,
                source,
            ));
        }
        let mut this = self;
        // SAFETY: ownership transfers into Installed and Drop is suppressed.
        let slot = unsafe { ManuallyDrop::take(&mut this.slot) };
        let generation = this.generation;
        let control = this.control.clone();
        std::mem::forget(this);
        Ok(InstalledMixerInput {
            slot: ManuallyDrop::new(slot),
            generation,
            control,
            admission: Some(admission),
        })
    }

    /// Install and release-open a preallocated input as one admitted transaction.
    ///
    /// The start admission cannot escape this call, so service sealing cannot
    /// wait on an abandoned pre-open value.
    ///
    /// # Errors
    ///
    /// Returns [`MixerInputStartFailure::Rejected`] with the unchanged source
    /// and reservation when admission fails. A structural post-install failure
    /// returns [`MixerInputStartFailure::Cleanup`] with sole cleanup ownership.
    pub fn start_preboxed(
        self,
        source: Box<dyn MixerInput>,
    ) -> Result<RunningMixerInput, MixerInputStartFailure> {
        self.install_preboxed(source)
            .map_err(MixerInputStartFailure::Rejected)?
            .open()
            .map_err(|failure| MixerInputStartFailure::Cleanup(failure.shutdown()))
    }
}

impl Drop for MixerInputReservation {
    fn drop(&mut self) {
        // SAFETY: the field is initialized on every ordinary Drop path.
        let slot = unsafe { ManuallyDrop::take(&mut self.slot) };
        let shutdown = submit_shutdown(slot, self.generation, &self.control);
        drop(shutdown);
    }
}

/// Installation failure retaining both the reserved slot and source.
pub struct MixerInputInstallFailure {
    error: MixerControlError,
    reservation: ManuallyDrop<MixerInputReservation>,
    source: ManuallyDrop<Box<dyn MixerInput>>,
}

impl fmt::Debug for MixerInputInstallFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MixerInputInstallFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl MixerInputInstallFailure {
    fn new(
        error: MixerControlError,
        reservation: MixerInputReservation,
        source: Box<dyn MixerInput>,
    ) -> Self {
        Self {
            error,
            reservation: ManuallyDrop::new(reservation),
            source: ManuallyDrop::new(source),
        }
    }

    /// Stable control-plane cause.
    #[must_use]
    pub const fn error(&self) -> MixerControlError {
        self.error
    }

    /// Recover the exact reservation and source for caller-owned cleanup.
    #[must_use]
    pub fn into_parts(
        mut self,
    ) -> (
        MixerControlError,
        MixerInputReservation,
        Box<dyn MixerInput>,
    ) {
        // SAFETY: both fields are taken exactly once and Drop is suppressed.
        let reservation = unsafe { ManuallyDrop::take(&mut self.reservation) };
        let source = unsafe { ManuallyDrop::take(&mut self.source) };
        let error = self.error;
        (error, reservation, source)
    }
}

/// A failed synchronous start transaction with exact remaining ownership.
pub enum MixerInputStartFailure {
    /// Admission failed before the callback became renderable.
    Rejected(MixerInputInstallFailure),
    /// A structural post-install failure owns the committed cleanup future.
    Cleanup(MixerInputShutdown),
}

impl fmt::Debug for MixerInputStartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(failure) => formatter.debug_tuple("Rejected").field(failure).finish(),
            Self::Cleanup(_) => formatter.debug_tuple("Cleanup").finish_non_exhaustive(),
        }
    }
}

/// Input installed in a permanent Kira slot but still prestart-silent.
struct InstalledMixerInput {
    slot: ManuallyDrop<Arc<InputSlot>>,
    generation: u64,
    control: Weak<ControlInner>,
    admission: Option<StartAdmission>,
}

impl fmt::Debug for InstalledMixerInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstalledMixerInput")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl InstalledMixerInput {
    /// Release-open the exact installed generation.
    ///
    /// # Errors
    ///
    /// A failure is structural because the retained start admission prevents a
    /// normal service seal from racing this transition. The returned owner is
    /// already closed and still owns cleanup.
    fn open(mut self) -> Result<RunningMixerInput, MixerInputOpenFailure> {
        // SAFETY: ownership transfers out of the consumed Installed value.
        let slot = unsafe { ManuallyDrop::take(&mut self.slot) };
        let generation = self.generation;
        let control = self.control.clone();
        if !slot.open(generation) {
            let _ = slot.close(generation, true);
            self.admission.take();
            std::mem::forget(self);
            return Err(MixerInputOpenFailure {
                owner: RunningMixerInput {
                    slot: ManuallyDrop::new(slot),
                    generation,
                    control,
                },
            });
        }
        // Releasing this admission is the sole operation after publication.
        self.admission.take();
        std::mem::forget(self);
        Ok(RunningMixerInput {
            slot: ManuallyDrop::new(slot),
            generation,
            control,
        })
    }
}

impl Drop for InstalledMixerInput {
    fn drop(&mut self) {
        // SAFETY: initialized on every ordinary Drop path.
        let slot = unsafe { ManuallyDrop::take(&mut self.slot) };
        let _ = slot.close(self.generation, false);
        self.admission.take();
        let shutdown = submit_shutdown(slot, self.generation, &self.control);
        drop(shutdown);
    }
}

/// Structural open failure retaining exact cleanup ownership.
#[derive(Debug)]
struct MixerInputOpenFailure {
    owner: RunningMixerInput,
}

impl MixerInputOpenFailure {
    /// Close and submit the failed installation for retirement.
    fn shutdown(self) -> MixerInputShutdown {
        self.owner.shutdown()
    }
}

/// Sole strong owner of one running logical mixer input.
pub struct RunningMixerInput {
    slot: ManuallyDrop<Arc<InputSlot>>,
    generation: u64,
    control: Weak<ControlInner>,
}

impl fmt::Debug for RunningMixerInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunningMixerInput")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl RunningMixerInput {
    /// Silence this logical input from the next render entry onward.
    #[must_use]
    pub fn suspend(&self) -> bool {
        self.slot.set_suspended(self.generation, true)
    }
    /// Reopen a suspended logical input from the next render entry onward.
    #[must_use]
    pub fn resume(&self) -> bool {
        self.slot.set_suspended(self.generation, false)
    }
    /// Absorbingly close and commit off-render retirement.
    #[must_use = "logical shutdown returns the proof-bearing cleanup future"]
    pub fn shutdown(mut self) -> MixerInputShutdown {
        // SAFETY: the consumed owner transfers its only strong Arc.
        let slot = unsafe { ManuallyDrop::take(&mut self.slot) };
        let generation = self.generation;
        let control = self.control.clone();
        std::mem::forget(self);
        submit_shutdown(slot, generation, &control)
    }
    /// Whether rendering or the installed callback failed closed.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        let word = self.slot.word.load(Ordering::Acquire);
        slot_generation(word) == self.generation && word & FAILED != 0
    }
}

impl Drop for RunningMixerInput {
    fn drop(&mut self) {
        // SAFETY: initialized on every ordinary Drop path.
        let slot = unsafe { ManuallyDrop::take(&mut self.slot) };
        let shutdown = submit_shutdown(slot, self.generation, &self.control);
        drop(shutdown);
    }
}

#[derive(Clone)]
struct MixerBusHandle {
    control: Weak<ControlInner>,
    session: SessionKey,
    bus: SessionBus,
}

impl MixerBusHandle {
    fn try_reserve_input(&self) -> Result<MixerInputReservation, MixerControlError> {
        let control = self
            .control
            .upgrade()
            .ok_or(MixerControlError::OwnerStopped)?;
        let record = send_request(&control, |response| {
            OwnerCommand::Reserve(self.session, self.bus, response)
        })?
        .map_err(MixerControlError::from)?;
        Ok(MixerInputReservation {
            slot: ManuallyDrop::new(record.slot),
            generation: record.generation,
            control: self.control.clone(),
        })
    }

    fn set_gain(&self, linear: f32) -> Result<(), MixerControlError> {
        if !linear.is_finite() || linear < 0.0 {
            return Err(MixerControlError::InvalidGain);
        }
        let control = self
            .control
            .upgrade()
            .ok_or(MixerControlError::OwnerStopped)?;
        send_request(&control, |response| {
            OwnerCommand::SetGain(self.session, self.bus, linear, response)
        })?
        .map_err(Into::into)
    }

    fn format(&self) -> Result<MixerFormat, MixerControlError> {
        self.control
            .upgrade()
            .map(|control| control.format)
            .ok_or(MixerControlError::OwnerStopped)
    }
}

macro_rules! typed_bus_handle {
    ($name:ident) => {
        #[doc = "A cloneable, scoped handle to one exact session bus."]
        #[derive(Clone)]
        pub struct $name(MixerBusHandle);

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .finish_non_exhaustive()
            }
        }

        impl $name {
            /// Reserve one real, preinstalled physical mixer slot.
            ///
            /// # Errors
            ///
            /// Returns a typed bounded-control, session, or capacity failure.
            pub fn try_reserve_input(&self) -> Result<MixerInputReservation, MixerControlError> {
                self.0.try_reserve_input()
            }
            /// Set this bus's finite linear gain through the mixer owner.
            ///
            /// # Errors
            ///
            /// Returns a typed validation or bounded-control failure.
            pub fn set_gain(&self, linear: f32) -> Result<(), MixerControlError> {
                self.0.set_gain(linear)
            }
            /// Exact format verified when the mixer backend started.
            ///
            /// # Errors
            ///
            /// Returns [`MixerControlError::OwnerStopped`] after owner teardown.
            pub fn format(&self) -> Result<MixerFormat, MixerControlError> {
                self.0.format()
            }
        }
    };
}

typed_bus_handle!(MixerScriptBusHandle);
typed_bus_handle!(MixerNativeBusHandle);
typed_bus_handle!(MixerSpeechBusHandle);

/// Scoped handle for one fixed session subtree.
#[derive(Clone)]
pub struct MixerSessionHandle {
    control: Weak<ControlInner>,
    key: SessionKey,
}

impl fmt::Debug for MixerSessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MixerSessionHandle")
            .field("id", &self.key.id)
            .finish_non_exhaustive()
    }
}

impl MixerSessionHandle {
    fn bus(&self, bus: SessionBus) -> MixerBusHandle {
        MixerBusHandle {
            control: self.control.clone(),
            session: self.key,
            bus,
        }
    }
    /// Exact Script-bus capability for Web Audio integration.
    #[must_use]
    pub fn script_bus(&self) -> MixerScriptBusHandle {
        MixerScriptBusHandle(self.bus(SessionBus::Script))
    }
    /// Exact Native-bus capability for client sounds.
    #[must_use]
    pub fn native_bus(&self) -> MixerNativeBusHandle {
        MixerNativeBusHandle(self.bus(SessionBus::Native))
    }
    /// Exact Speech-bus capability for accessibility playback.
    #[must_use]
    pub fn speech_bus(&self) -> MixerSpeechBusHandle {
        MixerSpeechBusHandle(self.bus(SessionBus::Speech))
    }
}

/// Bounded control plus unique joined ownership of one process mixer.
///
/// This join authority is intentionally thread-affine (`!Send` and `!Sync`).
/// Cloneable scoped bus handles carry cross-thread control. Keeping the service
/// thread-affine also prevents a [`MixerInput`] destructor from owning and
/// synchronously joining the owner that is waiting for that destructor.
pub struct MixerService<B: Backend> {
    control: Option<Arc<ControlInner>>,
    owner: Option<JoinHandle<()>>,
    owner_status: Arc<OwnerStatus>,
    format: MixerFormat,
    _backend: PhantomData<fn() -> B>,
    _thread_affinity: PhantomData<Rc<()>>,
}

impl<B: Backend> fmt::Debug for MixerService<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MixerService")
            .field("owner_live", &self.owner.is_some())
            .finish_non_exhaustive()
    }
}

impl<B> MixerService<B>
where
    B: Backend + 'static,
    B::Settings: Send + 'static,
    B::Error: Send + 'static,
{
    /// Create and verify the backend and mixer topology on a dedicated owner.
    ///
    /// # Errors
    ///
    /// Returns a typed spawn/backend/rate/owner failure. A startup failure is
    /// joined before this function returns.
    pub fn start(
        backend_settings: B::Settings,
        sample_rate: u32,
    ) -> Result<Self, MixerStartError<B::Error>> {
        if sample_rate == 0 {
            return Err(MixerStartError::InvalidSampleRate);
        }
        Self::start_with_limits(
            backend_settings,
            MixerFormat { sample_rate },
            MAX_SESSIONS,
            INPUTS_PER_BUS,
        )
    }

    fn start_with_limits(
        backend_settings: B::Settings,
        format: MixerFormat,
        max_sessions: usize,
        inputs_per_bus: usize,
    ) -> Result<Self, MixerStartError<B::Error>> {
        let (commands, command_receiver) = mpsc::sync_channel(CONTROL_QUEUE_CAPACITY);
        let (retirements, retirement_receiver) = mpsc::sync_channel(RETIREMENT_QUEUE_CAPACITY);
        let control = Arc::new(ControlInner {
            gate: Mutex::new(GateState {
                sealed: false,
                accepting_retirements: true,
                start_admissions: 0,
            }),
            gate_drained: Condvar::new(),
            commands,
            retirements,
            format,
        });
        let owner_status = Arc::new(OwnerStatus::new());
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let control_weak = Arc::downgrade(&control);
        let owner_status_thread = Arc::clone(&owner_status);
        let owner = thread::Builder::new()
            .name("smudgy-audio-owner".into())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run_owner::<B>(
                        backend_settings,
                        format,
                        max_sessions,
                        inputs_per_bus,
                        &command_receiver,
                        &retirement_receiver,
                        &started_sender,
                        &control_weak,
                        &owner_status_thread,
                    );
                }));
                if outcome.is_err() {
                    if let Some(control) = control_weak.upgrade() {
                        let _ = control.seal();
                    }
                    owner_status_thread.clean.store(false, Ordering::Release);
                    owner_status_thread.retired.store(true, Ordering::Release);
                }
                forget_panic(outcome);
            })
            .map_err(MixerStartError::Thread)?;
        match started_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                control: Some(control),
                owner: Some(owner),
                owner_status,
                format,
                _backend: PhantomData,
                _thread_affinity: PhantomData,
            }),
            Ok(Err(error)) => {
                drop(control);
                let _ = owner.join();
                Err(error)
            }
            Err(_) => {
                drop(control);
                let _ = owner.join();
                Err(MixerStartError::OwnerStopped)
            }
        }
    }

    /// Verified format shared by every logical input.
    #[must_use]
    pub fn format(&self) -> MixerFormat {
        self.format
    }

    /// Add and fully preinstall the fixed Script/Native/Speech subtree.
    ///
    /// # Errors
    ///
    /// Returns a bounded control or topology error. No handle is published
    /// until every permanent slot sound has been accepted.
    pub fn add_session(&self, id: AudioSessionId) -> Result<MixerSessionHandle, MixerControlError> {
        let control = self
            .control
            .as_ref()
            .ok_or(MixerControlError::OwnerStopped)?;
        let key = send_request(control, |response| OwnerCommand::AddSession(id, response))?
            .map_err(MixerControlError::from)?;
        Ok(MixerSessionHandle {
            control: Arc::downgrade(control),
            key,
        })
    }

    /// Seal production, join the backend owner, and force-resolve live slots.
    #[must_use]
    pub fn shutdown(mut self) -> MixerShutdown {
        let mut requested = false;
        if let Some(control) = self.control.as_ref()
            && control.seal().is_ok()
        {
            requested = control.commands.send(OwnerCommand::Shutdown).is_ok();
        }
        self.control.take();
        let joined = self.owner.take().is_some_and(|owner| owner.join().is_ok());
        let clean = requested
            && joined
            && self.owner_status.retired.load(Ordering::Acquire)
            && self.owner_status.clean.load(Ordering::Acquire);
        MixerShutdown { clean }
    }
}

impl<B: Backend> Drop for MixerService<B> {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.seal();
            let _ = control.commands.try_send(OwnerCommand::Shutdown);
        }
        // Explicit shutdown is the proof-bearing joined path. Dropping the last
        // control Arc disconnects the owner if the bounded queue was full.
        self.owner.take();
    }
}

fn send_request<T>(
    control: &Arc<ControlInner>,
    command: impl FnOnce(SyncSender<T>) -> OwnerCommand,
) -> Result<T, MixerControlError> {
    let gate = control
        .gate
        .lock()
        .map_err(|_| MixerControlError::OwnerStopped)?;
    if gate.sealed {
        return Err(MixerControlError::OwnerStopped);
    }
    let (response_sender, response_receiver) = mpsc::sync_channel(1);
    match control.commands.try_send(command(response_sender)) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => return Err(MixerControlError::Saturated),
        Err(TrySendError::Disconnected(_)) => return Err(MixerControlError::OwnerStopped),
    }
    drop(gate);
    response_receiver
        .recv()
        .map_err(|_| MixerControlError::OwnerStopped)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_owner<B>(
    backend_settings: B::Settings,
    format: MixerFormat,
    max_sessions: usize,
    inputs_per_bus: usize,
    commands: &Receiver<OwnerCommand>,
    retirements: &Receiver<RetirementRequest>,
    started: &SyncSender<Result<(), MixerStartError<B::Error>>>,
    control: &Weak<ControlInner>,
    status: &OwnerStatus,
) where
    B: Backend,
{
    let cleanup_capacity = max_sessions
        .saturating_mul(SESSION_BUS_COUNT)
        .saturating_mul(inputs_per_bus)
        .max(1);
    let (cleanup_sender, cleanup_receiver) = mpsc::sync_channel(cleanup_capacity);
    let (cleanup_results, cleanup_result_receiver) = mpsc::sync_channel(cleanup_capacity);
    let cleanup_owner = match thread::Builder::new()
        .name("smudgy-audio-cleanup".into())
        .spawn(move || run_cleanup_worker(&cleanup_receiver, &cleanup_results))
    {
        Ok(owner) => owner,
        Err(error) => {
            let _ = started.send(Err(MixerStartError::Thread(error)));
            status.retired.store(true, Ordering::Release);
            return;
        }
    };
    let mut mixer =
        match MixerCore::<B>::with_limits(backend_settings, format, max_sessions, inputs_per_bus) {
            Ok(mixer) => mixer,
            Err(error) => {
                let _ = started.send(Err(error));
                drop(cleanup_sender);
                let _ = cleanup_owner.join();
                status.retired.store(true, Ordering::Release);
                return;
            }
        };
    let published = started.send(Ok(())).is_ok();
    let mut pending = Vec::new();
    if published {
        let active = catch_unwind(AssertUnwindSafe(|| {
            run_active_owner(
                &mut mixer,
                commands,
                retirements,
                &cleanup_sender,
                &cleanup_result_receiver,
                control,
                &mut pending,
            );
        }));
        if active.is_err() {
            mixer.cleanup_clean = false;
        }
        forget_panic(active);
    }
    let clean = terminal_owner_cleanup(
        &mut mixer,
        retirements,
        control,
        &mut pending,
        cleanup_sender,
        &cleanup_result_receiver,
        cleanup_owner,
    );
    status.clean.store(clean, Ordering::Release);
    status.retired.store(true, Ordering::Release);
}

#[allow(clippy::too_many_arguments)]
fn run_active_owner<B: Backend>(
    mixer: &mut MixerCore<B>,
    commands: &Receiver<OwnerCommand>,
    retirements: &Receiver<RetirementRequest>,
    cleanup_sender: &SyncSender<CleanupJob>,
    cleanup_results: &Receiver<CleanupResult>,
    control: &Weak<ControlInner>,
    pending: &mut Vec<RetirementRequest>,
) {
    loop {
        drain_cleanup_results(mixer, cleanup_results);
        drain_retirements(retirements, pending);
        scan_retirements(mixer, pending, cleanup_sender);
        let mut shutting_down = false;
        match commands.recv_timeout(RETIREMENT_SCAN_INTERVAL) {
            Ok(OwnerCommand::AddSession(id, response)) => {
                let result = mixer.add_session(id);
                let fatal = matches!(result, Err(MixerMutationError::InternalInvariant));
                mixer.cleanup_clean &= !fatal;
                if let Err(send_error) = response.send(result)
                    && send_error.0.is_ok()
                {
                    shutting_down = true;
                }
                shutting_down |= fatal;
            }
            Ok(OwnerCommand::Reserve(key, bus, response)) => {
                let result = mixer.reserve(key, bus);
                if matches!(
                    result,
                    Err(MixerMutationError::InternalInvariant
                        | MixerMutationError::GenerationExhausted)
                ) {
                    mixer.cleanup_clean = false;
                }
                if let Err(send_error) = response.send(result)
                    && let Ok(record) = send_error.0
                    && mixer.restore_reserved(record).is_err()
                {
                    mixer.cleanup_clean = false;
                    shutting_down = true;
                }
            }
            Ok(OwnerCommand::SetGain(key, bus, linear, response)) => {
                let _ = response.send(mixer.set_gain(key, bus, linear));
            }
            Ok(OwnerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                shutting_down = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if shutting_down || control.upgrade().is_none() {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn terminal_owner_cleanup<B: Backend>(
    mixer: &mut MixerCore<B>,
    retirements: &Receiver<RetirementRequest>,
    control: &Weak<ControlInner>,
    pending: &mut Vec<RetirementRequest>,
    cleanup_sender: SyncSender<CleanupJob>,
    cleanup_results: &Receiver<CleanupResult>,
    cleanup_owner: JoinHandle<()>,
) -> bool {
    if control
        .upgrade()
        .is_some_and(|control| control.seal().is_err())
    {
        mixer.cleanup_clean = false;
    }
    drain_retirements(retirements, pending);
    mixer.close_all_slots();
    let backend_clean = retire_backend(mixer);
    if backend_clean {
        scan_retirements(mixer, pending, &cleanup_sender);
        for request in pending.drain(..) {
            let result = Err(MixerRetirementError::Structural);
            mixer.cleanup_clean = false;
            request.record.slot.force_quarantine();
            send_completion(request.completion, result);
        }
        let mut forced_jobs = mixer.prepare_forced_jobs().into_iter();
        while let Some(job) = forced_jobs.next() {
            if let Err(error) = cleanup_sender.send(job) {
                mixer.cleanup_clean = false;
                retain_failed_cleanup_job(error.0);
                for job in forced_jobs {
                    retain_failed_cleanup_job(job);
                }
                break;
            }
        }
    } else {
        mixer.cleanup_clean = false;
        for request in pending.drain(..) {
            request.record.slot.force_quarantine();
            send_completion(
                request.completion,
                Err(MixerRetirementError::OwnerUncertain),
            );
        }
        mixer.force_quarantine_all();
    }
    drop(cleanup_sender);
    while let Ok(result) = cleanup_results.recv() {
        finish_cleanup_result(mixer, result);
    }
    let cleanup_joined = cleanup_owner.join().is_ok();
    backend_clean && cleanup_joined && mixer.cleanup_clean
}

fn drain_retirements(
    retirements: &Receiver<RetirementRequest>,
    pending: &mut Vec<RetirementRequest>,
) {
    pending.extend(retirements.try_iter());
}

fn scan_retirements<B: Backend>(
    mixer: &mut MixerCore<B>,
    pending: &mut Vec<RetirementRequest>,
    cleanup_sender: &SyncSender<CleanupJob>,
) {
    let mut index = 0;
    while index < pending.len() {
        let request = &pending[index];
        if !request
            .record
            .slot
            .is_retire_ready(request.record.generation)
        {
            index += 1;
            continue;
        }
        let request = pending.swap_remove(index);
        let prepared = match request
            .record
            .slot
            .prepare_retirement(request.record.generation)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                mixer.cleanup_clean = false;
                send_completion(request.completion, Err(error));
                continue;
            }
        };
        let job = CleanupJob {
            record: request.record,
            prepared,
            completion: Some(request.completion),
            terminal: false,
        };
        if let Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) =
            cleanup_sender.try_send(job)
        {
            mixer.cleanup_clean = false;
            retain_failed_cleanup_job(job);
        }
    }
}

fn run_cleanup_worker(jobs: &Receiver<CleanupJob>, results: &SyncSender<CleanupResult>) {
    while let Ok(mut job) = jobs.recv() {
        if let Some(source) = job.prepared.source.take()
            && forget_panic(catch_unwind(AssertUnwindSafe(|| drop(source)))).is_none()
        {
            job.prepared.result = Err(MixerRetirementError::SourceDestructorPanicked);
        }
        if results
            .send(CleanupResult {
                record: job.record,
                result: job.prepared.result,
                completion: job.completion,
                terminal: job.terminal,
            })
            .is_err()
        {
            break;
        }
    }
}

fn retain_failed_cleanup_job(mut job: CleanupJob) {
    if let Some(source) = job.prepared.source.take() {
        std::mem::forget(source);
    }
    job.record.slot.force_quarantine();
    if let Some(completion) = job.completion.take() {
        send_completion(completion, Err(MixerRetirementError::OwnerUncertain));
    }
    std::mem::forget(job);
}

fn drain_cleanup_results<B: Backend>(mixer: &mut MixerCore<B>, results: &Receiver<CleanupResult>) {
    for result in results.try_iter() {
        finish_cleanup_result(mixer, result);
    }
}

fn finish_cleanup_result<B: Backend>(mixer: &mut MixerCore<B>, result: CleanupResult) {
    let CleanupResult {
        record,
        result,
        completion,
        terminal,
    } = result;
    let generation = record.generation;
    let result = if terminal {
        record.slot.finish_terminal(generation, result)
    } else {
        match record.slot.finish_reusable(generation, result) {
            Ok((retirement, next_generation)) => {
                mixer.recycle(record, next_generation).map(|()| retirement)
            }
            Err(error) => Err(error),
        }
    };
    mixer.cleanup_clean &= result.is_ok();
    if let Some(completion) = completion {
        send_completion(completion, result);
    }
}

fn send_completion(
    completion: oneshot::Sender<Result<MixerInputRetirement, MixerRetirementError>>,
    result: Result<MixerInputRetirement, MixerRetirementError>,
) {
    forget_panic(catch_unwind(AssertUnwindSafe(|| {
        let _ = completion.send(result);
    })));
}

fn retire_backend<B: Backend>(mixer: &mut MixerCore<B>) -> bool {
    let Some(manager) = mixer.manager.take() else {
        return false;
    };
    forget_panic(catch_unwind(AssertUnwindSafe(|| drop(manager)))).is_some()
}

fn forget_panic<T>(result: std::thread::Result<T>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(payload) => {
            std::mem::forget(payload);
            None
        }
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
