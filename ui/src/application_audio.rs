//! Application-owned physical Web Audio lifecycle coordination.
//!
//! The unique coordinator lives outside iced. The daemon receives only a
//! bounded command capability, so neither core nor a session can own the
//! process service, application gate, registration map, or retirement proof.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use futures::executor::block_on;
use smudgy_audio::{
    AudioSessionId, MixerControlError, MixerSessionRegistrar, MixerSessionRetirementError,
    MixerShutdown, SystemMixerService, SystemMixerUnavailable,
};
use smudgy_audio_web::{
    ApplicationAudioOwner, ApplicationAudioRegistrar, AudioHostLimits, SessionAudioRegistration,
    SessionAudioRegistrationError, SessionAudioScope, UnavailableAudioOutputCause,
    UnavailableSessionAudioRegistration, UnavailableSessionAudioRetirement,
};
use smudgy_core::session::runtime::{
    RuntimeAction, RuntimeThreadJoinOutcome, RuntimeThreadPublicationFailure,
};
use smudgy_core::session::{AudioSessionSpawnError, SessionId, registry};

/// Smudgy's one fixed logical and physical output rate.
pub const PHYSICAL_SAMPLE_RATE: u32 = 48_000;

const COORDINATOR_QUEUE_CAPACITY: usize = 128;

type RuntimeShutdown = Arc<dyn Fn(SessionId) -> bool + Send + Sync>;
type RuntimeJoin = Arc<dyn Fn(SessionId) -> RuntimeThreadJoinOutcome + Send + Sync>;
type RuntimeJoinAll = Arc<dyn Fn() -> Vec<RuntimeThreadJoinOutcome> + Send + Sync>;
type IoQuiesce = Arc<dyn Fn() + Send + Sync>;

fn request_runtime_shutdown(session_id: SessionId) -> bool {
    registry::get_runtime(session_id)
        .is_some_and(|runtime| runtime.tx().send(RuntimeAction::Shutdown).is_ok())
}

fn join_runtime(session_id: SessionId) -> RuntimeThreadJoinOutcome {
    smudgy_core::session::runtime::join_runtime_thread(session_id)
}

/// Stable startup failure for the outer application coordinator.
#[derive(Debug)]
pub enum ApplicationAudioStartError {
    Worker {
        source: std::io::Error,
        cleanup: ApplicationAudioStartCleanup,
    },
}

#[derive(Debug)]
pub enum ApplicationAudioStartCleanup {
    Physical(MixerShutdown),
    Unavailable(SystemMixerUnavailable),
}

impl fmt::Display for ApplicationAudioStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Worker { source, cleanup } => write!(
                formatter,
                "Web Audio lifecycle worker could not start: {source}; startup cleanup evidence: {cleanup:?}"
            ),
        }
    }
}

impl std::error::Error for ApplicationAudioStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Worker { source, .. } => Some(source),
        }
    }
}

impl ApplicationAudioStartError {
    #[must_use]
    pub fn cleanup_is_clean(&self) -> bool {
        match self {
            Self::Worker {
                cleanup: ApplicationAudioStartCleanup::Physical(shutdown),
                ..
            } => shutdown.clean && shutdown.failure.is_none(),
            Self::Worker {
                cleanup: ApplicationAudioStartCleanup::Unavailable(cause),
                ..
            } => cause.cleanup_proven(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationAudioOpenError {
    ApplicationSealed,
    DuplicateSession,
    SessionCapacity,
    CoordinatorStopped,
    Mixer(MixerControlError),
    Registration(SessionAudioRegistrationError),
    RegistrationRollback {
        registration: SessionAudioRegistrationError,
        retirement: MixerSessionRetirementError,
    },
}

impl fmt::Display for ApplicationAudioOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApplicationSealed => formatter.write_str("application audio is shutting down"),
            Self::DuplicateSession => formatter.write_str("the session already owns audio"),
            Self::SessionCapacity => {
                formatter.write_str("the application audio session capacity is full")
            }
            Self::CoordinatorStopped => {
                formatter.write_str("the application audio coordinator stopped")
            }
            Self::Mixer(error) => write!(
                formatter,
                "the shared mixer rejected the session: {error:?}"
            ),
            Self::Registration(error) => {
                write!(
                    formatter,
                    "the Web Audio host rejected the session: {error:?}"
                )
            }
            Self::RegistrationRollback {
                registration,
                retirement,
            } => write!(
                formatter,
                "the Web Audio host rejected the session ({registration:?}) and mixer rollback failed ({retirement:?})"
            ),
        }
    }
}

impl std::error::Error for ApplicationAudioOpenError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAudioCloseDisposition {
    Requested,
    AlreadyClosing,
    UnknownSession,
    InvalidRuntimeProof,
    CoordinatorStopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeShutdownRequest {
    /// A shutdown action was queued to the live runtime.
    Requested,
    /// No live sender remained, or exact pre-start cleanup already joined it.
    AlreadyClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAudioCloseResult {
    pub session_id: SessionId,
    pub shutdown: RuntimeShutdownRequest,
    pub runtime: RuntimeThreadJoinOutcome,
    pub publication_failure: Option<RuntimeThreadPublicationFailure>,
    pub retirement: SessionAudioRetirementResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAudioRetirementResult {
    Physical(Result<(), MixerSessionRetirementError>),
    MixerFree(UnavailableSessionAudioRetirement),
}

impl SessionAudioRetirementResult {
    #[must_use]
    pub fn is_clean_for(self, session_id: SessionId) -> bool {
        match self {
            Self::Physical(result) => result.is_ok(),
            Self::MixerFree(receipt) => receipt.session_id() == u64::from(u32::from(session_id)),
        }
    }
}

impl SessionAudioCloseResult {
    #[must_use]
    pub fn is_clean(self) -> bool {
        // The shutdown disposition is diagnostic: a runtime may already have
        // stopped and left the live registry. The still-owned exact join is
        // the authoritative lifecycle proof.
        self.publication_failure.is_none()
            && self.runtime
                == RuntimeThreadJoinOutcome::Clean {
                    session_id: self.session_id,
                }
            && self.retirement.is_clean_for(self.session_id)
    }
}

#[derive(Debug)]
pub struct ApplicationAudioShutdownReport {
    pub sessions: Vec<SessionAudioCloseResult>,
    pub unowned_runtime_joins: Vec<RuntimeThreadJoinOutcome>,
    pub io_quiesce_attempted: bool,
    pub io_quiesce_clean: bool,
    pub coordinator_joined: bool,
    pub lifecycle_worker_joined: bool,
    pub lifecycle_transport_clean: bool,
    pub output: ApplicationAudioOutputShutdown,
}

#[derive(Debug)]
pub enum ApplicationAudioOutputShutdown {
    Physical(MixerShutdown),
    Unavailable(SystemMixerUnavailable),
}

impl ApplicationAudioOutputShutdown {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        match self {
            Self::Physical(shutdown) => shutdown.clean && shutdown.failure.is_none(),
            Self::Unavailable(cause) => cause.cleanup_proven(),
        }
    }
}

impl ApplicationAudioShutdownReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.coordinator_joined
            && self.lifecycle_worker_joined
            && self.lifecycle_transport_clean
            && self.io_quiesce_attempted
            && self.io_quiesce_clean
            && self.sessions.iter().all(|result| result.is_clean())
            && self.unowned_runtime_joins.is_empty()
            && self.output.is_clean()
    }
}

impl fmt::Display for ApplicationAudioShutdownReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "session results={:?}; unowned runtime joins={:?}; I/O quiesce attempted={}; I/O quiesce clean={}; coordinator joined={}; lifecycle worker joined={}; lifecycle transport clean={}; output shutdown={:?}",
            self.sessions,
            self.unowned_runtime_joins,
            self.io_quiesce_attempted,
            self.io_quiesce_clean,
            self.coordinator_joined,
            self.lifecycle_worker_joined,
            self.lifecycle_transport_clean,
            self.output
        )
    }
}

