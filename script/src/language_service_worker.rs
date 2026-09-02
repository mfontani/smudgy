//! Background owner for the embedded authoring-time TypeScript service.
//!
//! The UI sends transport-neutral [`Command`] values through a cheap cloneable
//! [`LanguageServiceClient`]. A dedicated thread owns V8 and emits generation-fenced
//! [`EventEnvelope`] values. Dropping the host requests shutdown and hands the join to a
//! reaper thread, so closing the Automations window never keeps the service reachable or
//! blocks the UI while V8 tears down.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::language_service::{
    AcknowledgedState, AttachView, ChangeDocument, CloseDocument, CloseProject, Command,
    CommandEnvelope, CommandSequence, DocumentDescriptor, DocumentId, DocumentKey, DocumentResult,
    DocumentResultIdentity, DocumentStateIdentity, Event, EventEnvelope, FailureScope,
    GraphGeneration, Language, LanguageServiceLibrary, MAX_PROJECT_SOURCE_FILES,
    MAX_PROJECT_SOURCE_TEXT_BYTES, MAX_URI_BYTES, OpenDocument, OpenProject, PROTOCOL_VERSION,
    ProjectScope, ProjectSource, ProjectStateIdentity, ProjectStatus, ProjectStatusEvent,
    RefreshProject, RequestFailure, ServiceGeneration, Validate, ViewId, ViewRef, WorkerGeneration,
};
use crate::language_service_engine::{EmbeddedLanguageService, ProjectFile};

const EVENT_QUEUE_CAPACITY: usize = 32;
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Cloneable ordered command endpoint for one language-service worker.
///
/// Commands use an unbounded standard channel so lifecycle commands cannot be
/// lost behind a burst of edits. The Automations controller still coalesces to
/// full-snapshot recovery when a send fails because a stopped worker is the
/// only remaining send failure.
#[derive(Clone)]
pub struct LanguageServiceClient {
    commands: mpsc::Sender<CommandEnvelope>,
    next_sequence: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
}

impl LanguageServiceClient {
    /// Queues a command and returns its correlation sequence.
    pub fn send(&self, command: Command) -> Result<CommandSequence, LanguageServiceSendError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(LanguageServiceSendError::Stopped);
        }
        let value = self
            .next_sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                CommandSequence::new(current).and_then(|_| current.checked_add(1))
            })
            .map_err(|_| LanguageServiceSendError::SequenceExhausted)?;
        let sequence =
            CommandSequence::new(value).ok_or(LanguageServiceSendError::SequenceExhausted)?;
        self.commands
            .send(CommandEnvelope {
                protocol_version: PROTOCOL_VERSION,
                command_sequence: sequence,
                command,
            })
            .map_err(|_| LanguageServiceSendError::Stopped)?;
        Ok(sequence)
    }
}

/// Failure to enqueue a language-service command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageServiceSendError {
    SequenceExhausted,
    Stopped,
}

impl fmt::Display for LanguageServiceSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceExhausted => {
                formatter.write_str("language-service command sequence exhausted")
            }
            Self::Stopped => formatter.write_str("language-service worker has stopped"),
        }
    }
}

impl std::error::Error for LanguageServiceSendError {}

/// Window-scoped language-service owner.
pub struct LanguageServiceHost {
    client: LanguageServiceClient,
    events: Option<mpsc::Receiver<EventEnvelope>>,
    stop: mpsc::Sender<()>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl LanguageServiceHost {
    /// Tries to start a worker thread. V8 boots on that thread after this method returns.
    ///
    /// A caller which can degrade to syntax-only editing should prefer this constructor;
    /// failure to allocate an OS thread is not a reason to make the editor unavailable.
    pub fn try_spawn() -> std::io::Result<Self> {
        Self::try_spawn_with_libraries(Vec::new())
    }

    /// Tries to start a worker with an immutable authoring-time declaration snapshot.
    ///
    /// The supplied libraries are moved directly to the worker thread and installed
    /// before any project command can run. They are released with the worker and never
    /// enter the editable-document lifecycle.
    pub fn try_spawn_with_libraries(
        libraries: Vec<LanguageServiceLibrary>,
    ) -> std::io::Result<Self> {
        let (commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let (stop_tx, stop_rx) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_generation = allocate_worker_generation();
        let client = LanguageServiceClient {
            commands: commands_tx,
            next_sequence: Arc::new(AtomicU64::new(1)),
            stopping: Arc::clone(&stopping),
        };
        let worker = thread::Builder::new()
            .name("smudgy-language-service".to_owned())
            .spawn(move || {
                run_worker(
                    commands_rx,
                    stop_rx,
                    &events_tx,
                    worker_generation,
                    libraries,
                );
            })?;
        Ok(Self {
            client,
            events: Some(events_rx),
            stop: stop_tx,
            stopping,
            worker: Some(worker),
        })
    }

    /// Starts a worker thread for callers which cannot usefully recover from an OS
    /// thread-allocation failure.
    #[must_use]
    pub fn spawn() -> Self {
        Self::try_spawn().expect("language-service worker thread must spawn")
    }

    /// Returns a command endpoint suitable for editor LSP hooks.
    #[must_use]
    pub fn client(&self) -> LanguageServiceClient {
        self.client.clone()
    }

    /// Drains every event currently ready without blocking the UI thread.
    pub fn drain_events(&mut self) -> Vec<EventEnvelope> {
        let mut events = Vec::new();
        if let Some(receiver) = &self.events {
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        }
        events
    }

    /// Requests an orderly shutdown and waits for V8 to tear down.
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.request_stop();
        self.events.take();
        self.worker.take().map_or(Ok(()), JoinHandle::join)
    }

    fn request_stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.stop.send(());
    }
}

impl Drop for LanguageServiceHost {
    fn drop(&mut self) {
        self.request_stop();
        self.events.take();
        if let Some(worker) = self.worker.take() {
            // V8 teardown is finite during ordinary operation but does not belong on the
            // GUI thread. The process-global registry retains the reaper's join handle so
            // application exit cannot abandon Deno's temporary data directory mid-drop.
            let worker_slot = Arc::new(Mutex::new(Some(worker)));
            let reaper_worker = Arc::clone(&worker_slot);
            match thread::Builder::new()
                .name("smudgy-language-service-reaper".to_owned())
                .spawn(move || {
                    let worker = reaper_worker
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if let Some(worker) = worker {
                        let _ = worker.join();
                    }
                }) {
                Ok(reaper) => register_language_service_reaper(reaper),
                Err(error) => {
                    // Thread creation failure is exceptional. Prefer finite synchronous
                    // cleanup to detaching V8 and leaking its temporary data directory.
                    log::error!("failed to start language-service reaper: {error}");
                    let worker = worker_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if let Some(worker) = worker {
                        let _ = worker.join();
                    }
                }
            }
        }
    }
}

fn language_service_reapers() -> &'static Mutex<Vec<JoinHandle<()>>> {
    static REAPERS: OnceLock<Mutex<Vec<JoinHandle<()>>>> = OnceLock::new();
    REAPERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn join_language_service_reaper(reaper: JoinHandle<()>) {
    if reaper.join().is_err() {
        log::error!("language-service reaper thread panicked");
    }
}

fn register_language_service_reaper(reaper: JoinHandle<()>) {
    let completed = {
        let mut reapers = language_service_reapers()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut completed = Vec::new();
        let mut index = 0;
        while index < reapers.len() {
            if reapers[index].is_finished() {
                completed.push(reapers.swap_remove(index));
            } else {
                index += 1;
            }
        }
        reapers.push(reaper);
        completed
    };
    for reaper in completed {
        join_language_service_reaper(reaper);
    }
}

