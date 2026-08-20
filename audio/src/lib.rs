//! Process-level audio mixing primitives for Smudgy.
//!
//! This crate deliberately has no Deno, V8, UI, package, or session-runtime
//! dependency. It owns the bounded physical-mixer topology that those layers
//! will use through explicit adapters.

use std::{
    collections::HashMap,
    convert::Infallible,
    fmt,
    marker::PhantomData,
    mem::ManuallyDrop,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

use kira::{
    AudioManager, AudioManagerSettings, Capacities, Frame, PlaySoundError,
    backend::Backend,
    sound::{Sound, SoundData},
    track::{MainTrackBuilder, TrackBuilder, TrackHandle},
};

/// The fixed number of frames in one internal mixer quantum.
pub const INTERNAL_BUFFER_FRAMES: usize = 128;

/// The maximum number of simultaneous sessions in the first mixer profile.
pub const MAX_SESSIONS: usize = 32;

/// The maximum number of simultaneous inputs on each session bus.
pub const INPUTS_PER_BUS: usize = 32;

/// The bounded number of pending control operations.
pub const CONTROL_QUEUE_CAPACITY: usize = 128;

const SESSION_BUS_COUNT: usize = 3;

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

/// Whether an input has more audio to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerInputStatus {
    /// Keep the input installed for the next quantum.
    Active,
    /// Remove the input after this quantum.
    Finished,
}

/// A real-time audio producer installed into one session bus.
///
/// Implementations must overwrite the complete output slice and must not
/// allocate, block, or unwind. The bridge still contains an unexpected unwind
/// and fails closed so it cannot cross the backend callback boundary.
pub trait MixerInput: Send + 'static {
    /// Render the next stereo quantum.
    fn render(&mut self, output: &mut [Frame]) -> MixerInputStatus;
}

const INPUT_ACTIVE: usize = 1 << 0;
const INPUT_STARTED: usize = 1 << 1;
const INPUT_CLOSED: usize = 1 << 2;
const INPUT_FAILED: usize = 1 << 3;
const INPUT_SUSPENDED: usize = 1 << 4;

struct InputBridge {
    state: AtomicUsize,
    source: std::cell::UnsafeCell<ManuallyDrop<Box<dyn MixerInput>>>,
}

// The ACTIVE bit grants exclusive access to `source`. Every renderer receives
// only Weak access, and a second concurrent entry fails the bridge closed.
unsafe impl Sync for InputBridge {}

impl InputBridge {
    fn new(source: Box<dyn MixerInput>) -> Self {
        Self {
            state: AtomicUsize::new(0),
            source: std::cell::UnsafeCell::new(ManuallyDrop::new(source)),
        }
    }

    fn close(&self) {
        self.state.fetch_or(INPUT_CLOSED, Ordering::AcqRel);
    }

    fn try_enter(&self) -> InputEntry {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if state & INPUT_CLOSED != 0 {
                return InputEntry::Closed;
            }
            if state & INPUT_STARTED == 0 {
                return InputEntry::Prestart;
            }
            if state & INPUT_ACTIVE != 0 {
                self.state
                    .fetch_or(INPUT_CLOSED | INPUT_FAILED, Ordering::AcqRel);
                return InputEntry::Closed;
            }
            if state & INPUT_SUSPENDED != 0 {
                return InputEntry::Suspended;
            }
            if self
                .state
                .compare_exchange_weak(
                    state,
                    state | INPUT_ACTIVE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return InputEntry::Entered;
            }
        }
    }
}

enum InputEntry {
    Prestart,
    Suspended,
    Entered,
    Closed,
}

struct InputRenderGuard<'a>(&'a InputBridge);

impl Drop for InputRenderGuard<'_> {
    fn drop(&mut self) {
        self.0.state.fetch_and(!INPUT_ACTIVE, Ordering::Release);
    }
}

/// The sole strong owner of a mixer input callback.
///
/// Kira's render object receives only a [`Weak`] reference. Call [`Self::close`]
/// and then [`Self::try_retire`] off the render thread to destroy the callback.
/// Dropping an unretired owner intentionally leaks it after closing the gate;
/// this is preferable to destroying a host callback on an unknown thread.
pub struct MixerInputOwner {
    bridge: ManuallyDrop<Arc<InputBridge>>,
}