struct LifecycleJob {
    session_id: SessionId,
    shutdown: RuntimeShutdownRequest,
    runtime: Option<RuntimeThreadJoinOutcome>,
    publication_failure: Option<RuntimeThreadPublicationFailure>,
    registration: CoordinatedSessionAudio,
}

enum CoordinatedSessionAudio {
    Physical(SessionAudioRegistration),
    Unavailable(UnavailableSessionAudioRegistration),
}

impl CoordinatedSessionAudio {
    fn scope(&self) -> SessionAudioScope {
        match self {
            Self::Physical(registration) => registration.scope(),
            Self::Unavailable(registration) => registration.scope(),
        }
    }

    fn seal(&mut self) -> bool {
        match self {
            Self::Physical(registration) => registration.seal(),
            Self::Unavailable(registration) => registration.seal(),
        }
    }

    fn retire(self) -> SessionAudioRetirementResult {
        match self {
            Self::Physical(registration) => {
                SessionAudioRetirementResult::Physical(block_on(registration.retire()))
            }
            Self::Unavailable(registration) => {
                SessionAudioRetirementResult::MixerFree(registration.retire())
            }
        }
    }
}

enum LifecycleCommand {
    Close(LifecycleJob),
    Shutdown,
}

fn finish_job(job: LifecycleJob, runtime_join: &RuntimeJoin) -> SessionAudioCloseResult {
    // Load-bearing order: the exact runtime cannot touch its scope after this
    // join before mixer retirement starts.
    let runtime = job.runtime.unwrap_or_else(|| runtime_join(job.session_id));
    let retirement = job.registration.retire();
    SessionAudioCloseResult {
        session_id: job.session_id,
        shutdown: job.shutdown,
        runtime,
        publication_failure: job.publication_failure,
        retirement,
    }
}

struct CoordinatorState {
    open: bool,
    registrations: BTreeMap<SessionId, CoordinatedSessionAudio>,
    closing: BTreeSet<SessionId>,
    submitted: BTreeSet<SessionId>,
    completed: Vec<SessionAudioCloseResult>,
    completions: mpsc::Receiver<SessionAudioCloseResult>,
    io_quiesce_attempted: bool,
    io_quiesce_clean: bool,
    lifecycle_transport_clean: bool,
}

enum CoordinatorCommand {
    Open {
        session_id: SessionId,
        reply: mpsc::SyncSender<Result<SessionAudioScope, ApplicationAudioOpenError>>,
    },
    Close {
        session_id: SessionId,
        reply: mpsc::SyncSender<SessionAudioCloseDisposition>,
    },
    ClosePrejoined {
        session_id: SessionId,
        failure: RuntimeThreadPublicationFailure,
        cleanup: RuntimeThreadJoinOutcome,
        reply: mpsc::SyncSender<SessionAudioCloseDisposition>,
    },
    Finish {
        reply: mpsc::SyncSender<CoordinatorFinish>,
    },
}

struct CoordinatorFinish {
    sessions: Vec<SessionAudioCloseResult>,
    io_quiesce_attempted: bool,
    io_quiesce_clean: bool,
    lifecycle_transport_clean: bool,
}

/// Cloneable, UI-thread-affine bounded capability passed into iced.
#[derive(Clone)]
pub struct ApplicationAudioController {
    commands: mpsc::SyncSender<CoordinatorCommand>,
    _ui_thread: PhantomData<Rc<()>>,
}

impl fmt::Debug for ApplicationAudioController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationAudioController")
            .finish_non_exhaustive()
    }
}

impl ApplicationAudioController {
    pub fn begin_session(
        &self,
        session_id: SessionId,
    ) -> Result<PendingSessionAudio, ApplicationAudioOpenError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .send(CoordinatorCommand::Open { session_id, reply })
            .map_err(|_| ApplicationAudioOpenError::CoordinatorStopped)?;
        let scope = response
            .recv()
            .unwrap_or(Err(ApplicationAudioOpenError::CoordinatorStopped))?;
        Ok(PendingSessionAudio {
            controller: self.clone(),
            session_id,
            scope,
            committed: false,
        })
    }

    pub fn close_session(&self, session_id: SessionId) -> SessionAudioCloseDisposition {
        let (reply, response) = mpsc::sync_channel(1);
        if self
            .commands
            .send(CoordinatorCommand::Close { session_id, reply })
            .is_err()
        {
            return SessionAudioCloseDisposition::CoordinatorStopped;
        }
        response
            .recv()
            .unwrap_or(SessionAudioCloseDisposition::CoordinatorStopped)
    }

    fn close_prejoined_runtime(
        &self,
        session_id: SessionId,
        failure: RuntimeThreadPublicationFailure,
        cleanup: RuntimeThreadJoinOutcome,
    ) -> SessionAudioCloseDisposition {
        let (reply, response) = mpsc::sync_channel(1);
        if self
            .commands
            .send(CoordinatorCommand::ClosePrejoined {
                session_id,
                failure,
                cleanup,
                reply,
            })
            .is_err()
        {
            return SessionAudioCloseDisposition::CoordinatorStopped;
        }
        response
            .recv()
            .unwrap_or(SessionAudioCloseDisposition::CoordinatorStopped)
    }
}

/// Rollback guard spanning audio registration through exact runtime spawn and
/// UI publication. Any returned error or contained unwind closes the staged
/// registration; only the final `commit` disarms it.
pub struct PendingSessionAudio {
    controller: ApplicationAudioController,
    session_id: SessionId,
    scope: SessionAudioScope,
    committed: bool,
}

pub(crate) enum AbortedSessionSpawnError {
    Runtime(AudioSessionSpawnError),
    Publication {
        session_id: SessionId,
        failure: RuntimeThreadPublicationFailure,
    },
}

impl PendingSessionAudio {
    #[must_use]
    pub fn scope(&self) -> SessionAudioScope {
        self.scope.clone()
    }

    pub fn commit(mut self) {
        self.committed = true;
    }

    pub fn abort(mut self) -> SessionAudioCloseDisposition {
        self.committed = true;
        self.controller.close_session(self.session_id)
    }

    pub(crate) fn abort_spawn_error(
        self,
        error: AudioSessionSpawnError,
    ) -> AbortedSessionSpawnError {
        match error {
            AudioSessionSpawnError::RuntimePublication(error) => {
                let (session_id, failure, cleanup) = error.into_parts();
                let _ = self.abort_prejoined_parts(session_id, failure, cleanup);
                AbortedSessionSpawnError::Publication {
                    session_id,
                    failure,
                }
            }
            other => {
                let _ = self.abort();
                AbortedSessionSpawnError::Runtime(other)
            }
        }
    }

    fn abort_prejoined_parts(
        mut self,
        session_id: SessionId,
        failure: RuntimeThreadPublicationFailure,
        cleanup: RuntimeThreadJoinOutcome,
    ) -> SessionAudioCloseDisposition {
        if session_id != self.session_id || cleanup.session_id() != self.session_id {
            // Leave the guard armed: returning the typed rejection invokes the
            // ordinary close path from Drop, so a forged/mismatched proof can
            // never suppress exact lifecycle cleanup.
            return SessionAudioCloseDisposition::InvalidRuntimeProof;
        }
        self.committed = true;
        self.controller
            .close_prejoined_runtime(session_id, failure, cleanup)
    }
}

impl Drop for PendingSessionAudio {
    fn drop(&mut self) {
        if !self.committed {
            let disposition = self.controller.close_session(self.session_id);
            if !matches!(
                disposition,
                SessionAudioCloseDisposition::Requested
                    | SessionAudioCloseDisposition::AlreadyClosing
            ) {
                log::error!(
                    "staged audio rollback for session {} failed: {disposition:?}",
                    self.session_id
                );
            }
        }
    }
}