/// Joins every language-service teardown which began before this call.
///
/// The application calls this after Iced has dropped all windows. Ordinary window closes
/// remain nonblocking, while the final process-exit boundary waits for each embedded Deno
/// runtime to release its temporary data directory. No new host may be created concurrently
/// with the application-exit call.
pub fn join_language_service_reapers() {
    let mut reapers = language_service_reapers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for reaper in reapers.drain(..) {
        join_language_service_reaper(reaper);
    }
}

fn run_worker(
    commands: mpsc::Receiver<CommandEnvelope>,
    stop: mpsc::Receiver<()>,
    events: &mpsc::SyncSender<EventEnvelope>,
    worker_generation: WorkerGeneration,
    libraries: Vec<LanguageServiceLibrary>,
) {
    let mut state = match EmbeddedLanguageService::new_with_libraries(libraries) {
        Ok(engine) => WorkerState::new(engine, worker_generation),
        Err(error) => {
            report_boot_failure(commands, stop, events, worker_generation, &error);
            return;
        }
    };

    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        let envelope = match commands.recv_timeout(STOP_POLL_INTERVAL) {
            Ok(envelope) => envelope,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if stop.try_recv().is_ok() {
            break;
        }
        let sequence = envelope.command_sequence;
        if let Err(error) = envelope.validate() {
            if !send_event(
                events,
                sequence,
                Event::RequestFailed(worker_failure(worker_generation, &error)),
                worker_generation,
            ) {
                break;
            }
            continue;
        }
        let shutdown = matches!(envelope.command, Command::Shutdown);
        let command_failure_scope = failure_scope(&state, &envelope.command);
        match state.handle(envelope.command) {
            Ok(produced) => {
                for event in produced {
                    if !send_event(events, sequence, event, worker_generation) {
                        return;
                    }
                }
            }
            Err(error) => {
                let failure = RequestFailure {
                    scope: command_failure_scope,
                    code: "language_service_command_failed".to_owned(),
                    retryable: true,
                    user_message: "Language intelligence is temporarily unavailable.".to_owned(),
                    log_detail: Some(truncate_error(&error)),
                };
                if !send_event(
                    events,
                    sequence,
                    Event::RequestFailed(failure),
                    worker_generation,
                ) {
                    return;
                }
            }
        }
        if shutdown {
            break;
        }
    }
}

fn report_boot_failure(
    commands: mpsc::Receiver<CommandEnvelope>,
    stop: mpsc::Receiver<()>,
    events: &mpsc::SyncSender<EventEnvelope>,
    worker_generation: WorkerGeneration,
    error: &anyhow::Error,
) {
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        let envelope = match commands.recv_timeout(STOP_POLL_INTERVAL) {
            Ok(envelope) => envelope,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let shutdown = matches!(envelope.command, Command::Shutdown);
        if !send_event(
            events,
            envelope.command_sequence,
            Event::RequestFailed(RequestFailure {
                scope: FailureScope::Worker { worker_generation },
                code: "language_service_boot_failed".to_owned(),
                retryable: false,
                user_message: "Language intelligence could not start.".to_owned(),
                log_detail: Some(truncate_error(error)),
            }),
            worker_generation,
        ) {
            break;
        }
        if shutdown {
            break;
        }
    }
}

fn send_event(
    events: &mpsc::SyncSender<EventEnvelope>,
    command_sequence: CommandSequence,
    event: Event,
    worker_generation: WorkerGeneration,
) -> bool {
    let mut envelope = EventEnvelope {
        protocol_version: PROTOCOL_VERSION,
        command_sequence,
        event,
    };
    if let Err(error) = envelope.validate() {
        log::error!("language-service worker produced an invalid event: {error}");
        envelope.event = Event::RequestFailed(RequestFailure {
            scope: FailureScope::Worker { worker_generation },
            code: "invalid_language_service_result".to_owned(),
            retryable: true,
            user_message: "Language intelligence returned an invalid result.".to_owned(),
            log_detail: Some(error.to_string()),
        });
        debug_assert!(envelope.validate().is_ok());
    }
    events.send(envelope).is_ok()
}

fn allocate_worker_generation() -> WorkerGeneration {
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
    let value = NEXT_GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            WorkerGeneration::new(current).and_then(|_| current.checked_add(1))
        })
        .expect("language-service worker generation exhausted");
    WorkerGeneration::new(value).expect("allocated language-service worker generation is valid")
}

fn worker_failure(
    worker_generation: WorkerGeneration,
    error: &impl fmt::Display,
) -> RequestFailure {
    RequestFailure {
        scope: FailureScope::Worker { worker_generation },
        code: "invalid_language_service_command".to_owned(),
        retryable: false,
        user_message: "Language intelligence rejected an invalid request.".to_owned(),
        log_detail: Some(error.to_string()),
    }
}

fn truncate_error(error: &anyhow::Error) -> String {
    const MAX_LOG_CHARS: usize = 16 * 1024;
    error.to_string().chars().take(MAX_LOG_CHARS).collect()
}

#[derive(Clone)]
struct OpenedDocument {
    descriptor: DocumentDescriptor,
    text: String,
    file_name: String,
}

#[derive(Clone)]
struct ProjectState {
    graph_generation: GraphGeneration,
    service_generation: ServiceGeneration,
    views: HashMap<ViewId, ViewRef>,
    /// Atomically replaced saved project graph. Open documents are unsaved
    /// overlays and shadow an entry with the same document ID.
    base_sources: HashMap<DocumentId, ProjectFile>,
}

struct WorkerState {
    engine: EmbeddedLanguageService,
    worker_generation: WorkerGeneration,
    next_service_generation: u64,
    projects: HashMap<ProjectScope, ProjectState>,
    documents: HashMap<DocumentKey, OpenedDocument>,
    active_engine_project: Option<ProjectScope>,
}

impl WorkerState {
    fn new(engine: EmbeddedLanguageService, worker_generation: WorkerGeneration) -> Self {
        Self {
            engine,
            worker_generation,
            next_service_generation: 1,
            projects: HashMap::new(),
            documents: HashMap::new(),
            active_engine_project: None,
        }
    }

    fn handle(&mut self, command: Command) -> Result<Vec<Event>> {
        match command {
            Command::OpenProject(command) => self.open_project(command),
            Command::RefreshProject(command) => self.refresh_project(command),
            Command::CloseProject(command) => self.close_project(command),
            Command::AttachView(command) => self.attach_view(command),
            Command::DetachView(command) => self.detach_view(command),
            Command::OpenDocument(command) => self.open_document(command),
            Command::ChangeDocument(command) => self.change_document(command),
            Command::SaveDocument(command) => self.save_document(command),
            Command::CloseDocument(command) => self.close_document(command),
            Command::RequestDiagnostics(command) => self.request_diagnostics(command.identity),
            Command::RequestCompletion(command) => {
                let current = self.require_request(command.identity)?;
                self.ensure_engine_project(current.document.key.project)?;
                let result = self
                    .engine
                    .completion(current.document.key.document_id, command.position)?;
                Ok(vec![Event::Completion(DocumentResult {
                    identity: command.identity,
                    analyzed_uri: self.current_uri(command.identity)?,
                    result,
                })])
            }
            Command::RequestHover(command) => {
                let current = self.require_request(command.identity)?;
                self.ensure_engine_project(current.document.key.project)?;
                let result = self
                    .engine
                    .hover(current.document.key.document_id, command.position)?;
                Ok(vec![Event::Hover(DocumentResult {
                    identity: command.identity,
                    analyzed_uri: self.current_uri(command.identity)?,
                    result,
                })])
            }
            Command::RequestSignatureHelp(command) => {
                let current = self.require_request(command.identity)?;
                self.ensure_engine_project(current.document.key.project)?;
                let result = self
                    .engine
                    .signature_help(current.document.key.document_id, command.position)?;
                Ok(vec![Event::SignatureHelp(DocumentResult {
                    identity: command.identity,
                    analyzed_uri: self.current_uri(command.identity)?,
                    result,
                })])
            }
            Command::RequestDefinition(command) => {
                let current = self.require_request(command.identity)?;
                self.ensure_engine_project(current.document.key.project)?;
                let result = self
                    .engine
                    .definition(current.document.key.document_id, command.position)?;
                Ok(vec![Event::Definition(DocumentResult {
                    identity: command.identity,
                    analyzed_uri: self.current_uri(command.identity)?,
                    result,
                })])
            }
            Command::RequestFormatting(command) => {
                let current = self.require_request(command.identity)?;
                self.ensure_engine_project(current.document.key.project)?;
                let result = self
                    .engine
                    .formatting(current.document.key.document_id, command.options)?;
                Ok(vec![Event::Formatting(DocumentResult {
                    identity: command.identity,
                    analyzed_uri: self.current_uri(command.identity)?,
                    result,
                })])
            }
            Command::Cancel(command) => {
                let project = self.project_identity(command.project)?;
                Ok(vec![Event::StateAcknowledged(
                    AcknowledgedState::RequestCanceled {
                        project,
                        request_id: command.request_id,
                    },
                )])
            }
            Command::Shutdown => Ok(vec![Event::StateAcknowledged(
                AcknowledgedState::ShutdownAccepted {
                    worker_generation: self.worker_generation,
                },
            )]),
        }
    }