impl fmt::Debug for MixerInputOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MixerInputOwner")
            .field("state", &self.bridge.state.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl MixerInputOwner {
    /// Create a prestart-silent input and its single-use Kira payload.
    #[must_use]
    pub fn new(source: impl MixerInput) -> (Self, MixerInputSound) {
        let bridge = Arc::new(InputBridge::new(Box::new(source)));
        let sound = MixerInputSound {
            bridge: Arc::downgrade(&bridge),
        };
        (
            Self {
                bridge: ManuallyDrop::new(bridge),
            },
            sound,
        )
    }

    /// Open the prestart gate after the input has been accepted by its host.
    ///
    /// Returns `false` if the input was already started or closed.
    #[must_use]
    pub fn start(&self) -> bool {
        self.bridge
            .state
            .compare_exchange(0, INPUT_STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Absorbingly silence the input. This operation is idempotent.
    pub fn close(&self) {
        self.bridge.close();
    }

    /// Silence this logical input from the next render entry onward.
    ///
    /// This never pauses the shared physical mixer or any sibling input.
    /// Returns `false` when the input has not started or has already closed.
    #[must_use]
    pub fn suspend(&self) -> bool {
        self.set_suspended(true)
    }

    /// Reopen a suspended logical input from the next render entry onward.
    ///
    /// Returns `false` when the input has not started or has already closed.
    #[must_use]
    pub fn resume(&self) -> bool {
        self.set_suspended(false)
    }

    fn set_suspended(&self, suspended: bool) -> bool {
        loop {
            let state = self.bridge.state.load(Ordering::Acquire);
            if state & INPUT_STARTED == 0 || state & INPUT_CLOSED != 0 {
                return false;
            }
            let next = if suspended {
                state | INPUT_SUSPENDED
            } else {
                state & !INPUT_SUSPENDED
            };
            if next == state {
                return true;
            }
            if self
                .bridge
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Returns whether this logical input is currently suspended.
    #[must_use]
    pub fn is_suspended(&self) -> bool {
        self.bridge.state.load(Ordering::Acquire) & INPUT_SUSPENDED != 0
    }

    /// Returns whether rendering or the source callback failed closed.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.bridge.state.load(Ordering::Acquire) & INPUT_FAILED != 0
    }

    /// Destroy the callback off the render thread once no render entry remains.
    ///
    /// # Errors
    ///
    /// Returns the unchanged owner if a render callback still temporarily owns
    /// the bridge. The caller may retry after that callback exits.
    pub fn try_retire(mut self) -> Result<MixerInputRetirement, Self> {
        self.close();
        // SAFETY: this is the only place that takes the owner's strong Arc. The
        // ManuallyDrop field prevents the fallback Drop path from releasing it.
        let bridge = unsafe { ManuallyDrop::take(&mut self.bridge) };
        match Arc::try_unwrap(bridge) {
            Ok(bridge) => {
                let failed_before_retirement =
                    bridge.state.load(Ordering::Acquire) & INPUT_FAILED != 0;
                let source_destructor_panicked = catch_unwind(AssertUnwindSafe(|| {
                    // SAFETY: Arc uniqueness proves there is no active renderer,
                    // and this is the single deliberate destruction of `source`.
                    unsafe { ManuallyDrop::drop(&mut *bridge.source.get()) };
                }))
                .map_or_else(
                    |payload| {
                        std::mem::forget(payload);
                        true
                    },
                    |()| false,
                );
                let retirement = MixerInputRetirement {
                    failed_before_retirement,
                    source_destructor_panicked,
                };
                // `bridge` was moved out of `self`; do not run the fallback
                // Drop implementation against the now-uninitialized field.
                std::mem::forget(self);
                Ok(retirement)
            }
            Err(bridge) => {
                self.bridge = ManuallyDrop::new(bridge);
                Err(self)
            }
        }
    }
}

impl Drop for MixerInputOwner {
    fn drop(&mut self) {
        self.bridge.close();
        // `bridge` is ManuallyDrop: an unproven callback is quarantined rather
        // than destroyed on this arbitrary caller thread.
    }
}

/// Result of an off-render mixer-input retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerInputRetirement {
    /// Whether rendering had already failed closed.
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

/// Single-use Kira sound data backed by weak input access.
pub struct MixerInputSound {
    bridge: Weak<InputBridge>,
}

impl fmt::Debug for MixerInputSound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MixerInputSound")
            .finish_non_exhaustive()
    }
}

struct WeakMixerSound {
    bridge: Weak<InputBridge>,
    finished: bool,
}

impl SoundData for MixerInputSound {
    type Error = Infallible;
    type Handle = ();

    fn into_sound(self) -> Result<(Box<dyn Sound>, Self::Handle), Self::Error> {
        Ok((
            Box::new(WeakMixerSound {
                bridge: self.bridge,
                finished: false,
            }),
            (),
        ))
    }
}

impl Sound for WeakMixerSound {
    fn process(&mut self, output: &mut [Frame], _dt: f64, _info: &kira::info::Info) {
        output.fill(Frame::ZERO);
        if self.finished {
            return;
        }
        let Some(bridge) = self.bridge.upgrade() else {
            self.finished = true;
            return;
        };
        match bridge.try_enter() {
            InputEntry::Prestart | InputEntry::Suspended => {}
            InputEntry::Closed => self.finished = true,
            InputEntry::Entered => {
                let _guard = InputRenderGuard(&bridge);
                let result = catch_unwind(AssertUnwindSafe(|| {
                    // SAFETY: the ACTIVE bit grants this callback exclusive
                    // access to the non-Sync source for the guard's lifetime.
                    let source = unsafe { &mut **bridge.source.get() };
                    source.render(output)
                }));
                match result {
                    Ok(MixerInputStatus::Active) => {}
                    Ok(MixerInputStatus::Finished) => {
                        bridge.state.fetch_or(INPUT_CLOSED, Ordering::AcqRel);
                        self.finished = true;
                    }
                    Err(payload) => {
                        output.fill(Frame::ZERO);
                        bridge
                            .state
                            .fetch_or(INPUT_CLOSED | INPUT_FAILED, Ordering::AcqRel);
                        self.finished = true;
                        std::mem::forget(payload);
                    }
                }
            }
        }
    }

    fn finished(&self) -> bool {
        self.finished
            || self
                .bridge
                .upgrade()
                .is_none_or(|bridge| bridge.state.load(Ordering::Acquire) & INPUT_CLOSED != 0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MixerMutationError {
    DuplicateSession,
    SessionCapacity,
    UnknownSession,
    InputCapacity,
    InternalInvariant,
}

struct SessionTracks {
    _root: TrackHandle,
    script: TrackHandle,
    native: TrackHandle,
    speech: TrackHandle,
}

impl SessionTracks {
    fn bus_mut(&mut self, bus: SessionBus) -> &mut TrackHandle {
        match bus {
            SessionBus::Script => &mut self.script,
            SessionBus::Native => &mut self.native,
            SessionBus::Speech => &mut self.speech,
        }
    }
}

struct MixerCore<B: Backend> {
    manager: AudioManager<B>,
    sessions: HashMap<AudioSessionId, SessionTracks>,
    max_sessions: usize,
    inputs_per_bus: usize,
}

impl<B: Backend> MixerCore<B> {
    fn new(backend_settings: B::Settings) -> Result<Self, B::Error> {
        Self::with_limits(backend_settings, MAX_SESSIONS, INPUTS_PER_BUS)
    }

    fn with_limits(
        backend_settings: B::Settings,
        max_sessions: usize,
        inputs_per_bus: usize,
    ) -> Result<Self, B::Error> {
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
            backend_settings,
        })?;
        Ok(Self {
            manager,
            sessions: HashMap::with_capacity(max_sessions),
            max_sessions,
            inputs_per_bus,
        })
    }

    fn add_session(&mut self, id: AudioSessionId) -> Result<(), MixerMutationError> {
        if self.sessions.contains_key(&id) {
            return Err(MixerMutationError::DuplicateSession);
        }
        if self.sessions.len() == self.max_sessions {
            return Err(MixerMutationError::SessionCapacity);
        }
        let mut root = self
            .manager
            .add_sub_track(
                TrackBuilder::new()
                    .sound_capacity(0)
                    .sub_track_capacity(SESSION_BUS_COUNT),
            )
            .map_err(|_| MixerMutationError::InternalInvariant)?;
        let bus = || {
            TrackBuilder::new()
                .sound_capacity(self.inputs_per_bus)
                .sub_track_capacity(0)
        };
        let script = root
            .add_sub_track(bus())
            .map_err(|_| MixerMutationError::InternalInvariant)?;
        let native = root
            .add_sub_track(bus())
            .map_err(|_| MixerMutationError::InternalInvariant)?;
        let speech = root
            .add_sub_track(bus())
            .map_err(|_| MixerMutationError::InternalInvariant)?;
        let old = self.sessions.insert(
            id,
            SessionTracks {
                _root: root,
                script,
                native,
                speech,
            },
        );
        debug_assert!(old.is_none());
        Ok(())
    }

    fn remove_session(&mut self, id: AudioSessionId) -> Result<(), MixerMutationError> {
        self.sessions
            .remove(&id)
            .map(|_| ())
            .ok_or(MixerMutationError::UnknownSession)
    }

    fn play(
        &mut self,
        id: AudioSessionId,
        bus: SessionBus,
        sound: MixerInputSound,
    ) -> Result<(), MixerMutationError> {
        let track = self
            .sessions
            .get_mut(&id)
            .ok_or(MixerMutationError::UnknownSession)?
            .bus_mut(bus);
        match track.play(sound) {
            Ok(()) => Ok(()),
            Err(PlaySoundError::SoundLimitReached) => Err(MixerMutationError::InputCapacity),
            Err(PlaySoundError::IntoSoundError(never)) => match never {},
        }
    }
}

enum OwnerCommand {
    AddSession(AudioSessionId, SyncSender<Result<(), MixerMutationError>>),
    RemoveSession(AudioSessionId, SyncSender<Result<(), MixerMutationError>>),
    Play(
        AudioSessionId,
        SessionBus,
        MixerInputSound,
        SyncSender<Result<(), MixerMutationError>>,
    ),
    Shutdown,
}

/// Errors reported by the bounded mixer control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerControlError {
    /// The fixed command queue is currently full.
    Saturated,
    /// The mixer owner has already stopped.
    OwnerStopped,
    /// The session identity is already present.
    DuplicateSession,
    /// The fixed session capacity has been reached.
    SessionCapacity,
    /// The requested session does not exist.
    UnknownSession,
    /// The selected bus has reached its fixed input capacity.
    InputCapacity,
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
            MixerMutationError::InternalInvariant => Self::InternalInvariant,
        }
    }
}