fn lock_state(state: &Mutex<CoordinatorState>) -> std::sync::MutexGuard<'_, CoordinatorState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn registration_failure(
    failure: smudgy_audio_web::SessionAudioRegistrationFailure,
) -> ApplicationAudioOpenError {
    let registration = failure.error();
    match block_on(failure.into_owner().retire()) {
        Ok(()) => ApplicationAudioOpenError::Registration(registration),
        Err(retirement) => ApplicationAudioOpenError::RegistrationRollback {
            registration,
            retirement,
        },
    }
}

#[derive(Clone)]
enum SessionRegistrationSource {
    Physical(MixerSessionRegistrar),
    Unavailable(SystemMixerUnavailable),
}

impl SessionRegistrationSource {
    fn register(
        &self,
        application: &ApplicationAudioRegistrar,
        session_id: SessionId,
    ) -> Result<CoordinatedSessionAudio, ApplicationAudioOpenError> {
        let id = AudioSessionId(u64::from(u32::from(session_id)));
        match self {
            Self::Physical(mixer) => {
                let owner = mixer
                    .add_session(id)
                    .map_err(ApplicationAudioOpenError::Mixer)?;
                application
                    .register_session(owner)
                    .map(CoordinatedSessionAudio::Physical)
                    .map_err(registration_failure)
            }
            Self::Unavailable(cause) => application
                .register_unavailable_session(
                    id,
                    UnavailableAudioOutputCause::new(cause.to_string()),
                )
                .map(CoordinatedSessionAudio::Unavailable)
                .map_err(ApplicationAudioOpenError::Registration),
        }
    }
}

fn open_registration(
    state: &Mutex<CoordinatorState>,
    source: &SessionRegistrationSource,
    application: &ApplicationAudioRegistrar,
    session_id: SessionId,
) -> Result<SessionAudioScope, ApplicationAudioOpenError> {
    {
        let state = lock_state(state);
        if !state.open {
            return Err(ApplicationAudioOpenError::ApplicationSealed);
        }
        if state.registrations.contains_key(&session_id) || state.closing.contains(&session_id) {
            return Err(ApplicationAudioOpenError::DuplicateSession);
        }
        if matches!(source, SessionRegistrationSource::Unavailable(_))
            && state.registrations.len() + state.closing.len() >= smudgy_audio::MAX_SESSIONS
        {
            return Err(ApplicationAudioOpenError::SessionCapacity);
        }
    }
    let registration = source.register(application, session_id)?;
    let scope = registration.scope();
    let mut state = lock_state(state);
    if !state.open {
        drop(state);
        return Err(match registration.retire() {
            SessionAudioRetirementResult::Physical(Err(retirement)) => {
                ApplicationAudioOpenError::RegistrationRollback {
                    registration: SessionAudioRegistrationError::ApplicationSealed,
                    retirement,
                }
            }
            SessionAudioRetirementResult::Physical(Ok(()))
            | SessionAudioRetirementResult::MixerFree(_) => {
                ApplicationAudioOpenError::ApplicationSealed
            }
        });
    }
    if state.registrations.contains_key(&session_id) || state.closing.contains(&session_id) {
        drop(state);
        let _ = registration.retire();
        return Err(ApplicationAudioOpenError::DuplicateSession);
    }
    state.registrations.insert(session_id, registration);
    Ok(scope)
}

fn take_close_job(
    state: &Mutex<CoordinatorState>,
    runtime_shutdown: &RuntimeShutdown,
    session_id: SessionId,
) -> Result<LifecycleJob, SessionAudioCloseDisposition> {
    let mut registration = {
        let mut state = lock_state(state);
        let Some(registration) = state.registrations.remove(&session_id) else {
            return Err(if state.closing.contains(&session_id) {
                SessionAudioCloseDisposition::AlreadyClosing
            } else {
                SessionAudioCloseDisposition::UnknownSession
            });
        };
        state.closing.insert(session_id);
        registration
    };
    registration.seal();
    Ok(LifecycleJob {
        session_id,
        shutdown: if runtime_shutdown(session_id) {
            RuntimeShutdownRequest::Requested
        } else {
            RuntimeShutdownRequest::AlreadyClosed
        },
        runtime: None,
        publication_failure: None,
        registration,
    })
}

fn take_prejoined_close_job(
    state: &Mutex<CoordinatorState>,
    session_id: SessionId,
    failure: RuntimeThreadPublicationFailure,
    cleanup: RuntimeThreadJoinOutcome,
) -> Result<LifecycleJob, SessionAudioCloseDisposition> {
    if cleanup.session_id() != session_id {
        return Err(SessionAudioCloseDisposition::InvalidRuntimeProof);
    }
    let mut registration = {
        let mut state = lock_state(state);
        let Some(registration) = state.registrations.remove(&session_id) else {
            return Err(if state.closing.contains(&session_id) {
                SessionAudioCloseDisposition::AlreadyClosing
            } else {
                SessionAudioCloseDisposition::UnknownSession
            });
        };
        state.closing.insert(session_id);
        registration
    };
    registration.seal();
    Ok(LifecycleJob {
        session_id,
        shutdown: RuntimeShutdownRequest::AlreadyClosed,
        runtime: Some(cleanup),
        publication_failure: Some(failure),
        registration,
    })
}

fn record_result(state: &Mutex<CoordinatorState>, result: SessionAudioCloseResult) {
    let mut state = lock_state(state);
    state.closing.remove(&result.session_id);
    state.submitted.remove(&result.session_id);
    state.completed.push(result);
}

fn submit_job(
    state: &Mutex<CoordinatorState>,
    lifecycle: &mpsc::Sender<LifecycleCommand>,
    runtime_join: &RuntimeJoin,
    job: LifecycleJob,
) {
    lock_state(state).submitted.insert(job.session_id);
    if let Err(mpsc::SendError(LifecycleCommand::Close(job))) =
        lifecycle.send(LifecycleCommand::Close(job))
    {
        let mut state_guard = lock_state(state);
        state_guard.submitted.remove(&job.session_id);
        state_guard.lifecycle_transport_clean = false;
        drop(state_guard);
        // No detach and no Drop-only fallback: the coordinator retains the
        // exact authority and performs the same proof-bearing work itself.
        record_result(state, finish_job(job, runtime_join));
    }
}

fn drain_completed(state: &Mutex<CoordinatorState>) {
    let mut state = lock_state(state);
    while let Ok(result) = state.completions.try_recv() {
        state.closing.remove(&result.session_id);
        state.submitted.remove(&result.session_id);
        state.completed.push(result);
    }
}

fn wait_for_all(state: &Mutex<CoordinatorState>) {
    loop {
        drain_completed(state);
        let mut state = lock_state(state);
        if state.closing.is_empty() {
            return;
        }
        match state.completions.recv() {
            Ok(result) => {
                state.closing.remove(&result.session_id);
                state.submitted.remove(&result.session_id);
                state.completed.push(result);
            }
            Err(_) => {
                state.lifecycle_transport_clean = false;
                return;
            }
        }
    }
}

fn quiesce_io_once(state: &Mutex<CoordinatorState>, io_quiesce: &IoQuiesce) {
    {
        let mut state = lock_state(state);
        if state.io_quiesce_attempted {
            return;
        }
        // Record the attempt before entering code that may unwind. Recovery
        // must never retry process-global I/O shutdown.
        state.io_quiesce_attempted = true;
    }
    if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| io_quiesce())) {
        // A panic payload may itself have a hostile destructor. The shutdown
        // report carries the failure without allowing a second unwind to lose
        // registration authority.
        std::mem::forget(payload);
        lock_state(state).io_quiesce_clean = false;
    }
}