    fn open_project(&mut self, command: OpenProject) -> Result<Vec<Event>> {
        if self.projects.contains_key(&command.project) {
            bail!("language-service project is already open");
        }
        let service_generation = self.allocate_service_generation()?;
        self.projects.insert(
            command.project,
            ProjectState {
                graph_generation: GraphGeneration::new(1).expect("one is a valid graph generation"),
                service_generation,
                views: HashMap::new(),
                base_sources: HashMap::new(),
            },
        );
        let identity = self.project_identity(command.project)?;
        Ok(vec![
            Event::StateAcknowledged(AcknowledgedState::ProjectOpened(identity)),
            Event::ProjectStatus(ProjectStatusEvent {
                identity,
                status: ProjectStatus::Ready,
            }),
        ])
    }

    fn refresh_project(&mut self, command: RefreshProject) -> Result<Vec<Event>> {
        let RefreshProject {
            project,
            graph_generation,
            sources,
        } = command;
        let previous_generation = self
            .projects
            .get(&project)
            .context("refresh of unopened language-service project")?
            .graph_generation;
        if graph_generation <= previous_generation {
            bail!("language-service graph generation is stale");
        }
        let next_sources = project_sources(project, sources)?;
        let previous_sources = {
            let state = self
                .projects
                .get_mut(&project)
                .context("project disappeared during refresh")?;
            let previous_sources = std::mem::replace(&mut state.base_sources, next_sources);
            state.graph_generation = graph_generation;
            previous_sources
        };
        if let Err(error) = self.sync_engine_project(project) {
            let state = self
                .projects
                .get_mut(&project)
                .context("project disappeared while rolling back refresh")?;
            state.graph_generation = previous_generation;
            state.base_sources = previous_sources;
            return Err(error);
        }
        let identity = self.project_identity(project)?;
        Ok(vec![
            Event::StateAcknowledged(AcknowledgedState::ProjectRefreshed(identity)),
            Event::ProjectStatus(ProjectStatusEvent {
                identity,
                status: ProjectStatus::Ready,
            }),
        ])
    }

    fn close_project(&mut self, command: CloseProject) -> Result<Vec<Event>> {
        let identity = self.project_identity(command.project)?;
        let project = self
            .projects
            .remove(&command.project)
            .context("project disappeared during close")?;
        let document_keys = self
            .documents
            .keys()
            .filter(|key| key.project == command.project)
            .copied()
            .collect::<Vec<_>>();
        let removed_documents = document_keys
            .into_iter()
            .filter_map(|key| self.documents.remove(&key).map(|document| (key, document)))
            .collect::<Vec<_>>();
        if self.active_engine_project == Some(command.project) {
            if let Err(error) = self.engine.replace_project(Vec::new()) {
                self.projects.insert(command.project, project);
                self.documents.extend(removed_documents);
                return Err(error).context("clear closing language-service project");
            }
            self.active_engine_project = None;
        }
        Ok(vec![Event::StateAcknowledged(
            AcknowledgedState::ProjectClosed(identity),
        )])
    }

    fn attach_view(&mut self, command: AttachView) -> Result<Vec<Event>> {
        let previous = self
            .projects
            .get(&command.project)
            .context("view attachment for unopened language-service project")?
            .views
            .get(&command.view.view_id)
            .copied();
        if let Some(current) = previous {
            if command.view.generation <= current.generation {
                bail!("language-service view generation is stale");
            }
        }
        let removed_documents = previous.map_or_else(Vec::new, |previous| {
            self.take_view_documents(command.project, previous)
        });
        if !removed_documents.is_empty() {
            if let Err(error) = self.sync_engine_project(command.project) {
                self.documents.extend(removed_documents);
                return Err(error).context("replace attached language-service view");
            }
        }
        self.projects
            .get_mut(&command.project)
            .context("project disappeared during view attachment")?
            .views
            .insert(command.view.view_id, command.view);
        let project = self.project_identity(command.project)?;
        Ok(vec![Event::StateAcknowledged(
            AcknowledgedState::ViewAttached {
                project,
                view: command.view,
            },
        )])
    }

    fn detach_view(&mut self, command: crate::language_service::DetachView) -> Result<Vec<Event>> {
        let current = self
            .projects
            .get(&command.project)
            .context("view detachment for unopened language-service project")?
            .views
            .get(&command.view.view_id)
            .copied()
            .context("detachment of unknown language-service view")?;
        if current != command.view {
            bail!("language-service view detachment is stale");
        }

        let removed_documents = self.take_view_documents(command.project, command.view);
        if !removed_documents.is_empty() {
            if let Err(error) = self.sync_engine_project(command.project) {
                self.documents.extend(removed_documents);
                return Err(error).context("remove detached-view documents");
            }
        }
        self.projects
            .get_mut(&command.project)
            .context("project disappeared during view detachment")?
            .views
            .remove(&command.view.view_id);
        let project = self.project_identity(command.project)?;
        Ok(vec![Event::StateAcknowledged(
            AcknowledgedState::ViewDetached {
                project,
                view: command.view,
            },
        )])
    }

    fn open_document(&mut self, command: OpenDocument) -> Result<Vec<Event>> {
        let key = command.descriptor.document.key;
        self.require_project(key.project)?;
        self.require_attached_view(key.project, command.descriptor.document.view)?;
        if self.documents.contains_key(&key) {
            bail!(
                "language-service document {} is already open",
                key.document_id
            );
        }
        let file_name = vfs_file_name(&command.descriptor)?;
        self.documents.insert(
            key,
            OpenedDocument {
                descriptor: command.descriptor,
                text: command.text,
                file_name,
            },
        );
        if let Err(error) = self.sync_engine_project(key.project) {
            self.documents.remove(&key);
            return Err(error).context("open language-service document");
        }
        let state = self.document_identity(key)?;
        Ok(vec![
            Event::StateAcknowledged(AcknowledgedState::DocumentOpened(state)),
            Event::ProjectStatus(ProjectStatusEvent {
                identity: self.project_identity(key.project)?,
                status: ProjectStatus::Ready,
            }),
        ])
    }