/// Failure to construct the dedicated mixer owner.
#[derive(Debug)]
pub enum MixerStartError<E> {
    /// The owner thread could not be created.
    Thread(std::io::Error),
    /// The selected Kira backend could not be created.
    Backend(E),
    /// The owner terminated before reporting its startup result.
    OwnerStopped,
}

/// Result of explicitly joining the mixer owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerShutdown {
    /// Whether the owner returned normally after dropping the manager/backend.
    pub clean: bool,
}

/// A bounded control handle for the process mixer and its dedicated owner.
pub struct MixerService<B: Backend> {
    commands: Option<SyncSender<OwnerCommand>>,
    owner: Option<JoinHandle<()>>,
    _backend: PhantomData<fn() -> B>,
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
    /// Create the backend and mixer topology on a dedicated owner thread.
    ///
    /// # Errors
    ///
    /// Returns a typed spawn/backend/owner failure. A backend failure is joined
    /// before this function returns, so no endpoint owner escapes an error.
    pub fn start(backend_settings: B::Settings) -> Result<Self, MixerStartError<B::Error>> {
        let (commands, receiver) = mpsc::sync_channel(CONTROL_QUEUE_CAPACITY);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let owner = thread::Builder::new()
            .name("smudgy-audio-owner".into())
            .spawn(move || run_owner::<B>(backend_settings, &receiver, &started_sender))
            .map_err(MixerStartError::Thread)?;
        match started_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                commands: Some(commands),
                owner: Some(owner),
                _backend: PhantomData,
            }),
            Ok(Err(error)) => {
                let _ = owner.join();
                Err(MixerStartError::Backend(error))
            }
            Err(_) => {
                let _ = owner.join();
                Err(MixerStartError::OwnerStopped)
            }
        }
    }

    /// Add the fixed Script/Native/Speech subtree for a session.
    ///
    /// # Errors
    ///
    /// Returns a bounded control or topology error.
    pub fn add_session(&self, id: AudioSessionId) -> Result<(), MixerControlError> {
        self.request(|response| OwnerCommand::AddSession(id, response))
    }

    /// Remove a session subtree.
    ///
    /// # Errors
    ///
    /// Returns a bounded control error or [`MixerControlError::UnknownSession`].
    pub fn remove_session(&self, id: AudioSessionId) -> Result<(), MixerControlError> {
        self.request(|response| OwnerCommand::RemoveSession(id, response))
    }

    /// Install one weak input on a session bus.
    ///
    /// The matching [`MixerInputOwner`] remains outside the mixer and must be
    /// started only after this operation succeeds.
    ///
    /// # Errors
    ///
    /// Returns a bounded control, identity, or input-capacity error.
    pub fn play(
        &self,
        id: AudioSessionId,
        bus: SessionBus,
        sound: MixerInputSound,
    ) -> Result<(), MixerControlError> {
        self.request(|response| OwnerCommand::Play(id, bus, sound, response))
    }

    fn request(
        &self,
        command: impl FnOnce(SyncSender<Result<(), MixerMutationError>>) -> OwnerCommand,
    ) -> Result<(), MixerControlError> {
        let commands = self
            .commands
            .as_ref()
            .ok_or(MixerControlError::OwnerStopped)?;
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        match commands.try_send(command(response_sender)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(MixerControlError::Saturated),
            Err(TrySendError::Disconnected(_)) => return Err(MixerControlError::OwnerStopped),
        }
        response_receiver
            .recv()
            .map_err(|_| MixerControlError::OwnerStopped)?
            .map_err(Into::into)
    }

    /// Close the FIFO, join the owner, and retire the backend on that owner.
    #[must_use]
    pub fn shutdown(mut self) -> MixerShutdown {
        let clean = if let (Some(commands), Some(owner)) = (self.commands.take(), self.owner.take())
        {
            let sent = commands.send(OwnerCommand::Shutdown).is_ok();
            drop(commands);
            let joined = owner.join().is_ok();
            sent && joined
        } else {
            false
        };
        MixerShutdown { clean }
    }
}

impl<B: Backend> Drop for MixerService<B> {
    fn drop(&mut self) {
        // Disconnecting the last producer makes the owner drain accepted work
        // and retire. Explicit `shutdown` is the proof-bearing joined path.
        self.commands.take();
        self.owner.take();
    }
}

fn run_owner<B>(
    backend_settings: B::Settings,
    commands: &Receiver<OwnerCommand>,
    started: &SyncSender<Result<(), B::Error>>,
) where
    B: Backend,
{
    let mut mixer = match MixerCore::<B>::new(backend_settings) {
        Ok(mixer) => mixer,
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };
    if started.send(Ok(())).is_err() {
        return;
    }
    while let Ok(command) = commands.recv() {
        match command {
            OwnerCommand::AddSession(id, response) => {
                let _ = response.send(mixer.add_session(id));
            }
            OwnerCommand::RemoveSession(id, response) => {
                let _ = response.send(mixer.remove_session(id));
            }
            OwnerCommand::Play(id, bus, sound, response) => {
                let _ = response.send(mixer.play(id, bus, sound));
            }
            OwnerCommand::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests;