#[allow(clippy::too_many_arguments)]
fn run_coordinator(
    commands: mpsc::Receiver<CoordinatorCommand>,
    state: Arc<Mutex<CoordinatorState>>,
    source: SessionRegistrationSource,
    application: ApplicationAudioRegistrar,
    runtime_shutdown: RuntimeShutdown,
    runtime_join: RuntimeJoin,
    io_quiesce: IoQuiesce,
    lifecycle: mpsc::Sender<LifecycleCommand>,
) {
    while let Ok(command) = commands.recv() {
        drain_completed(&state);
        match command {
            CoordinatorCommand::Open { session_id, reply } => {
                let _ = reply.send(open_registration(&state, &source, &application, session_id));
            }
            CoordinatorCommand::Close { session_id, reply } => {
                let result = take_close_job(&state, &runtime_shutdown, session_id);
                let disposition = match result {
                    Ok(job) => {
                        submit_job(&state, &lifecycle, &runtime_join, job);
                        SessionAudioCloseDisposition::Requested
                    }
                    Err(disposition) => disposition,
                };
                let _ = reply.send(disposition);
            }
            CoordinatorCommand::ClosePrejoined {
                session_id,
                failure,
                cleanup,
                reply,
            } => {
                let result = take_prejoined_close_job(&state, session_id, failure, cleanup);
                let disposition = match result {
                    Ok(job) => {
                        submit_job(&state, &lifecycle, &runtime_join, job);
                        SessionAudioCloseDisposition::Requested
                    }
                    Err(disposition) => disposition,
                };
                let _ = reply.send(disposition);
            }
            CoordinatorCommand::Finish { reply } => {
                let ids = {
                    let mut state = lock_state(&state);
                    state.open = false;
                    state.registrations.keys().copied().collect::<Vec<_>>()
                };
                // Signal and seal every session first. Only after the whole set
                // is non-enterable may process I/O quiesce and exact joins run.
                let mut jobs = Vec::with_capacity(ids.len());
                for session_id in ids {
                    if let Ok(job) = take_close_job(&state, &runtime_shutdown, session_id) {
                        jobs.push(job);
                    }
                }
                quiesce_io_once(&state, &io_quiesce);
                for job in jobs {
                    submit_job(&state, &lifecycle, &runtime_join, job);
                }
                wait_for_all(&state);
                let mut state = lock_state(&state);
                state.completed.sort_by_key(|result| result.session_id);
                let finish = CoordinatorFinish {
                    sessions: std::mem::take(&mut state.completed),
                    io_quiesce_attempted: state.io_quiesce_attempted,
                    io_quiesce_clean: state.io_quiesce_clean,
                    lifecycle_transport_clean: state.lifecycle_transport_clean,
                };
                drop(state);
                let _ = reply.send(finish);
                return;
            }
        }
    }
}

pub(crate) trait ProcessMixer: Sized {
    fn session_registrar(&self) -> MixerSessionRegistrar;
    fn shutdown(self) -> MixerShutdown;
}

impl ProcessMixer for SystemMixerService {
    fn session_registrar(&self) -> MixerSessionRegistrar {
        self.session_registrar()
    }
    fn shutdown(self) -> MixerShutdown {
        self.shutdown()
    }
}

#[cfg(test)]
impl ProcessMixer for smudgy_audio::MixerService {
    fn session_registrar(&self) -> MixerSessionRegistrar {
        self.session_registrar()
    }
    fn shutdown(self) -> MixerShutdown {
        self.shutdown()
    }
}

#[derive(Clone, Debug)]
pub enum ApplicationAudioAvailability {
    Physical,
    Unavailable(SystemMixerUnavailable),
}

/// Unique outer application authority, retained on `run`'s stack beyond iced.
pub struct ApplicationAudio<S = SystemMixerService> {
    service: Option<S>,
    availability: ApplicationAudioAvailability,
    owner: Option<ApplicationAudioOwner>,
    controller: ApplicationAudioController,
    state: Arc<Mutex<CoordinatorState>>,
    lifecycle: mpsc::Sender<LifecycleCommand>,
    runtime_shutdown: RuntimeShutdown,
    runtime_join: RuntimeJoin,
    runtime_join_all: RuntimeJoinAll,
    io_quiesce: IoQuiesce,
    coordinator: Option<JoinHandle<()>>,
    lifecycle_worker: Option<JoinHandle<()>>,
}

impl ApplicationAudio<SystemMixerService> {
    pub fn start() -> Result<Self, ApplicationAudioStartError> {
        let runtime_shutdown: RuntimeShutdown = Arc::new(request_runtime_shutdown);
        let runtime_join: RuntimeJoin = Arc::new(join_runtime);
        let runtime_join_all: RuntimeJoinAll =
            Arc::new(smudgy_core::session::runtime::join_all_runtime_threads);
        let io_quiesce: IoQuiesce = Arc::new(smudgy_core::session::connection::shutdown_io_runtime);
        match SystemMixerService::start(PHYSICAL_SAMPLE_RATE) {
            Ok(service) => match Self::with_service_and_runtime(
                service,
                production_limits(),
                runtime_shutdown,
                runtime_join,
                runtime_join_all,
                io_quiesce,
            ) {
                Ok(application) => Ok(application),
                Err((source, service)) => Err(ApplicationAudioStartError::Worker {
                    source,
                    cleanup: ApplicationAudioStartCleanup::Physical(service.shutdown()),
                }),
            },
            Err(error) => {
                let cause = SystemMixerUnavailable::from(error);
                Self::with_unavailable_and_runtime(
                    cause.clone(),
                    production_limits(),
                    runtime_shutdown,
                    runtime_join,
                    runtime_join_all,
                    io_quiesce,
                )
                .map_err(|source| ApplicationAudioStartError::Worker {
                    source,
                    cleanup: ApplicationAudioStartCleanup::Unavailable(cause),
                })
            }
        }
    }
}

impl<S: ProcessMixer> ApplicationAudio<S> {
    fn with_service_and_runtime(
        service: S,
        limits: AudioHostLimits,
        runtime_shutdown: RuntimeShutdown,
        runtime_join: RuntimeJoin,
        runtime_join_all: RuntimeJoinAll,
        io_quiesce: IoQuiesce,
    ) -> Result<Self, (std::io::Error, S)> {
        let source = SessionRegistrationSource::Physical(service.session_registrar());
        Self::with_source_and_runtime(
            Some(service),
            source,
            ApplicationAudioAvailability::Physical,
            limits,
            runtime_shutdown,
            runtime_join,
            runtime_join_all,
            io_quiesce,
        )
        .map_err(|(error, service)| {
            (
                error,
                service.expect("physical coordinator construction returns its service"),
            )
        })
    }