    fn change_document(&mut self, command: ChangeDocument) -> Result<Vec<Event>> {
        let key = command.document.key;
        self.require_attached_view(key.project, command.document.view)?;
        let document = self
            .documents
            .get_mut(&key)
            .with_context(|| format!("change of unopened document {}", key.document_id))?;
        if document.descriptor.document != command.document {
            bail!("change base version is stale for {}", key.document_id);
        }
        let previous = document.clone();
        document.text = command
            .apply_to(&document.text)
            .context("apply authoring document changes")?;
        document.descriptor.document.version = command.new_version;
        if let Err(error) = self.sync_engine_project(key.project) {
            self.documents.insert(key, previous);
            return Err(error).context("change language-service document");
        }
        let state = self.document_identity(key)?;
        Ok(vec![Event::StateAcknowledged(
            AcknowledgedState::DocumentChanged(state),
        )])
    }

    fn save_document(
        &mut self,
        command: crate::language_service::SaveDocument,
    ) -> Result<Vec<Event>> {
        let key = command.document.key;
        self.require_attached_view(key.project, command.document.view)?;
        let (changed, previous) = {
            let document = self
                .documents
                .get_mut(&key)
                .with_context(|| format!("save of unopened document {}", key.document_id))?;
            if document.descriptor.document != command.document {
                bail!("save version is stale for {}", key.document_id);
            }
            let changed = document.text != command.text;
            let previous = document.clone();
            document.text = command.text;
            document.descriptor.disk_revision = Some(command.disk_revision);
            (changed, previous)
        };
        if changed {
            if let Err(error) = self.sync_engine_project(key.project) {
                self.documents.insert(key, previous);
                return Err(error).context("save language-service document");
            }
        }
        let state = self.document_identity(key)?;
        Ok(vec![Event::StateAcknowledged(
            AcknowledgedState::DocumentSaved(state),
        )])
    }

    fn close_document(&mut self, command: CloseDocument) -> Result<Vec<Event>> {
        let key = command.document.key;
        let current = self.document_identity(key)?;
        if current.document != command.document {
            bail!("close version is stale for {}", key.document_id);
        }
        self.require_attached_view(key.project, command.document.view)?;
        let removed = self
            .documents
            .remove(&key)
            .context("document disappeared during close")?;
        if let Err(error) = self.sync_engine_project(key.project) {
            self.documents.insert(key, removed);
            return Err(error).context("close language-service document");
        }
        let project = self.project_identity(key.project)?;
        Ok(vec![Event::StateAcknowledged(
            AcknowledgedState::DocumentClosed(DocumentStateIdentity {
                document: command.document,
                graph_generation: project.graph_generation,
                service_generation: project.service_generation,
                worker_generation: project.worker_generation,
            }),
        )])
    }

    fn request_diagnostics(&mut self, identity: DocumentResultIdentity) -> Result<Vec<Event>> {
        let current = self.require_request(identity)?;
        self.ensure_engine_project(current.document.key.project)?;
        let result = self.engine.diagnostics(current.document.key.document_id)?;
        Ok(vec![Event::Diagnostics(DocumentResult {
            identity,
            analyzed_uri: self.current_uri(identity)?,
            result,
        })])
    }

    fn require_project(&self, project: ProjectScope) -> Result<()> {
        if self.projects.contains_key(&project) {
            Ok(())
        } else {
            bail!("unknown language-service project")
        }
    }

    fn require_attached_view(&self, project: ProjectScope, view: Option<ViewRef>) -> Result<()> {
        let Some(view) = view else {
            return Ok(());
        };
        let current = self
            .projects
            .get(&project)
            .context("view belongs to an unopened language-service project")?
            .views
            .get(&view.view_id)
            .copied()
            .context("document belongs to an unattached language-service view")?;
        if current == view {
            Ok(())
        } else {
            bail!("document belongs to a stale language-service view")
        }
    }

    fn take_view_documents(
        &mut self,
        project: ProjectScope,
        view: ViewRef,
    ) -> Vec<(DocumentKey, OpenedDocument)> {
        let keys = self
            .documents
            .iter()
            .filter_map(|(key, document)| {
                (key.project == project && document.descriptor.document.view == Some(view))
                    .then_some(*key)
            })
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| self.documents.remove(&key).map(|document| (key, document)))
            .collect()
    }

    fn project_files(&self, project: ProjectScope) -> Result<Vec<ProjectFile>> {
        let mut files = self
            .projects
            .get(&project)
            .context("unknown language-service project")?
            .base_sources
            .clone();
        for document in self
            .documents
            .values()
            .filter(|document| document.descriptor.document.key.project == project)
        {
            let file = ProjectFile::new(
                document.descriptor.document.key.document_id,
                &document.file_name,
                &document.descriptor.uri,
                document.descriptor.language,
                &document.text,
            )?;
            if let Some(base) = files.get(&file.document_id) {
                if base.file_name != file.file_name
                    || base.uri != file.uri
                    || base.language != file.language
                {
                    bail!("opened document identity does not match its retained base source");
                }
            }
            // An opened document is the authoritative unsaved overlay for its
            // stable document identity. Closing it reveals the retained base.
            files.insert(file.document_id, file);
        }
        validate_combined_project_files(&files)?;
        Ok(files.into_values().collect())
    }

    fn sync_engine_project(&mut self, project: ProjectScope) -> Result<()> {
        let files = self.project_files(project)?;
        let generation = self.allocate_service_generation()?;
        self.engine.replace_project(files)?;
        self.active_engine_project = Some(project);
        self.projects
            .get_mut(&project)
            .context("project disappeared during engine synchronization")?
            .service_generation = generation;
        Ok(())
    }

    fn ensure_engine_project(&mut self, project: ProjectScope) -> Result<()> {
        if self.active_engine_project == Some(project) {
            return Ok(());
        }
        let files = self.project_files(project)?;
        self.engine.replace_project(files)?;
        self.active_engine_project = Some(project);
        Ok(())
    }

    fn allocate_service_generation(&mut self) -> Result<ServiceGeneration> {
        let generation = ServiceGeneration::new(self.next_service_generation)
            .context("language-service generation exhausted")?;
        self.next_service_generation = self
            .next_service_generation
            .checked_add(1)
            .context("language-service generation overflow")?;
        Ok(generation)
    }

    fn project_identity(&self, project: ProjectScope) -> Result<ProjectStateIdentity> {
        let state = self
            .projects
            .get(&project)
            .context("unknown language-service project")?;
        Ok(ProjectStateIdentity {
            project,
            graph_generation: state.graph_generation,
            service_generation: state.service_generation,
            worker_generation: self.worker_generation,
        })
    }

    fn document_identity(&self, key: DocumentKey) -> Result<DocumentStateIdentity> {
        let document = self
            .documents
            .get(&key)
            .with_context(|| format!("unknown language-service document {}", key.document_id))?;
        let project = self.project_identity(key.project)?;
        Ok(DocumentStateIdentity {
            document: document.descriptor.document,
            graph_generation: project.graph_generation,
            service_generation: project.service_generation,
            worker_generation: project.worker_generation,
        })
    }

    fn require_request(&self, identity: DocumentResultIdentity) -> Result<DocumentStateIdentity> {
        let current = self.document_identity(identity.state.document.key)?;
        if current != identity.state {
            bail!("language-service request state is stale");
        }
        self.require_attached_view(current.document.key.project, current.document.view)?;
        Ok(current)
    }

    fn current_uri(&self, identity: DocumentResultIdentity) -> Result<Option<String>> {
        let document = self
            .documents
            .get(&identity.state.document.key)
            .context("request document disappeared")?;
        Ok(Some(document.descriptor.uri.clone()))
    }
}

fn validate_combined_project_files(files: &HashMap<DocumentId, ProjectFile>) -> Result<()> {
    if files.len() > MAX_PROJECT_SOURCE_FILES {
        bail!(
            "project has {} combined saved and open sources; maximum is \
             {MAX_PROJECT_SOURCE_FILES}",
            files.len()
        );
    }
    let source_text_bytes = files.values().try_fold(0_usize, |total, file| {
        total
            .checked_add(file.text.len())
            .context("combined project source text size overflow")
    })?;
    if source_text_bytes > MAX_PROJECT_SOURCE_TEXT_BYTES {
        bail!(
            "project has {source_text_bytes} combined saved and open source bytes; maximum is \
             {MAX_PROJECT_SOURCE_TEXT_BYTES}"
        );
    }
    Ok(())
}

fn failure_scope(state: &WorkerState, command: &Command) -> FailureScope {
    match command {
        Command::RequestDiagnostics(command) => FailureScope::Document(command.identity),
        Command::RequestCompletion(command)
        | Command::RequestHover(command)
        | Command::RequestSignatureHelp(command)
        | Command::RequestDefinition(command) => FailureScope::Document(command.identity),
        Command::RequestFormatting(command) => FailureScope::Document(command.identity),
        Command::OpenProject(command) => state.project_identity(command.project).map_or(
            FailureScope::Worker {
                worker_generation: state.worker_generation,
            },
            FailureScope::Project,
        ),
        Command::RefreshProject(command) => state.project_identity(command.project).map_or(
            FailureScope::Worker {
                worker_generation: state.worker_generation,
            },
            FailureScope::Project,
        ),
        Command::CloseProject(command) => state.project_identity(command.project).map_or(
            FailureScope::Worker {
                worker_generation: state.worker_generation,
            },
            FailureScope::Project,
        ),
        _ => FailureScope::Worker {
            worker_generation: state.worker_generation,
        },
    }
}

fn project_sources(
    project: ProjectScope,
    sources: Vec<ProjectSource>,
) -> Result<HashMap<DocumentId, ProjectFile>> {
    if sources.len() > MAX_PROJECT_SOURCE_FILES {
        bail!(
            "project snapshot has {} sources; maximum is {MAX_PROJECT_SOURCE_FILES}",
            sources.len()
        );
    }
    let mut files = HashMap::with_capacity(sources.len());
    let mut ids_by_file_name = HashMap::with_capacity(sources.len());
    let mut source_text_bytes = 0_usize;
    for source in sources {
        if source.uri.is_empty()
            || source.uri.len() > MAX_URI_BYTES
            || source.uri.chars().any(char::is_control)
        {
            bail!("invalid project-source URI");
        }
        source_text_bytes = source_text_bytes
            .checked_add(source.text.len())
            .context("project source text size overflow")?;
        if source_text_bytes > MAX_PROJECT_SOURCE_TEXT_BYTES {
            bail!(
                "project snapshot has {source_text_bytes} decoded source bytes; maximum is \
                 {MAX_PROJECT_SOURCE_TEXT_BYTES}"
            );
        }
        let file_name =
            vfs_file_name_for(project, source.document_id, &source.uri, source.language)?;
        let file = ProjectFile::new(
            source.document_id,
            &file_name,
            source.uri,
            source.language,
            source.text,
        )?;
        if files.insert(file.document_id, file).is_some() {
            bail!("duplicate project-source document identity");
        }
        if let Some(previous_id) = ids_by_file_name.insert(file_name.clone(), source.document_id) {
            bail!(
                "duplicate project-source VFS name {file_name} for {previous_id} and {}",
                source.document_id
            );
        }
    }
    Ok(files)
}

fn vfs_file_name(descriptor: &DocumentDescriptor) -> Result<String> {
    vfs_file_name_for(
        descriptor.document.key.project,
        descriptor.document.key.document_id,
        &descriptor.uri,
        descriptor.language,
    )
}