    fn with_unavailable_and_runtime(
        cause: SystemMixerUnavailable,
        limits: AudioHostLimits,
        runtime_shutdown: RuntimeShutdown,
        runtime_join: RuntimeJoin,
        runtime_join_all: RuntimeJoinAll,
        io_quiesce: IoQuiesce,
    ) -> Result<Self, std::io::Error> {
        Self::with_source_and_runtime(
            None,
            SessionRegistrationSource::Unavailable(cause.clone()),
            ApplicationAudioAvailability::Unavailable(cause),
            limits,
            runtime_shutdown,
            runtime_join,
            runtime_join_all,
            io_quiesce,
        )
        .map_err(|(error, service)| {
            debug_assert!(service.is_none());
            error
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn with_source_and_runtime(
        service: Option<S>,
        source: SessionRegistrationSource,
        availability: ApplicationAudioAvailability,
        limits: AudioHostLimits,
        runtime_shutdown: RuntimeShutdown,
        runtime_join: RuntimeJoin,
        runtime_join_all: RuntimeJoinAll,
        io_quiesce: IoQuiesce,
    ) -> Result<Self, (std::io::Error, Option<S>)> {
        let owner = ApplicationAudioOwner::new(limits);
        let application = owner.registrar();
        let (commands_tx, commands_rx) = mpsc::sync_channel(COORDINATOR_QUEUE_CAPACITY);
        // The coordinator enforces the same 32-session application bound in
        // both physical and unavailable modes, so this ownership channel is
        // intrinsically bounded by live authorities. An
        // unbounded transport keeps iced Close responsive when an earlier
        // exact runtime join is deliberately slow.
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        let (completed_tx, completions) = mpsc::channel();
        let state = Arc::new(Mutex::new(CoordinatorState {
            open: true,
            registrations: BTreeMap::new(),
            closing: BTreeSet::new(),
            submitted: BTreeSet::new(),
            completed: Vec::new(),
            completions,
            io_quiesce_attempted: false,
            io_quiesce_clean: true,
            lifecycle_transport_clean: true,
        }));
        let lifecycle_runtime_join = Arc::clone(&runtime_join);
        let lifecycle_worker = match thread::Builder::new()
            .name("smudgy-audio-lifecycle".to_string())
            .spawn(move || {
                while let Ok(command) = lifecycle_rx.recv() {
                    match command {
                        LifecycleCommand::Close(job) => {
                            if completed_tx
                                .send(finish_job(job, &lifecycle_runtime_join))
                                .is_err()
                            {
                                break;
                            }
                        }
                        LifecycleCommand::Shutdown => break,
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(error) => return Err((error, service)),
        };
        let coordinator_state = Arc::clone(&state);
        let coordinator_shutdown = Arc::clone(&runtime_shutdown);
        let coordinator_join = Arc::clone(&runtime_join);
        let coordinator_io = Arc::clone(&io_quiesce);
        let coordinator_lifecycle = lifecycle_tx.clone();
        let coordinator = match thread::Builder::new()
            .name("smudgy-audio-coordinator".to_string())
            .spawn(move || {
                run_coordinator(
                    commands_rx,
                    coordinator_state,
                    source,
                    application,
                    coordinator_shutdown,
                    coordinator_join,
                    coordinator_io,
                    coordinator_lifecycle,
                );
            }) {
            Ok(worker) => worker,
            Err(error) => {
                let _ = lifecycle_tx.send(LifecycleCommand::Shutdown);
                let _ = lifecycle_worker.join();
                return Err((error, service));
            }
        };
        Ok(Self {
            service,
            availability,
            owner: Some(owner),
            controller: ApplicationAudioController {
                commands: commands_tx,
                _ui_thread: PhantomData,
            },
            state,
            lifecycle: lifecycle_tx,
            runtime_shutdown,
            runtime_join,
            runtime_join_all,
            io_quiesce,
            coordinator: Some(coordinator),
            lifecycle_worker: Some(lifecycle_worker),
        })
    }

    #[must_use]
    pub fn controller(&self) -> ApplicationAudioController {
        self.controller.clone()
    }

    #[must_use]
    pub fn availability(&self) -> ApplicationAudioAvailability {
        self.availability.clone()
    }

    fn recover_coordinator_failure(&self) -> CoordinatorFinish {
        let ids = {
            let mut state = lock_state(&self.state);
            state.open = false;
            // A closing id without a durably submitted lifecycle job can only
            // result from an unexpected coordinator unwind. It has no
            // completion that recovery may safely wait for.
            let orphaned = state
                .closing
                .difference(&state.submitted)
                .copied()
                .collect::<Vec<_>>();
            if !orphaned.is_empty() {
                state.lifecycle_transport_clean = false;
                for session_id in orphaned {
                    state.closing.remove(&session_id);
                }
            }
            state.registrations.keys().copied().collect::<Vec<_>>()
        };
        let mut jobs = Vec::new();
        for id in ids {
            if let Ok(job) = take_close_job(&self.state, &self.runtime_shutdown, id) {
                jobs.push(job);
            }
        }
        quiesce_io_once(&self.state, &self.io_quiesce);
        for job in jobs {
            submit_job(&self.state, &self.lifecycle, &self.runtime_join, job);
        }
        wait_for_all(&self.state);
        let mut state = lock_state(&self.state);
        state.lifecycle_transport_clean = false;
        state.completed.sort_by_key(|result| result.session_id);
        CoordinatorFinish {
            sessions: std::mem::take(&mut state.completed),
            io_quiesce_attempted: state.io_quiesce_attempted,
            io_quiesce_clean: state.io_quiesce_clean,
            lifecycle_transport_clean: false,
        }
    }

    pub fn shutdown(mut self) -> ApplicationAudioShutdownReport {
        if let Some(owner) = self.owner.as_mut() {
            owner.seal();
        }
        let (reply, response) = mpsc::sync_channel(1);
        let finish = if self
            .controller
            .commands
            .send(CoordinatorCommand::Finish { reply })
            .is_ok()
        {
            response.recv().ok()
        } else {
            None
        };
        let coordinator_joined = self
            .coordinator
            .take()
            .is_some_and(|worker| worker.join().is_ok());
        let finish = finish.unwrap_or_else(|| self.recover_coordinator_failure());
        let _ = self.lifecycle.send(LifecycleCommand::Shutdown);
        let lifecycle_worker_joined = self
            .lifecycle_worker
            .take()
            .is_some_and(|worker| worker.join().is_ok());
        let unowned_runtime_joins = (self.runtime_join_all)();
        let output = match self.service.take() {
            Some(service) => ApplicationAudioOutputShutdown::Physical(service.shutdown()),
            None => match self.availability {
                ApplicationAudioAvailability::Unavailable(cause) => {
                    ApplicationAudioOutputShutdown::Unavailable(cause)
                }
                ApplicationAudioAvailability::Physical => {
                    unreachable!("physical application audio retains its service until shutdown")
                }
            },
        };
        self.owner.take();
        ApplicationAudioShutdownReport {
            sessions: finish.sessions,
            unowned_runtime_joins,
            io_quiesce_attempted: finish.io_quiesce_attempted,
            io_quiesce_clean: finish.io_quiesce_clean,
            coordinator_joined,
            lifecycle_worker_joined,
            lifecycle_transport_clean: finish.lifecycle_transport_clean,
            output,
        }
    }

    #[cfg(test)]
    pub(crate) fn seal_application_for_test(&mut self) {
        self.owner
            .as_mut()
            .expect("test application owner is live")
            .seal();
    }
}

#[cfg(test)]
pub(crate) fn test_application_audio_with_core_runtime() -> (
    ApplicationAudio<smudgy_audio::MixerService>,
    smudgy_audio::test_support::TestDriverProbe,
) {
    let (service, probe) = smudgy_audio::test_support::start_test_mixer(
        PHYSICAL_SAMPLE_RATE,
        smudgy_audio::test_support::TestDriverConfig::default(),
    )
    .expect("test mixer starts");
    let application = ApplicationAudio::with_service_and_runtime(
        service,
        production_limits(),
        Arc::new(request_runtime_shutdown),
        Arc::new(join_runtime),
        Arc::new(smudgy_core::session::runtime::join_all_runtime_threads),
        Arc::new(|| {}),
    )
    .unwrap_or_else(|(error, _)| panic!("test lifecycle workers start: {error}"));
    (application, probe)
}

#[cfg(test)]
static CORE_RUNTIME_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_core_runtime_test() -> std::sync::MutexGuard<'static, ()> {
    CORE_RUNTIME_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) struct TestAudioRenderer {
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

#[cfg(test)]
impl TestAudioRenderer {
    pub(crate) fn start(probe: smudgy_audio::test_support::TestDriverProbe) -> Self {
        use std::sync::atomic::Ordering;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let render_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !render_stop.load(Ordering::Acquire) {
                let mut output = [0.0; 256];
                let _ = probe.render(&mut output, 2);
                thread::yield_now();
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

#[cfg(test)]
impl Drop for TestAudioRenderer {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .expect("test renderer joins exactly once")
            .join()
            .expect("test renderer stays healthy");
    }
}

fn production_limits() -> AudioHostLimits {
    AudioHostLimits::unlimited()
        .max_online_contexts(Some(64))
        .max_live_audio_bytes(Some(64 * 1024 * 1024))
        .max_graph_nodes(Some(4_096))
        .max_graph_connections(Some(4_096))
        .max_scheduled_sources(Some(1_024))
        .max_automation_events(Some(4_096))
        .max_queued_control_commands(Some(1_024))
        .max_queued_events(Some(1_024))
        .max_decode_jobs(Some(4))
        .max_offline_render_jobs(Some(2))
}

#[cfg(test)]
mod tests {
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use smudgy_audio::test_support::{TestDriverConfig, TestDriverProbe, start_test_mixer};
    use smudgy_audio::{
        MixerOutputFailure, MixerService, MixerStartError, MixerStartupFailure, SystemOutputError,
    };

    use super::*;

    fn test_application(
        events: Arc<Mutex<Vec<String>>>,
    ) -> (ApplicationAudio<MixerService>, TestDriverProbe) {
        let join_events = Arc::clone(&events);
        test_application_with_join(
            events,
            Arc::new(move |id| {
                join_events.lock().unwrap().push(format!("join:{id}"));
                RuntimeThreadJoinOutcome::Clean { session_id: id }
            }),
        )
    }

    fn test_application_with_join(
        events: Arc<Mutex<Vec<String>>>,
        runtime_join: RuntimeJoin,
    ) -> (ApplicationAudio<MixerService>, TestDriverProbe) {
        let io_events = Arc::clone(&events);
        test_application_with_hooks(
            events,
            runtime_join,
            Arc::new(move || io_events.lock().unwrap().push("io".to_string())),
        )
    }

    fn test_application_with_hooks(
        events: Arc<Mutex<Vec<String>>>,
        runtime_join: RuntimeJoin,
        io_quiesce: IoQuiesce,
    ) -> (ApplicationAudio<MixerService>, TestDriverProbe) {
        let (service, probe) =
            start_test_mixer(PHYSICAL_SAMPLE_RATE, TestDriverConfig::default()).unwrap();
        let shutdown_events = events;
        let application = ApplicationAudio::with_service_and_runtime(
            service,
            production_limits(),
            Arc::new(move |id| {
                shutdown_events
                    .lock()
                    .unwrap()
                    .push(format!("shutdown:{id}"));
                true
            }),
            runtime_join,
            Arc::new(Vec::new),
            io_quiesce,
        )
        .unwrap_or_else(|(error, _)| panic!("test lifecycle workers start: {error}"));
        (application, probe)
    }

    fn test_unavailable_application(
        cause: SystemMixerUnavailable,
    ) -> ApplicationAudio<MixerService> {
        ApplicationAudio::<MixerService>::with_unavailable_and_runtime(
            cause,
            production_limits(),
            Arc::new(|_| false),
            Arc::new(|session_id| RuntimeThreadJoinOutcome::Clean { session_id }),
            Arc::new(Vec::new),
            Arc::new(|| {}),
        )
        .expect("mixer-free lifecycle workers start")
    }

    fn clean_unavailable_cause() -> SystemMixerUnavailable {
        SystemMixerUnavailable::from(MixerStartError::<SystemOutputError>::InvalidSampleRate)
    }

    #[test]
    fn optional_start_cleanup_truth_table_is_fail_closed() {
        let worker = |cleanup| ApplicationAudioStartError::Worker {
            source: std::io::Error::other("deterministic lifecycle worker failure"),
            cleanup,
        };
        assert!(
            worker(ApplicationAudioStartCleanup::Physical(MixerShutdown {
                clean: true,
                failure: None,
            }))
            .cleanup_is_clean()
        );
        assert!(
            !worker(ApplicationAudioStartCleanup::Physical(MixerShutdown {
                clean: false,
                failure: None,
            }))
            .cleanup_is_clean()
        );
        assert!(
            !worker(ApplicationAudioStartCleanup::Physical(MixerShutdown {
                clean: true,
                failure: Some(MixerOutputFailure::BackendFailure),
            }))
            .cleanup_is_clean()
        );
        assert!(
            worker(ApplicationAudioStartCleanup::Unavailable(
                clean_unavailable_cause()
            ))
            .cleanup_is_clean()
        );
        let uncertain =
            SystemMixerUnavailable::from(MixerStartError::CleanupUncertain(MixerStartupFailure::<
                SystemOutputError,
            >::DriverFailed(
                MixerOutputFailure::BackendFailure,
            )));
        assert!(!worker(ApplicationAudioStartCleanup::Unavailable(uncertain)).cleanup_is_clean());
        let owner_stopped =
            SystemMixerUnavailable::from(MixerStartError::<SystemOutputError>::OwnerStopped);
        assert!(
            !worker(ApplicationAudioStartCleanup::Unavailable(owner_stopped)).cleanup_is_clean()
        );
    }

    #[test]
    fn unavailable_coordinator_opens_siblings_rolls_back_and_shuts_down_without_mixer_proof() {
        let cause = clean_unavailable_cause();
        let application = test_unavailable_application(cause.clone());
        assert!(matches!(
            application.availability(),
            ApplicationAudioAvailability::Unavailable(ref retained)
                if matches!(retained.error(), MixerStartError::InvalidSampleRate)
        ));
        let controller = application.controller();

        let first_id = SessionId::from(70);
        let first = controller.begin_session(first_id).unwrap();
        assert_eq!(first.scope().session_id(), 70);
        first.commit();
        let second_id = SessionId::from(71);
        let second = controller.begin_session(second_id).unwrap();
        assert_eq!(second.scope().session_id(), 71);
        second.commit();
        assert_eq!(
            controller.close_session(first_id),
            SessionAudioCloseDisposition::Requested
        );

        // Closing one mixer-free generation does not disturb a live sibling,
        // and a fresh staged registration can still be rolled back exactly.
        let rollback_id = SessionId::from(72);
        let rollback = controller.begin_session(rollback_id).unwrap();
        assert_eq!(rollback.scope().session_id(), 72);
        assert_eq!(rollback.abort(), SessionAudioCloseDisposition::Requested);

        let report = application.shutdown();
        assert_eq!(report.sessions.len(), 3);
        for result in &report.sessions {
            assert_eq!(
                result.runtime,
                RuntimeThreadJoinOutcome::Clean {
                    session_id: result.session_id
                }
            );
            match result.retirement {
                SessionAudioRetirementResult::MixerFree(receipt) => {
                    assert_eq!(
                        receipt.session_id(),
                        u64::from(u32::from(result.session_id))
                    );
                    assert_ne!(receipt.generation(), 0);
                }
                SessionAudioRetirementResult::Physical(_) => {
                    panic!("unavailable session fabricated mixer retirement")
                }
            }
            assert!(result.is_clean());
        }
        assert!(matches!(
            report.output,
            ApplicationAudioOutputShutdown::Unavailable(ref retained)
                if matches!(retained.error(), MixerStartError::InvalidSampleRate)
        ));
        assert!(
            report.is_clean(),
            "clean unavailability is not shutdown failure"
        );
    }

    #[test]
    fn unavailable_seal_rejects_open_and_uncertain_startup_never_reports_clean() {
        let mut sealed = test_unavailable_application(clean_unavailable_cause());
        let controller = sealed.controller();
        sealed.seal_application_for_test();
        assert!(matches!(
            controller.begin_session(SessionId::from(73)),
            Err(ApplicationAudioOpenError::Registration(
                SessionAudioRegistrationError::ApplicationSealed
            ))
        ));
        let sealed_report = sealed.shutdown();
        assert!(sealed_report.sessions.is_empty());
        assert!(sealed_report.is_clean());

        let uncertain =
            SystemMixerUnavailable::from(MixerStartError::<SystemOutputError>::OwnerStopped);
        let uncertain_report = test_unavailable_application(uncertain).shutdown();
        assert!(matches!(
            uncertain_report.output,
            ApplicationAudioOutputShutdown::Unavailable(ref retained)
                if matches!(retained.error(), MixerStartError::OwnerStopped)
                    && !retained.cleanup_proven()
        ));
        assert!(
            !uncertain_report.is_clean(),
            "lost physical startup proof cannot become clean shutdown"
        );
    }

    #[test]
    fn unavailable_registration_keeps_session_bound_and_reuses_exact_close_capacity() {
        let application = test_unavailable_application(clean_unavailable_cause());
        let controller = application.controller();
        for id in 100..100 + smudgy_audio::MAX_SESSIONS as u32 {
            controller
                .begin_session(SessionId::from(id))
                .expect("bounded mixer-free registration")
                .commit();
        }
        assert!(matches!(
            controller.begin_session(SessionId::from(999)),
            Err(ApplicationAudioOpenError::SessionCapacity)
        ));
        assert_eq!(
            controller.close_session(SessionId::from(100)),
            SessionAudioCloseDisposition::Requested
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let replacement = loop {
            match controller.begin_session(SessionId::from(999)) {
                Ok(pending) => break pending,
                Err(ApplicationAudioOpenError::SessionCapacity) => {
                    assert!(
                        Instant::now() < deadline,
                        "exact close did not return capacity"
                    );
                    thread::yield_now();
                }
                Err(other) => panic!("unexpected replacement rejection: {other:?}"),
            }
        };
        replacement.commit();

        let report = application.shutdown();
        assert_eq!(report.sessions.len(), smudgy_audio::MAX_SESSIONS + 1);
        assert!(report.sessions.iter().all(|result| result.is_clean()));
        assert!(report.is_clean());
    }

    #[test]
    fn exact_clean_join_is_authoritative_when_shutdown_sender_is_already_absent() {
        let session_id = SessionId::from(6);
        let result = |shutdown, runtime, retirement| SessionAudioCloseResult {
            session_id,
            shutdown,
            runtime,
            publication_failure: None,
            retirement,
        };
        let physical = SessionAudioRetirementResult::Physical;

        assert!(
            result(
                RuntimeShutdownRequest::AlreadyClosed,
                RuntimeThreadJoinOutcome::Clean { session_id },
                physical(Ok(()))
            )
            .is_clean(),
            "an exact clean join proves an already-stopped runtime"
        );
        assert!(
            result(
                RuntimeShutdownRequest::Requested,
                RuntimeThreadJoinOutcome::Clean { session_id },
                physical(Ok(()))
            )
            .is_clean()
        );
        assert!(
            !result(
                RuntimeShutdownRequest::Requested,
                RuntimeThreadJoinOutcome::Clean {
                    session_id: SessionId::from(7),
                },
                physical(Ok(())),
            )
            .is_clean(),
            "a clean join for another id is not this session's proof"
        );
        for runtime in [
            RuntimeThreadJoinOutcome::NotTrackedOrAlreadyJoined { session_id },
            RuntimeThreadJoinOutcome::Panicked { session_id },
            RuntimeThreadJoinOutcome::SpawnFailed { session_id },
        ] {
            assert!(
                !result(RuntimeShutdownRequest::Requested, runtime, physical(Ok(()))).is_clean(),
                "a shutdown send cannot promote {runtime:?} into proof"
            );
        }
        assert!(
            !result(
                RuntimeShutdownRequest::Requested,
                RuntimeThreadJoinOutcome::Clean { session_id },
                physical(Err(MixerSessionRetirementError::CleanupFailed)),
            )
            .is_clean(),
            "runtime proof cannot hide failed audio retirement"
        );
    }

    #[test]
    fn coordinator_reports_already_stopped_runtime_clean_from_exact_join() {
        let (service, probe) =
            start_test_mixer(PHYSICAL_SAMPLE_RATE, TestDriverConfig::default()).unwrap();
        let application = ApplicationAudio::with_service_and_runtime(
            service,
            production_limits(),
            Arc::new(|_| false),
            Arc::new(|session_id| RuntimeThreadJoinOutcome::Clean { session_id }),
            Arc::new(Vec::new),
            Arc::new(|| {}),
        )
        .unwrap_or_else(|(error, _)| panic!("test lifecycle workers start: {error}"));
        let controller = application.controller();
        controller
            .begin_session(SessionId::from(7))
            .unwrap()
            .commit();
        assert_eq!(
            controller.close_session(SessionId::from(7)),
            SessionAudioCloseDisposition::Requested
        );

        let report = shutdown_while_rendering(application, probe);
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(
            report.sessions[0].shutdown,
            RuntimeShutdownRequest::AlreadyClosed
        );
        assert!(report.sessions[0].is_clean());
        assert!(report.is_clean(), "{report}");
    }

    #[test]
    fn prejoined_publication_failure_retires_without_double_join_or_unowned_handle() {
        let joins = Arc::new(AtomicUsize::new(0));
        let join_calls = Arc::clone(&joins);
        let (application, probe) = test_application_with_join(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(move |session_id| {
                join_calls.fetch_add(1, Ordering::AcqRel);
                RuntimeThreadJoinOutcome::Clean { session_id }
            }),
        );
        let session_id = SessionId::from(8);
        let pending = application.controller().begin_session(session_id).unwrap();
        assert_eq!(
            pending.abort_prejoined_parts(
                session_id,
                RuntimeThreadPublicationFailure::PublicationUnwound,
                RuntimeThreadJoinOutcome::Clean { session_id },
            ),
            SessionAudioCloseDisposition::Requested
        );

        let report = shutdown_while_rendering(application, probe);
        assert_eq!(
            joins.load(Ordering::Acquire),
            0,
            "exact join is not repeated"
        );
        assert!(report.unowned_runtime_joins.is_empty());
        assert_eq!(report.sessions.len(), 1);
        let result = report.sessions[0];
        assert_eq!(
            result.publication_failure,
            Some(RuntimeThreadPublicationFailure::PublicationUnwound)
        );
        assert_eq!(
            result.runtime,
            RuntimeThreadJoinOutcome::Clean { session_id }
        );
        assert!(result.retirement.is_clean_for(session_id));
        assert!(
            !result.is_clean(),
            "cleanup proof cannot erase startup failure"
        );
        assert!(!report.is_clean());
    }

    #[test]
    fn mismatched_prejoined_proof_falls_back_to_ordinary_exact_join() {
        let joins = Arc::new(AtomicUsize::new(0));
        let join_calls = Arc::clone(&joins);
        let (application, probe) = test_application_with_join(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(move |session_id| {
                join_calls.fetch_add(1, Ordering::AcqRel);
                RuntimeThreadJoinOutcome::Clean { session_id }
            }),
        );
        let session_id = SessionId::from(9);
        let pending = application.controller().begin_session(session_id).unwrap();
        assert_eq!(
            pending.abort_prejoined_parts(
                session_id,
                RuntimeThreadPublicationFailure::PublicationUnwound,
                RuntimeThreadJoinOutcome::Clean {
                    session_id: SessionId::from(10),
                },
            ),
            SessionAudioCloseDisposition::InvalidRuntimeProof
        );

        let report = shutdown_while_rendering(application, probe);
        assert_eq!(
            joins.load(Ordering::Acquire),
            1,
            "rejected proof leaves the rollback guard armed for exact join"
        );
        assert!(report.unowned_runtime_joins.is_empty());
        assert_eq!(report.sessions.len(), 1);
        let result = report.sessions[0];
        assert_eq!(result.shutdown, RuntimeShutdownRequest::Requested);
        assert_eq!(result.publication_failure, None);
        assert_eq!(
            result.runtime,
            RuntimeThreadJoinOutcome::Clean { session_id }
        );
        assert!(result.is_clean());
        assert!(report.is_clean(), "{report}");
    }

    fn shutdown_while_rendering(
        application: ApplicationAudio<MixerService>,
        probe: TestDriverProbe,
    ) -> ApplicationAudioShutdownReport {
        let stop = Arc::new(AtomicBool::new(false));
        let render_stop = Arc::clone(&stop);
        let renderer = thread::spawn(move || {
            while !render_stop.load(Ordering::Acquire) {
                let mut output = [0.0; 256];
                let _ = probe.render(&mut output, 2);
                thread::yield_now();
            }
        });
        let report = application.shutdown();
        stop.store(true, Ordering::Release);
        renderer.join().unwrap();
        report
    }

    #[test]
    fn fixed_rate_duplicate_close_and_join_barrier_are_exact() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (application, probe) = test_application(Arc::clone(&events));
        assert_eq!(
            application.service.as_ref().unwrap().format().sample_rate(),
            48_000
        );
        let controller = application.controller();
        controller
            .begin_session(SessionId::from(7))
            .unwrap()
            .commit();
        assert_eq!(
            controller.close_session(SessionId::from(7)),
            SessionAudioCloseDisposition::Requested
        );
        assert_eq!(
            controller.close_session(SessionId::from(7)),
            SessionAudioCloseDisposition::AlreadyClosing
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while events.lock().unwrap().len() < 2 {
            assert!(Instant::now() < deadline, "runtime join did not begin");
            thread::yield_now();
        }
        assert_eq!(&*events.lock().unwrap(), &["shutdown:7", "join:7"]);
        let report = shutdown_while_rendering(application, probe);
        assert!(report.is_clean(), "{report}");
    }

    #[test]
    fn quit_signals_every_session_then_quiesces_io_before_join() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (application, probe) = test_application(Arc::clone(&events));
        let controller = application.controller();
        controller
            .begin_session(SessionId::from(10))
            .unwrap()
            .commit();
        controller
            .begin_session(SessionId::from(11))
            .unwrap()
            .commit();
        let report = shutdown_while_rendering(application, probe);
        let events = events.lock().unwrap();
        let io = events.iter().position(|event| event == "io").unwrap();
        let joins = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| event.starts_with("join:").then_some(index))
            .collect::<Vec<_>>();
        assert!(
            events[..io]
                .iter()
                .filter(|event| event.starts_with("shutdown:"))
                .count()
                == 2
        );
        assert!(joins.into_iter().all(|join| join > io));
        assert_eq!(report.sessions.len(), 2);
        assert!(report.is_clean(), "{report}");
    }

    #[test]
    fn application_seal_rejects_new_registration_without_publication() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (mut application, probe) = test_application(events);
        let controller = application.controller();
        application.owner.as_mut().unwrap().seal();
        let rollback_probe = probe.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let render_stop = Arc::clone(&stop);
        let renderer = thread::spawn(move || {
            while !render_stop.load(Ordering::Acquire) {
                let mut output = [0.0; 256];
                let _ = rollback_probe.render(&mut output, 2);
                thread::yield_now();
            }
        });
        let result = controller.begin_session(SessionId::from(20));
        stop.store(true, Ordering::Release);
        renderer.join().unwrap();
        assert!(matches!(
            result,
            Err(ApplicationAudioOpenError::Registration(
                SessionAudioRegistrationError::ApplicationSealed
            ))
        ));
        let report = shutdown_while_rendering(application, probe);
        assert!(
            report.sessions.is_empty(),
            "failed open was never published"
        );
        assert!(report.is_clean(), "{report}");
    }

    #[test]
    fn mass_close_stays_responsive_while_first_exact_join_is_blocked() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let release = Arc::new(std::sync::Barrier::new(2));
        let join_release = Arc::clone(&release);
        let join_events = Arc::clone(&events);
        let (application, probe) = test_application_with_join(
            Arc::clone(&events),
            Arc::new(move |id| {
                join_events.lock().unwrap().push(format!("join:{id}"));
                if id == SessionId::from(1) {
                    let _ = entered_tx.send(());
                    join_release.wait();
                }
                RuntimeThreadJoinOutcome::Clean { session_id: id }
            }),
        );
        let controller = application.controller();
        for id in 1..=32 {
            controller
                .begin_session(SessionId::from(id))
                .unwrap()
                .commit();
        }
        assert!(matches!(
            controller.begin_session(SessionId::from(1)),
            Err(ApplicationAudioOpenError::DuplicateSession)
        ));
        assert!(matches!(
            controller.begin_session(SessionId::from(33)),
            Err(ApplicationAudioOpenError::Mixer(
                MixerControlError::SessionCapacity
            ))
        ));
        assert_eq!(
            controller.close_session(SessionId::from(1)),
            SessionAudioCloseDisposition::Requested
        );
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let started = Instant::now();
        for id in 2..=32 {
            assert_eq!(
                controller.close_session(SessionId::from(id)),
                SessionAudioCloseDisposition::Requested
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "iced close commands blocked behind the first runtime join"
        );
        release.wait();
        let report = shutdown_while_rendering(application, probe);
        assert_eq!(report.sessions.len(), 32);
        assert!(report.is_clean(), "{report}");
    }

    #[test]
    fn quiesce_panic_still_joins_retires_and_shuts_output_for_iced_ok_or_error() {
        for iced_failed in [false, true] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let join_events = Arc::clone(&events);
            let io_events = Arc::clone(&events);
            let (application, probe) = test_application_with_hooks(
                Arc::clone(&events),
                Arc::new(move |id| {
                    join_events.lock().unwrap().push(format!("join:{id}"));
                    RuntimeThreadJoinOutcome::Clean { session_id: id }
                }),
                Arc::new(move || {
                    io_events.lock().unwrap().push("io:panic".to_string());
                    panic!("injected I/O quiesce panic");
                }),
            );
            let controller = application.controller();
            for id in [50, 51] {
                controller
                    .begin_session(SessionId::from(id))
                    .unwrap()
                    .commit();
            }

            let report = shutdown_while_rendering(application, probe);
            assert!(report.io_quiesce_attempted);
            assert!(!report.io_quiesce_clean);
            assert_eq!(report.sessions.len(), 2);
            assert!(
                report
                    .sessions
                    .iter()
                    .all(|result| result.retirement.is_clean_for(result.session_id))
            );
            assert!(report.output.is_clean(), "physical output still shut down");
            assert!(!report.is_clean(), "the quiesce panic remains visible");
            let events = events.lock().unwrap();
            let io = events.iter().position(|event| event == "io:panic").unwrap();
            assert!(
                events[..io]
                    .iter()
                    .filter(|event| event.starts_with("shutdown:"))
                    .count()
                    == 2
            );
            assert!(
                events[io + 1..]
                    .iter()
                    .all(|event| event.starts_with("join:"))
            );
            drop(events);

            let combined = if iced_failed {
                crate::finish_run_with_audio::<anyhow::Error>(
                    Err(anyhow::anyhow!("injected iced failure")),
                    report,
                )
            } else {
                crate::finish_run_with_audio::<anyhow::Error>(Ok(()), report)
            };
            let combined = combined.expect_err("the quiesce panic is never reported as clean");
            let message = combined.to_string();
            assert!(message.contains("I/O quiesce clean=false"));
            assert_eq!(message.contains("injected iced failure"), iced_failed);
        }
    }

    #[test]
    fn uncommitted_open_guard_retires_after_spawn_failure_or_panic() {
        for outcome in [
            RuntimeThreadJoinOutcome::SpawnFailed {
                session_id: SessionId::from(40),
            },
            RuntimeThreadJoinOutcome::Panicked {
                session_id: SessionId::from(40),
            },
        ] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let join_events = Arc::clone(&events);
            let (application, probe) = test_application_with_join(
                Arc::clone(&events),
                Arc::new(move |id| {
                    join_events.lock().unwrap().push(format!("join:{id}"));
                    outcome
                }),
            );
            let pending = application
                .controller()
                .begin_session(SessionId::from(40))
                .unwrap();
            if matches!(outcome, RuntimeThreadJoinOutcome::SpawnFailed { .. }) {
                assert_eq!(
                    pending.abort(),
                    SessionAudioCloseDisposition::Requested,
                    "ordinary spawn failure takes the explicit rollback path"
                );
            } else {
                let unwind = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let _rollback_guard = pending;
                    panic!("injected construction panic");
                }));
                assert!(unwind.is_err(), "the rollback guard survived the unwind");
            }
            let report = shutdown_while_rendering(application, probe);
            assert_eq!(report.sessions.len(), 1);
            assert_eq!(report.sessions[0].runtime, outcome);
            assert!(
                report.sessions[0]
                    .retirement
                    .is_clean_for(report.sessions[0].session_id)
            );
            assert!(!report.is_clean(), "spawn failure/panic must stay visible");
            let events = events.lock().unwrap();
            assert_eq!(events[0], "shutdown:40");
            assert_eq!(events[1], "join:40");
            assert_eq!(events.iter().filter(|event| *event == "join:40").count(), 1);
        }
    }
}