fn vfs_file_name_for(
    project: ProjectScope,
    document_id: DocumentId,
    uri: &str,
    language: Language,
) -> Result<String> {
    let raw_path = uri
        .split_once("://")
        .map_or(uri, |(_, path)| path)
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_start_matches('/');
    let fallback = document_id.to_string();
    let replaced = raw_path.replace('\\', "/");
    let mut parts = Vec::new();
    for part in replaced.split('/') {
        match part {
            "" | "." => {}
            ".." => bail!("language-service URI path escapes its project"),
            value => parts.push(value),
        }
    }
    let mut path = if parts.is_empty() {
        fallback
    } else {
        parts.join("/")
    };
    if std::path::Path::new(&path).extension().is_none() {
        path.push_str(match language {
            Language::JavaScript => ".js",
            Language::TypeScript => ".ts",
            Language::JavaScriptReact => ".jsx",
            Language::TypeScriptReact => ".tsx",
            Language::Json => ".json",
            Language::PlainText => ".txt",
        });
    }
    Ok(format!(
        "/projects/{}/{}/{}",
        project.client_id.get(),
        project.project_id.get(),
        path
    ))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::language_service::{
        AnalysisContextId, AutomationKind, ClientId, DiagnosticCode, DiskRevision, DocumentId,
        DocumentKind, DocumentRef, DocumentVersion, Language, OpenDocument, ProjectId, RequestId,
        ViewGeneration,
    };

    use super::*;

    fn number<T>(value: u64) -> T
    where
        T: TryFrom<u64>,
        T::Error: fmt::Debug,
    {
        T::try_from(value).expect("valid test wire number")
    }

    fn project(client: u64, project: u64) -> ProjectScope {
        ProjectScope {
            client_id: number::<ClientId>(client),
            project_id: number::<ProjectId>(project),
        }
    }

    fn descriptor_for(
        project: ProjectScope,
        document_id_byte: u8,
        view: Option<ViewRef>,
        uri: &str,
    ) -> DocumentDescriptor {
        DocumentDescriptor {
            document: DocumentRef {
                key: DocumentKey {
                    project,
                    document_id: DocumentId::try_from([document_id_byte; 16])
                        .expect("non-nil document ID"),
                },
                view,
                version: number::<DocumentVersion>(1),
            },
            uri: uri.to_owned(),
            language: Language::TypeScript,
            kind: DocumentKind::InlineAutomation {
                automation_kind: AutomationKind::Alias,
            },
            analysis_context: number::<AnalysisContextId>(1),
            disk_revision: Some(number::<DiskRevision>(1)),
        }
    }

    fn descriptor() -> DocumentDescriptor {
        descriptor_for(project(1, 2), 3, None, "smudgy-inline:///aliases/test.ts")
    }

    fn project_source(document_id_byte: u8, uri: &str, text: &str) -> ProjectSource {
        ProjectSource {
            document_id: DocumentId::try_from([document_id_byte; 16])
                .expect("non-nil project-source document ID"),
            uri: uri.to_owned(),
            language: Language::TypeScript,
            kind: DocumentKind::StandaloneModule,
            text: text.to_owned(),
        }
    }

    fn wait_for(
        host: &mut LanguageServiceHost,
        predicate: impl Fn(&Event) -> bool,
    ) -> EventEnvelope {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            for event in host.drain_events() {
                if predicate(&event.event) {
                    return event;
                }
            }
            assert!(
                Instant::now() < deadline,
                "language-service event timed out"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn worker_opens_document_and_returns_fenced_diagnostics() {
        let mut host = LanguageServiceHost::spawn();
        let descriptor = descriptor();
        host.client()
            .send(Command::OpenProject(OpenProject {
                project: descriptor.document.key.project,
            }))
            .expect("queue open project");
        wait_for(&mut host, |event| {
            matches!(
                event,
                Event::StateAcknowledged(AcknowledgedState::ProjectOpened(_))
            )
        });
        host.client()
            .send(Command::OpenDocument(OpenDocument {
                descriptor: descriptor.clone(),
                text: "const value: number = \"wrong\";\n".to_owned(),
            }))
            .expect("queue open document");
        let opened = wait_for(&mut host, |event| {
            matches!(
                event,
                Event::StateAcknowledged(AcknowledgedState::DocumentOpened(_))
            )
        });
        let Event::StateAcknowledged(AcknowledgedState::DocumentOpened(state)) = opened.event
        else {
            unreachable!();
        };
        let request_id = number::<RequestId>(8);
        host.client()
            .send(Command::RequestDiagnostics(
                crate::language_service::DocumentRequest {
                    identity: DocumentResultIdentity { state, request_id },
                },
            ))
            .expect("queue diagnostics");
        let result = wait_for(&mut host, |event| matches!(event, Event::Diagnostics(_)));
        let Event::Diagnostics(result) = result.event else {
            unreachable!();
        };
        assert_eq!(result.identity.request_id, request_id);
        assert_eq!(result.identity.state, state);
        assert!(
            result
                .result
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2322)))
        );
        host.shutdown().expect("worker must shut down cleanly");
    }

    #[test]
    fn worker_returns_signature_help_with_exact_identity_and_uri() {
        let mut host = LanguageServiceHost::spawn();
        let descriptor = descriptor();
        host.client()
            .send(Command::OpenProject(OpenProject {
                project: descriptor.document.key.project,
            }))
            .expect("queue open project");
        wait_for(&mut host, |event| {
            matches!(
                event,
                Event::StateAcknowledged(AcknowledgedState::ProjectOpened(_))
            )
        });
        host.client()
            .send(Command::OpenDocument(OpenDocument {
                descriptor: descriptor.clone(),
                text: "function send(message: string, urgent?: boolean): void {}\nsend(".to_owned(),
            }))
            .expect("queue open document");
        let opened = wait_for(&mut host, |event| {
            matches!(
                event,
                Event::StateAcknowledged(AcknowledgedState::DocumentOpened(_))
            )
        });
        let Event::StateAcknowledged(AcknowledgedState::DocumentOpened(state)) = opened.event
        else {
            unreachable!();
        };
        let identity = DocumentResultIdentity {
            state,
            request_id: number::<RequestId>(18),
        };
        host.client()
            .send(Command::RequestSignatureHelp(
                crate::language_service::PositionRequest {
                    identity,
                    position: crate::language_service::Utf16Position {
                        line: 1,
                        character: 5,
                    },
                },
            ))
            .expect("queue signature help");
        let event = wait_for(&mut host, |event| matches!(event, Event::SignatureHelp(_)));
        let Event::SignatureHelp(result) = event.event else {
            unreachable!();
        };
        assert_eq!(result.identity, identity);
        assert_eq!(
            result.analyzed_uri.as_deref(),
            Some(descriptor.uri.as_str())
        );
        let help = result.result.expect("signature help at incomplete call");
        assert_eq!(help.prefix, "send(");
        assert_eq!(help.active_parameter, Some(0));
        assert_eq!(help.parameters.len(), 2);
        assert!(help.parameters[1].is_optional);
        host.shutdown().expect("worker must shut down cleanly");
    }

    #[test]
    fn worker_rejects_signature_help_for_a_stale_document_version() {
        let engine = EmbeddedLanguageService::new().expect("boot language service");
        let mut state = WorkerState::new(engine, allocate_worker_generation());
        let descriptor = descriptor();
        state
            .open_project(OpenProject {
                project: descriptor.document.key.project,
            })
            .expect("open project");
        state
            .open_document(OpenDocument {
                descriptor: descriptor.clone(),
                text: "function send(message: string): void {}\nsend(".to_owned(),
            })
            .expect("open document");
        let stale_identity = DocumentResultIdentity {
            state: state
                .document_identity(descriptor.document.key)
                .expect("current document identity"),
            request_id: number::<RequestId>(19),
        };
        state
            .change_document(ChangeDocument {
                document: descriptor.document,
                new_version: number::<DocumentVersion>(2),
                changes: crate::language_service::DocumentChanges {
                    changes: vec![crate::language_service::TextChange {
                        range: None,
                        text: "function send(message: string): void {}\nsend()".to_owned(),
                    }],
                },
            })
            .expect("advance document version");

        assert!(
            state
                .handle(Command::RequestSignatureHelp(
                    crate::language_service::PositionRequest {
                        identity: stale_identity,
                        position: crate::language_service::Utf16Position {
                            line: 1,
                            character: 5,
                        },
                    },
                ))
                .is_err(),
            "the complete request identity must fence stale signature help"
        );
    }

    #[test]
    fn worker_seeds_rooted_immutable_declaration_libraries() {
        let mut host =
            LanguageServiceHost::try_spawn_with_libraries(vec![LanguageServiceLibrary {
                file_name: "/types/smudgy-test.d.ts".to_owned(),
                text: "declare module \"smudgy:test\" { export function value(): string; }\n"
                    .into(),
                is_root: true,
            }])
            .expect("spawn language service with immutable declarations");
        let descriptor = descriptor();
        host.client()
            .send(Command::OpenProject(OpenProject {
                project: descriptor.document.key.project,
            }))
            .expect("queue open project");
        wait_for(&mut host, |event| {
            matches!(
                event,
                Event::StateAcknowledged(AcknowledgedState::ProjectOpened(_))
            )
        });
        host.client()
            .send(Command::OpenDocument(OpenDocument {
                descriptor: descriptor.clone(),
                text: "import { value } from \"smudgy:test\";\nconst wrong: number = value();\n"
                    .to_owned(),
            }))
            .expect("queue open document");
        let opened = wait_for(&mut host, |event| {
            matches!(
                event,
                Event::StateAcknowledged(AcknowledgedState::DocumentOpened(_))
            )
        });
        let Event::StateAcknowledged(AcknowledgedState::DocumentOpened(state)) = opened.event
        else {
            unreachable!();
        };
        host.client()
            .send(Command::RequestDiagnostics(
                crate::language_service::DocumentRequest {
                    identity: DocumentResultIdentity {
                        state,
                        request_id: number::<RequestId>(9),
                    },
                },
            ))
            .expect("queue diagnostics");
        let result = wait_for(&mut host, |event| matches!(event, Event::Diagnostics(_)));
        let Event::Diagnostics(result) = result.event else {
            unreachable!();
        };
        assert!(
            result
                .result
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2322))),
            "the seeded declaration must type value() as string"
        );
        assert!(
            !result
                .result
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2307))),
            "the rooted declaration must resolve smudgy:test"
        );
        host.shutdown().expect("worker must shut down cleanly");
    }

    #[test]
    fn worker_rejects_stale_lifecycle_and_releases_replaced_view_documents() {
        let engine = EmbeddedLanguageService::new().expect("boot language service");
        let mut state = WorkerState::new(engine, allocate_worker_generation());
        let project = project(5, 6);
        let first_view = ViewRef {
            view_id: number::<ViewId>(7),
            generation: number::<ViewGeneration>(1),
        };
        let descriptor = descriptor_for(
            project,
            8,
            Some(first_view),
            "smudgy-inline:///aliases/view.ts",
        );

        assert!(
            state
                .open_document(OpenDocument {
                    descriptor: descriptor.clone(),
                    text: "const beforeProject = true;".to_owned(),
                })
                .is_err()
        );
        state
            .open_project(OpenProject { project })
            .expect("open project");
        state
            .attach_view(AttachView {
                project,
                view: first_view,
            })
            .expect("attach first view");
        let open = OpenDocument {
            descriptor: descriptor.clone(),
            text: "const currentView = true;".to_owned(),
        };
        state.open_document(open.clone()).expect("open document");
        assert!(state.open_document(open).is_err());
        assert!(
            state
                .refresh_project(RefreshProject {
                    project,
                    graph_generation: number::<GraphGeneration>(1),
                    sources: Vec::new(),
                })
                .is_err()
        );

        let second_view = ViewRef {
            view_id: first_view.view_id,
            generation: number::<ViewGeneration>(2),
        };
        state
            .attach_view(AttachView {
                project,
                view: second_view,
            })
            .expect("replace view generation");
        assert!(!state.documents.contains_key(&descriptor.document.key));
        assert!(
            state
                .attach_view(AttachView {
                    project,
                    view: first_view,
                })
                .is_err()
        );
    }

    #[test]
    fn worker_keeps_project_programs_isolated() {
        let engine = EmbeddedLanguageService::new().expect("boot language service");
        let mut state = WorkerState::new(engine, allocate_worker_generation());
        let first_project = project(9, 10);
        let second_project = project(9, 11);
        state
            .open_project(OpenProject {
                project: first_project,
            })
            .expect("open first project");
        state
            .open_project(OpenProject {
                project: second_project,
            })
            .expect("open second project");
        state
            .open_document(OpenDocument {
                descriptor: descriptor_for(first_project, 12, None, "smudgy-module:///private.ts"),
                text: "declare const privateMarker: \"PROJECT_ONE\";".to_owned(),
            })
            .expect("open first-project declaration");
        let second_descriptor =
            descriptor_for(second_project, 13, None, "smudgy-module:///consumer.ts");
        state
            .open_document(OpenDocument {
                descriptor: second_descriptor.clone(),
                text: "privateMarker;".to_owned(),
            })
            .expect("open second-project consumer");
        let identity = DocumentResultIdentity {
            state: state
                .document_identity(second_descriptor.document.key)
                .expect("second document identity"),
            request_id: number::<RequestId>(1),
        };
        let events = state
            .request_diagnostics(identity)
            .expect("isolated-project diagnostics");
        let Event::Diagnostics(result) = &events[0] else {
            panic!("expected diagnostics event");
        };
        assert!(
            result
                .result
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2304)))
        );
    }

    #[test]
    fn opened_overlay_shadows_base_and_close_restores_it() {
        let engine = EmbeddedLanguageService::new().expect("boot language service");
        let mut state = WorkerState::new(engine, allocate_worker_generation());
        let project = project(20, 21);
        let uri = "smudgy-project:///modules/value.ts";
        let document_id = DocumentId::try_from([22; 16]).expect("non-nil base document identity");
        state
            .open_project(OpenProject { project })
            .expect("open project");
        state
            .refresh_project(RefreshProject {
                project,
                graph_generation: number::<GraphGeneration>(2),
                sources: vec![project_source(22, uri, "export const value: number = 1;\n")],
            })
            .expect("install base source");

        let base_diagnostics = state
            .engine
            .diagnostics(document_id)
            .expect("base diagnostics");
        assert!(
            !base_diagnostics
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2322)))
        );

        let mut descriptor = descriptor_for(project, 22, None, uri);
        descriptor.kind = DocumentKind::StandaloneModule;
        state
            .open_document(OpenDocument {
                descriptor: descriptor.clone(),
                text: "export const value: number = \"overlay\";\n".to_owned(),
            })
            .expect("open authoritative overlay");
        let overlay_diagnostics = state
            .engine
            .diagnostics(document_id)
            .expect("overlay diagnostics");
        assert!(
            overlay_diagnostics
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2322))),
            "the opened document must shadow the source with the same stable identity"
        );

        state
            .close_document(CloseDocument {
                document: descriptor.document,
            })
            .expect("close overlay");
        assert!(
            state
                .projects
                .get(&project)
                .expect("open project")
                .base_sources
                .contains_key(&document_id),
            "closing an overlay must retain its base source"
        );
        let restored_diagnostics = state
            .engine
            .diagnostics(document_id)
            .expect("restored base diagnostics");
        assert!(
            !restored_diagnostics
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2322))),
            "closing the overlay must reveal the retained base text"
        );

        state
            .close_project(CloseProject { project })
            .expect("close project");
        state
            .open_project(OpenProject { project })
            .expect("reopen project");
        assert!(
            state
                .projects
                .get(&project)
                .expect("reopened project")
                .base_sources
                .is_empty(),
            "closing a project must release its base snapshot"
        );
    }

    #[test]
    fn opened_overlay_must_preserve_its_base_source_identity() {
        let engine = EmbeddedLanguageService::new().expect("boot language service");
        let mut state = WorkerState::new(engine, allocate_worker_generation());
        let project = project(40, 41);
        let base_uri = "smudgy-project:///modules/value.ts";
        let document_id = DocumentId::try_from([42; 16]).expect("non-nil document identity");
        state
            .open_project(OpenProject { project })
            .expect("open project");
        state
            .refresh_project(RefreshProject {
                project,
                graph_generation: number::<GraphGeneration>(2),
                sources: vec![project_source(
                    42,
                    base_uri,
                    "export const retained = true;\n",
                )],
            })
            .expect("install base source");

        let mut mismatched =
            descriptor_for(project, 42, None, "smudgy-project:///modules/different.ts");
        mismatched.kind = DocumentKind::StandaloneModule;
        assert!(
            state
                .open_document(OpenDocument {
                    descriptor: mismatched,
                    text: "export const replacement = true;\n".to_owned(),
                })
                .is_err(),
            "a stable document ID may not silently replace a different physical source"
        );
        assert!(
            !state.documents.contains_key(&DocumentKey {
                project,
                document_id,
            }),
            "a rejected overlay must be rolled back"
        );
        assert_eq!(
            state
                .projects
                .get(&project)
                .expect("project remains open")
                .base_sources
                .get(&document_id)
                .expect("base source remains retained")
                .uri,
            base_uri
        );
        state
            .engine
            .diagnostics(document_id)
            .expect("the retained base remains installed");
    }

    #[test]
    fn base_sources_provide_cross_file_diagnostics_and_definitions() {
        let engine = EmbeddedLanguageService::new().expect("boot language service");
        let mut state = WorkerState::new(engine, allocate_worker_generation());
        let project = project(23, 24);
        let library_uri = "smudgy-project:///modules/library.ts";
        let consumer_uri = "smudgy-project:///modules/consumer.ts";
        let consumer_text = "import { answer } from \"./library.ts\";\n\
const wrong: string = answer;\n\
answer;\n";
        state
            .open_project(OpenProject { project })
            .expect("open project");
        state
            .refresh_project(RefreshProject {
                project,
                graph_generation: number::<GraphGeneration>(2),
                sources: vec![
                    project_source(25, library_uri, "export const answer: number = 42;\n"),
                    project_source(26, consumer_uri, consumer_text),
                ],
            })
            .expect("install project sources");

        let mut consumer = descriptor_for(project, 26, None, consumer_uri);
        consumer.kind = DocumentKind::StandaloneModule;
        state
            .open_document(OpenDocument {
                descriptor: consumer.clone(),
                text: consumer_text.to_owned(),
            })
            .expect("open consumer overlay");
        let state_identity = state
            .document_identity(consumer.document.key)
            .expect("consumer identity");
        let diagnostics = state
            .request_diagnostics(DocumentResultIdentity {
                state: state_identity,
                request_id: number::<RequestId>(27),
            })
            .expect("cross-file diagnostics");
        let Event::Diagnostics(diagnostics) = &diagnostics[0] else {
            panic!("expected diagnostics event");
        };
        assert!(
            diagnostics
                .result
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2322))),
            "the imported number must be checked against the consumer's string annotation"
        );
        assert!(
            !diagnostics
                .result
                .items
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2307))),
            "the sibling module must resolve from the retained project snapshot"
        );

        let definition = state
            .handle(Command::RequestDefinition(
                crate::language_service::PositionRequest {
                    identity: DocumentResultIdentity {
                        state: state_identity,
                        request_id: number::<RequestId>(28),
                    },
                    position: crate::language_service::Utf16Position {
                        line: 2,
                        character: 0,
                    },
                },
            ))
            .expect("cross-file definition");
        let Event::Definition(definition) = &definition[0] else {
            panic!("expected definition event");
        };
        assert!(
            definition.result.targets.iter().any(|target| {
                target.document_id
                    == DocumentId::try_from([25; 16]).expect("non-nil library identity")
                    && target.analyzed_uri.as_deref() == Some(library_uri)
            }),
            "definition must route to the imported base-source identity"
        );
    }

    #[test]
    fn stale_and_duplicate_vfs_refreshes_leave_the_snapshot_atomic() {
        let engine = EmbeddedLanguageService::new().expect("boot language service");
        let mut state = WorkerState::new(engine, allocate_worker_generation());
        let project = project(29, 30);
        let original_id =
            DocumentId::try_from([31; 16]).expect("non-nil original document identity");
        let original_text = "export const retained = true;\n";
        state
            .open_project(OpenProject { project })
            .expect("open project");
        state
            .refresh_project(RefreshProject {
                project,
                graph_generation: number::<GraphGeneration>(2),
                sources: vec![project_source(
                    31,
                    "smudgy-project:///modules/original.ts",
                    original_text,
                )],
            })
            .expect("install original snapshot");

        assert!(
            state
                .refresh_project(RefreshProject {
                    project,
                    graph_generation: number::<GraphGeneration>(2),
                    sources: vec![project_source(
                        31,
                        "smudgy-project:///modules/original.ts",
                        "export const stale = true;\n",
                    )],
                })
                .is_err(),
            "equal graph generations must be rejected"
        );

        let overlay = descriptor_for(project, 32, None, "smudgy-project:///modules/collision.ts");
        state
            .open_document(OpenDocument {
                descriptor: overlay,
                text: "export const overlay = true;\n".to_owned(),
            })
            .expect("open overlay authority");
        assert!(
            state
                .refresh_project(RefreshProject {
                    project,
                    graph_generation: number::<GraphGeneration>(3),
                    sources: vec![project_source(
                        33,
                        "other-scheme:///modules/collision.ts",
                        "export const collision = true;\n",
                    )],
                })
                .is_err(),
            "a base source may not claim an opened overlay's VFS name under another identity"
        );

        let project_state = state.projects.get(&project).expect("project remains open");
        assert_eq!(
            project_state.graph_generation,
            number::<GraphGeneration>(2),
            "a failed replacement must restore the graph generation"
        );
        assert_eq!(project_state.base_sources.len(), 1);
        assert_eq!(
            project_state
                .base_sources
                .get(&original_id)
                .expect("original base source retained")
                .text,
            original_text,
            "a failed replacement must restore the complete base snapshot"
        );
        state
            .ensure_engine_project(project)
            .expect("the previous engine project remains usable");
        state
            .engine
            .diagnostics(original_id)
            .expect("the original base file remains installed");
    }

    #[test]
    fn worker_rejects_vfs_escape_and_rolls_back_duplicate_file_authority() {
        let project = project(14, 15);
        let escaping = descriptor_for(
            project,
            16,
            None,
            "smudgy-module:///../../other-project/secret.ts",
        );
        assert!(vfs_file_name(&escaping).is_err());

        let engine = EmbeddedLanguageService::new().expect("boot language service");
        let mut state = WorkerState::new(engine, allocate_worker_generation());
        state
            .open_project(OpenProject { project })
            .expect("open project");
        let first = descriptor_for(project, 17, None, "smudgy-module:///same.ts");
        state
            .open_document(OpenDocument {
                descriptor: first.clone(),
                text: "export const retained = true;".to_owned(),
            })
            .expect("open first authority");
        let duplicate = descriptor_for(project, 18, None, "smudgy-module:///same.ts");
        assert!(
            state
                .open_document(OpenDocument {
                    descriptor: duplicate.clone(),
                    text: "export const replacement = true;".to_owned(),
                })
                .is_err()
        );
        assert!(state.documents.contains_key(&first.document.key));
        assert!(!state.documents.contains_key(&duplicate.document.key));
        state
            .ensure_engine_project(project)
            .expect("rolled-back engine remains usable");
    }

    #[test]
    fn host_shutdown_fences_cloned_clients_and_worker_generations_do_not_repeat() {
        let first_generation = allocate_worker_generation();
        let second_generation = allocate_worker_generation();
        assert!(second_generation > first_generation);

        let host = LanguageServiceHost::spawn();
        let client = host.client();
        host.shutdown().expect("worker must shut down cleanly");
        assert_eq!(
            client.send(Command::OpenProject(OpenProject {
                project: project(19, 20),
            })),
            Err(LanguageServiceSendError::Stopped)
        );
    }

    #[test]
    fn combined_saved_and_open_project_snapshot_enforces_file_and_byte_caps() {
        let mut files = HashMap::new();
        for index in 1..=MAX_PROJECT_SOURCE_FILES + 1 {
            let mut bytes = [0_u8; 16];
            bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
            let document_id = DocumentId::try_from(bytes).expect("non-nil generated ID");
            files.insert(
                document_id,
                ProjectFile {
                    document_id,
                    file_name: format!("/project/{index}.ts"),
                    uri: format!("smudgy-test:///{index}.ts"),
                    language: Language::TypeScript,
                    text: String::new(),
                },
            );
        }
        assert!(validate_combined_project_files(&files).is_err());

        files.clear();
        let document_id = DocumentId::try_from([1_u8; 16]).expect("non-nil test ID");
        files.insert(
            document_id,
            ProjectFile {
                document_id,
                file_name: "/project/large.ts".to_owned(),
                uri: "smudgy-test:///large.ts".to_owned(),
                language: Language::TypeScript,
                text: "x".repeat(MAX_PROJECT_SOURCE_TEXT_BYTES + 1),
            },
        );
        assert!(validate_combined_project_files(&files).is_err());
    }

    #[test]
    fn application_exit_join_removes_language_service_temporary_directory() {
        const HELPER_ENV: &str = "SMUDGY_LANGUAGE_SERVICE_EXIT_JOIN_HELPER";
        const TEST_NAME: &str = concat!(
            "language_service_worker::tests::",
            "application_exit_join_removes_language_service_temporary_directory"
        );

        if std::env::var_os(HELPER_ENV).is_some() {
            let mut host = LanguageServiceHost::spawn();
            host.client()
                .send(Command::OpenProject(OpenProject {
                    project: project(21, 22),
                }))
                .expect("queue helper project open");
            wait_for(&mut host, |event| {
                matches!(
                    event,
                    Event::StateAcknowledged(AcknowledgedState::ProjectOpened(_))
                )
            });
            drop(host);
            join_language_service_reapers();

            let prefix = format!("smudgy-language-service-{}-", std::process::id());
            let leftovers = std::fs::read_dir(std::env::temp_dir())
                .expect("read temporary directory")
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.starts_with(&prefix))
                .collect::<Vec<_>>();
            assert!(
                leftovers.is_empty(),
                "joined helper left language-service data directories: {leftovers:?}"
            );
            return;
        }

        let mut child = std::process::Command::new(
            std::env::current_exe().expect("locate current test executable"),
        )
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(HELPER_ENV, "1")
        .spawn()
        .expect("spawn isolated language-service exit helper");
        let child_id = child.id();
        let status = child.wait().expect("wait for exit helper");
        assert!(status.success(), "exit helper failed with {status}");

        let prefix = format!("smudgy-language-service-{child_id}-");
        let leftovers = std::fs::read_dir(std::env::temp_dir())
            .expect("read temporary directory")
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with(&prefix))
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "process exit left language-service data directories: {leftovers:?}"
        );
    }
}
