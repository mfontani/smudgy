//! Host-owned automation editor controller.
//!
//! The controller deliberately depends on abstract editor and service channels, keeping
//! persistence and language-service state outside the widget. Writable JavaScript and
//! TypeScript documents use the upstream `iced-code-editor` adapter; read-only surfaces
//! remain on Smudgy's existing widgets and do not enter this lifecycle.

mod iced_surface;

#[allow(unused_imports)]
pub(super) use iced_surface::{IcedCodeEditorSurface, IcedEditorMessage};

use std::io::Read;
use std::time::{Duration, Instant};

use smudgy_script::language_service::{
    AcknowledgedState, AnalysisContextId, AutomationKind, CancelRequest, ChangeDocument, ClientId,
    CloseDocument, Command, CommandSequence, CompletionResult, DefinitionResult, DefinitionTarget,
    Diagnostic, DiagnosticsResult, DiskRevision, DocumentChanges, DocumentDescriptor, DocumentId,
    DocumentKey, DocumentKind, DocumentRef, DocumentRequest, DocumentResult,
    DocumentResultIdentity, DocumentStateIdentity, DocumentVersion, Event, EventEnvelope,
    FailureScope, FormattingOptions, FormattingRequest, GraphGeneration, HoverResult, Language,
    LanguageServiceLibrary, MAX_DOCUMENT_BYTES, MAX_PROJECT_SOURCE_FILES,
    MAX_PROJECT_SOURCE_TEXT_BYTES, OpenDocument, OpenProject, PositionRequest, ProjectId,
    ProjectScope, ProjectSource, ProjectStateIdentity, ProjectStatus, RefreshProject, RequestId,
    SaveDocument, TextEdit, Utf16Position, Utf16Range, Validate, WorkerGeneration,
    validate_document_text,
};
use smudgy_script::language_service_worker::{
    LanguageServiceClient, LanguageServiceHost, LanguageServiceSendError,
};

/// Result of routing a worker event into the current editor state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EventDisposition {
    Applied,
    Stale,
    Invalid,
}

/// Language-intelligence availability never controls whether the text surface is usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServiceStatus {
    Starting,
    Ready,
    Unavailable,
}

/// An editor-surface update and the exact ordered text changes it produced.
pub(super) struct SurfaceUpdate<Effect> {
    pub effect: Effect,
    pub changes: Option<DocumentChanges>,
    /// Latest completion position requested by the editor, expressed in the
    /// editor's scalar-column coordinates.
    pub completion: Option<CompletionIntent>,
    /// Pointer-derived hover intent. Passive editor messages leave it unchanged;
    /// leaving text or losing canvas focus clears the current hover lifecycle.
    pub hover: HoverUpdate,
    /// Whether text, caret, selection, or editor navigation changed the semantic
    /// context that transient intelligence was requested for.
    pub semantic_context_changed: bool,
    /// A user-requested definition position in the editor's scalar-column coordinates.
    pub definition: Option<ScalarPosition>,
    /// A user-requested whole-document format using the editor's current indentation style.
    pub formatting: Option<FormattingOptions>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScalarPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct SurfacePoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct OverlayMetrics {
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub viewport_scroll: f32,
    pub line_height: f32,
    pub char_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CompletionIntent {
    pub position: ScalarPosition,
    pub anchor: SurfacePoint,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct HoverIntent {
    pub position: ScalarPosition,
    pub anchor: SurfacePoint,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) enum HoverUpdate {
    #[default]
    Unchanged,
    Clear,
    At(HoverIntent),
}

const HOVER_DEBOUNCE: Duration = Duration::from_millis(300);

/// The minimum host seam every real automation editor surface must implement.
///
/// `changes` returned from [`Self::update`] are relative to the current document version.
/// A real surface must produce them from its authoritative buffer without keeping a second
/// mutable text copy in this controller.
pub(super) trait EditorSurface {
    type Message;
    type Effect;

    fn content(&self) -> String;
    fn update(&mut self, message: &Self::Message) -> SurfaceUpdate<Self::Effect>;
    fn apply_completion(
        &mut self,
        item: &smudgy_script::language_service::CompletionItem,
    ) -> SurfaceUpdate<Self::Effect>;
    /// Applies one simultaneous, already-fenced edit batch. `Err` means the
    /// surface could not represent the ranges; `Ok(None)` means a no-op.
    fn apply_text_edits(&mut self, edits: &[TextEdit]) -> Result<Option<DocumentChanges>, ()>;
    fn goto_position(&mut self, position: ScalarPosition) -> Self::Effect;
    fn reset(&mut self, text: &str, language: Language) -> Self::Effect;
    fn is_modified(&self) -> bool;
    fn mark_saved(&mut self);
    fn request_focus(&self);
    fn lose_focus(&mut self);
    fn is_dialog_open(&self) -> bool;
}

/// Narrow parent-side channel; framing, sequencing, and process ownership stay outside the
/// editor component.
pub(super) trait LanguageServiceChannel {
    type Error;

    fn send(&mut self, command: Command) -> Result<(), Self::Error>;
}

impl LanguageServiceChannel for LanguageServiceClient {
    type Error = LanguageServiceSendError;

    fn send(&mut self, command: Command) -> Result<(), Self::Error> {
        LanguageServiceClient::send(self, command).map(|_| ())
    }
}

/// Semantic kind of the single writable code document mounted in this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodeDocument {
    Alias,
    Trigger,
    Hotkey,
    StandaloneModule,
    OwnedPackage,
}

/// Which saved-source graph may participate beneath the mounted editor overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LanguageProjectContext {
    Inline,
    Modules,
    OwnedPackage(String),
}

/// Stable window-lifetime identity for one saved source or scoped generated root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum LanguageSourceKey {
    Module(String),
    OwnedPackage { package: String, subpath: String },
    InlineBridge,
}

/// One accepted cross-file definition jump. The origin state and mount fence the
/// queued navigation message across project refreshes, edits, and editor rebinds.
#[derive(Debug, Clone)]
pub struct DefinitionNavigation {
    origin: DocumentStateIdentity,
    origin_mount_generation: u64,
    target: DefinitionTarget,
}

/// The newest project refresh whose acknowledgement still owns the context transition.
///
/// The worker installs refreshes atomically, so the UI must not call a context current merely
/// because its command entered the channel. The command sequence disambiguates a failure (whose
/// failure scope describes the still-current graph), while the graph generation fences success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingLanguageProjectRefresh {
    context: LanguageProjectContext,
    graph_generation: GraphGeneration,
    command_sequence: CommandSequence,
    retries_remaining: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectRefreshRetry {
    context: LanguageProjectContext,
    retries_remaining: u8,
}

struct PendingProjectSource {
    key: LanguageSourceKey,
    uri: String,
    language: Language,
    kind: DocumentKind,
    text: String,
}

#[derive(Debug, Clone, Copy)]
enum RequestKind {
    Diagnostics,
    Completion,
    Hover,
    Definition,
    Formatting,
}

#[derive(Debug, Default)]
struct OutstandingRequests {
    diagnostics: Option<RequestId>,
    completion: Option<RequestId>,
    hover: Option<RequestId>,
    definition: Option<RequestId>,
    formatting: Option<RequestId>,
}

impl OutstandingRequests {
    fn get(&self, kind: RequestKind) -> Option<RequestId> {
        match kind {
            RequestKind::Diagnostics => self.diagnostics,
            RequestKind::Completion => self.completion,
            RequestKind::Hover => self.hover,
            RequestKind::Definition => self.definition,
            RequestKind::Formatting => self.formatting,
        }
    }

    fn set(&mut self, kind: RequestKind, request_id: Option<RequestId>) {
        *match kind {
            RequestKind::Diagnostics => &mut self.diagnostics,
            RequestKind::Completion => &mut self.completion,
            RequestKind::Hover => &mut self.hover,
            RequestKind::Definition => &mut self.definition,
            RequestKind::Formatting => &mut self.formatting,
        } = request_id;
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn take_matching(&mut self, request_id: RequestId) -> bool {
        for kind in [
            RequestKind::Diagnostics,
            RequestKind::Completion,
            RequestKind::Hover,
            RequestKind::Definition,
            RequestKind::Formatting,
        ] {
            if self.get(kind) == Some(request_id) {
                self.set(kind, None);
                return true;
            }
        }
        false
    }
}

/// Accepted service data. Rendering and edit application are intentionally later adapter
/// concerns; retaining them here proves routing never uses a URI as document authority.
#[derive(Debug, Default)]
pub(super) struct ServiceResults {
    diagnostics: Vec<Diagnostic>,
    completion: Option<AcceptedCompletion>,
    hover: Option<AcceptedHover>,
    definition: Option<AcceptedDefinition>,
}

impl ServiceResults {
    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug)]
struct AcceptedDefinition {
    origin: DocumentStateIdentity,
    result: DefinitionResult,
}

#[derive(Debug)]
struct AcceptedCompletion {
    identity: DocumentResultIdentity,
    result: CompletionResult,
    anchor: SurfacePoint,
    selected: usize,
}

#[derive(Debug)]
struct AcceptedHover {
    result: HoverResult,
    anchor: SurfacePoint,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PendingCompletion {
    position: Utf16Position,
    anchor: SurfacePoint,
}

#[derive(Debug, Clone, Copy)]
struct PendingHover {
    position: Utf16Position,
    anchor: SurfacePoint,
    ready_at: Instant,
}

/// Exact UI authority for one completion row. A delayed click must not apply
/// the same index from a later result or a remounted document.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompletionSelection {
    document_id: DocumentId,
    mount_generation: u64,
    identity: DocumentResultIdentity,
    index: usize,
}

/// Owns one authoritative editor surface and generation-fenced language-service view.
pub(super) struct AutomationCodeEditor<S, C>
where
    S: EditorSurface,
    C: LanguageServiceChannel,
{
    surface: S,
    document: DocumentDescriptor,
    service: Option<C>,
    service_state: Option<DocumentStateIdentity>,
    project_state: Option<ProjectStateIdentity>,
    worker_generation: Option<WorkerGeneration>,
    /// Last document state successfully queued to the worker. This may be newer
    /// than the last acknowledgement, but it is the exact state a later close
    /// must name after the worker drains its FIFO.
    service_document: Option<DocumentRef>,
    /// A full close/open synchronization is needed after transient backpressure.
    resync_pending: bool,
    status: ServiceStatus,
    outstanding: OutstandingRequests,
    results: ServiceResults,
    pending_completion: Option<PendingCompletion>,
    completion_request_anchor: Option<SurfacePoint>,
    hover_position: Option<Utf16Position>,
    pending_hover: Option<PendingHover>,
    hover_request_anchor: Option<SurfacePoint>,
    pending_definition: Option<Utf16Position>,
    pending_formatting: Option<FormattingOptions>,
    service_edit_applied: bool,
    closed: bool,
}

impl<S, C> AutomationCodeEditor<S, C>
where
    S: EditorSurface,
    C: LanguageServiceChannel,
{
    pub fn new(surface: S, document: DocumentDescriptor, service: Option<C>) -> Self {
        let mut editor = Self {
            surface,
            document,
            service,
            service_state: None,
            project_state: None,
            worker_generation: None,
            service_document: None,
            resync_pending: false,
            status: ServiceStatus::Starting,
            outstanding: OutstandingRequests::default(),
            results: ServiceResults::default(),
            pending_completion: None,
            completion_request_anchor: None,
            hover_position: None,
            pending_hover: None,
            hover_request_anchor: None,
            pending_definition: None,
            pending_formatting: None,
            service_edit_applied: false,
            closed: false,
        };
        editor.open_service_document();
        editor
    }

    pub fn document(&self) -> &DocumentDescriptor {
        &self.document
    }

    pub fn content(&self) -> String {
        self.surface.content()
    }

    pub fn is_modified(&self) -> bool {
        self.surface.is_modified()
    }

    pub fn service_status(&self) -> ServiceStatus {
        self.status
    }

    pub fn has_language_service(&self) -> bool {
        self.service.is_some()
    }

    pub fn results(&self) -> &ServiceResults {
        &self.results
    }

    pub fn update(&mut self, message: &S::Message) -> S::Effect {
        self.update_with_change(message).0
    }

    /// Updates the surface and reports whether its authoritative text changed.
    pub fn update_with_change(&mut self, message: &S::Message) -> (S::Effect, bool) {
        let SurfaceUpdate {
            effect,
            changes,
            completion,
            hover,
            semantic_context_changed,
            definition,
            formatting,
        } = self.surface.update(message);
        if self.closed {
            return (effect, false);
        }
        // Tick, pointer hover, focus hand-off to an overlay button, and scrolling
        // must not destroy a valid completion. Only mutations to text/caret/
        // selection/navigation invalidate the semantic request context.
        if semantic_context_changed {
            self.clear_transient_intelligence();
        }
        let changed = changes.is_some();
        if let Some(changes) = changes {
            self.document_changed(changes);
        }
        if let Some(intent) = completion
            && let Some(position) = scalar_to_utf16(intent.position, &self.surface.content())
        {
            self.clear_completion();
            self.clear_hover();
            self.pending_completion = Some(PendingCompletion {
                position,
                anchor: intent.anchor,
            });
        }
        match hover {
            HoverUpdate::Unchanged => {}
            HoverUpdate::Clear => self.clear_hover(),
            HoverUpdate::At(intent) => self.observe_hover_intent(intent, Instant::now()),
        }
        if let Some(position) = definition {
            self.pending_definition = scalar_to_utf16(position, &self.surface.content());
            self.results.definition = None;
            self.outstanding.set(RequestKind::Definition, None);
        }
        if let Some(options) = formatting {
            self.pending_formatting = Some(options);
            self.outstanding.set(RequestKind::Formatting, None);
        }
        (effect, changed)
    }

    /// Applies one current completion item through the authoritative surface.
    fn apply_completion(
        &mut self,
        index: usize,
        expected: DocumentResultIdentity,
    ) -> Option<(S::Effect, bool)> {
        let accepted = self.results.completion.as_ref()?;
        if accepted.identity != expected || self.service_state != Some(expected.state) {
            return None;
        }
        let item = accepted.result.items.get(index)?.clone();
        let SurfaceUpdate {
            effect,
            changes,
            completion: _,
            hover: _,
            semantic_context_changed: _,
            definition: _,
            formatting: _,
        } = self.surface.apply_completion(&item);
        let changed = changes.is_some();
        if let Some(changes) = changes {
            self.document_changed(changes);
        }
        self.clear_completion();
        self.clear_hover();
        Some((effect, changed))
    }

    fn apply_selected_completion(&mut self) -> Option<(S::Effect, bool)> {
        let completion = self.results.completion.as_ref()?;
        self.apply_completion(completion.selected, completion.identity)
    }

    fn move_completion_selection(&mut self, delta: i32) -> bool {
        let Some(completion) = &mut self.results.completion else {
            return false;
        };
        // The popup intentionally renders at most eight rows. Keyboard state
        // must stay within that visible set instead of selecting an unseen
        // item from a larger protocol result.
        let count = completion.result.items.len().min(8);
        if count == 0 {
            return false;
        }
        completion.selected = if delta.is_negative() {
            completion
                .selected
                .checked_sub(delta.unsigned_abs() as usize)
                .unwrap_or(count.saturating_sub(1))
        } else {
            completion
                .selected
                .saturating_add(delta as usize)
                .checked_rem(count)
                .unwrap_or(0)
        };
        true
    }

    pub fn rebind(&mut self, document: DocumentDescriptor, text: &str) -> S::Effect {
        let project_changed = self.document.document.key.project != document.document.key.project;
        self.close_service_document();
        let effect = self.surface.reset(text, document.language);
        self.surface.mark_saved();
        self.document = document;
        self.closed = false;
        if project_changed {
            self.project_state = None;
            self.worker_generation = None;
        }
        self.clear_service_state();
        self.pending_completion = None;
        self.completion_request_anchor = None;
        self.hover_position = None;
        self.pending_hover = None;
        self.hover_request_anchor = None;
        self.pending_definition = None;
        self.pending_formatting = None;
        self.service_edit_applied = false;
        self.status = ServiceStatus::Starting;
        if self.service_document.is_some() {
            self.resync_pending = true;
            self.retry_service_sync();
        } else {
            self.open_service_document();
        }
        effect
    }

    /// Call only after persistence has durably accepted this exact editor version.
    pub fn mark_saved(&mut self, disk_revision: DiskRevision) {
        if self.closed {
            return;
        }
        self.surface.mark_saved();
        self.document.disk_revision = Some(disk_revision);
        let command = Command::SaveDocument(SaveDocument {
            document: self.document.document,
            text: self.surface.content(),
            disk_revision,
        });
        if !self.send(command) {
            // The service rejected this save snapshot. A coalesced close/open
            // retry carries the same content and disk revision.
            self.schedule_service_resync();
        }
    }

    pub fn request_focus(&self) {
        self.surface.request_focus();
    }

    pub fn lose_focus(&mut self) {
        self.surface.lose_focus();
        self.clear_completion();
        self.clear_hover();
        self.pending_definition = None;
        self.pending_formatting = None;
        self.outstanding.clear();
        self.results.clear();
    }

    pub fn is_dialog_open(&self) -> bool {
        self.surface.is_dialog_open()
    }

    pub fn request_diagnostics(&mut self, request_id: RequestId) -> bool {
        self.begin_request(RequestKind::Diagnostics, request_id, |identity| {
            Command::RequestDiagnostics(DocumentRequest { identity })
        })
    }

    pub fn request_completion(&mut self, request_id: RequestId, position: Utf16Position) -> bool {
        let sent = self.begin_request(RequestKind::Completion, request_id, |identity| {
            Command::RequestCompletion(PositionRequest { identity, position })
        });
        if sent {
            self.completion_request_anchor = None;
        }
        sent
    }

    /// Sends the newest editor-triggered completion after its exact document
    /// version has been acknowledged by the worker.
    pub fn request_pending_completion(&mut self, request_id: RequestId) -> bool {
        let Some(pending) = self.pending_completion.take() else {
            return false;
        };
        if self.begin_request(RequestKind::Completion, request_id, |identity| {
            Command::RequestCompletion(PositionRequest {
                identity,
                position: pending.position,
            })
        }) {
            self.completion_request_anchor = Some(pending.anchor);
            true
        } else {
            self.pending_completion = Some(pending);
            false
        }
    }

    pub fn request_pending_definition(&mut self, request_id: RequestId) -> bool {
        let Some(position) = self.pending_definition.take() else {
            return false;
        };
        if self.request_definition(request_id, position) {
            true
        } else {
            self.pending_definition = Some(position);
            false
        }
    }

    pub fn request_pending_formatting(&mut self, request_id: RequestId) -> bool {
        let Some(options) = self.pending_formatting.take() else {
            return false;
        };
        if self.request_formatting(request_id, options) {
            true
        } else {
            self.pending_formatting = Some(options);
            false
        }
    }

    pub fn request_hover(&mut self, request_id: RequestId, position: Utf16Position) -> bool {
        let sent = self.begin_request(RequestKind::Hover, request_id, |identity| {
            Command::RequestHover(PositionRequest { identity, position })
        });
        if sent {
            self.hover_request_anchor = None;
        }
        sent
    }

    fn request_pending_hover(&mut self, request_id: RequestId, now: Instant) -> bool {
        let Some(pending) = self.pending_hover.take() else {
            return false;
        };
        if now < pending.ready_at {
            self.pending_hover = Some(pending);
            return false;
        }
        if self.begin_request(RequestKind::Hover, request_id, |identity| {
            Command::RequestHover(PositionRequest {
                identity,
                position: pending.position,
            })
        }) {
            self.hover_request_anchor = Some(pending.anchor);
            true
        } else {
            self.pending_hover = Some(pending);
            false
        }
    }

    fn clear_completion(&mut self) {
        self.pending_completion = None;
        self.completion_request_anchor = None;
        self.outstanding.set(RequestKind::Completion, None);
        self.results.completion = None;
    }

    fn clear_hover(&mut self) {
        self.hover_position = None;
        self.pending_hover = None;
        self.hover_request_anchor = None;
        self.outstanding.set(RequestKind::Hover, None);
        self.results.hover = None;
    }

    fn clear_transient_intelligence(&mut self) {
        self.clear_completion();
        self.clear_hover();
    }

    fn observe_hover_intent(&mut self, intent: HoverIntent, now: Instant) {
        // Completion is the active interaction; background hover motion must
        // never replace it or open a competing overlay.
        if self.pending_completion.is_some()
            || self.outstanding.completion.is_some()
            || self.results.completion.is_some()
        {
            self.clear_hover();
            return;
        }
        let Some(position) = scalar_to_utf16(intent.position, &self.surface.content()) else {
            self.clear_hover();
            return;
        };
        if self.hover_position == Some(position) {
            return;
        }
        self.clear_hover();
        self.hover_position = Some(position);
        self.pending_hover = Some(PendingHover {
            position,
            anchor: intent.anchor,
            ready_at: now + HOVER_DEBOUNCE,
        });
    }

    pub(super) fn dismiss_overlays(&mut self) {
        self.clear_completion();
        self.clear_hover();
    }

    pub fn request_definition(&mut self, request_id: RequestId, position: Utf16Position) -> bool {
        self.begin_request(RequestKind::Definition, request_id, |identity| {
            Command::RequestDefinition(PositionRequest { identity, position })
        })
    }

    pub fn request_formatting(
        &mut self,
        request_id: RequestId,
        options: FormattingOptions,
    ) -> bool {
        self.begin_request(RequestKind::Formatting, request_id, |identity| {
            Command::RequestFormatting(FormattingRequest { identity, options })
        })
    }

    fn take_definition(&mut self) -> Option<AcceptedDefinition> {
        self.results.definition.take()
    }

    fn goto_utf16_position(&mut self, position: Utf16Position) -> Option<S::Effect> {
        let position = utf16_to_scalar(position, &self.surface.content())?;
        Some(self.surface.goto_position(position))
    }

    fn is_current_state(&self, state: DocumentStateIdentity) -> bool {
        !self.closed && self.service_state == Some(state)
    }

    fn take_service_edit_applied(&mut self) -> bool {
        std::mem::take(&mut self.service_edit_applied)
    }

    pub fn cancel_request(&mut self, request_id: RequestId) -> bool {
        if !self.outstanding.take_matching(request_id) {
            return false;
        }
        let project = self.document.document.key.project;
        self.send(Command::Cancel(CancelRequest {
            project,
            request_id,
        }))
    }

    pub fn apply_service_event(&mut self, envelope: &EventEnvelope) -> EventDisposition {
        if envelope.validate().is_err() {
            return EventDisposition::Invalid;
        }
        if self.closed {
            return EventDisposition::Stale;
        }

        match &envelope.event {
            Event::StateAcknowledged(state) => self.apply_acknowledgement(*state),
            Event::WorkerRestarted { worker_generation } => {
                if self
                    .worker_generation
                    .is_some_and(|current| *worker_generation <= current)
                {
                    return EventDisposition::Stale;
                }
                self.worker_generation = Some(*worker_generation);
                self.project_state = None;
                self.service_document = None;
                self.resync_pending = false;
                self.clear_service_state();
                self.status = ServiceStatus::Starting;
                self.open_service_document();
                EventDisposition::Applied
            }
            Event::Diagnostics(result) => {
                if !self.accepts_result(result, RequestKind::Diagnostics) {
                    return EventDisposition::Stale;
                }
                let text = self.surface.content();
                if !diagnostic_ranges_fit(
                    &result.result,
                    self.document.document.key.document_id,
                    &text,
                ) {
                    return EventDisposition::Invalid;
                }
                self.results.diagnostics.clone_from(&result.result.items);
                self.outstanding.set(RequestKind::Diagnostics, None);
                EventDisposition::Applied
            }
            Event::Completion(result) => {
                if !self.accepts_result(result, RequestKind::Completion) {
                    return EventDisposition::Stale;
                }
                if !completion_ranges_fit(&result.result, &self.surface.content()) {
                    self.clear_completion();
                    return EventDisposition::Invalid;
                }
                let anchor = self.completion_request_anchor.take().unwrap_or_default();
                self.clear_hover();
                self.results.completion =
                    (!result.result.items.is_empty()).then(|| AcceptedCompletion {
                        identity: result.identity,
                        result: result.result.clone(),
                        anchor,
                        selected: 0,
                    });
                self.outstanding.set(RequestKind::Completion, None);
                EventDisposition::Applied
            }
            Event::Hover(result) => {
                if !self.accepts_result(result, RequestKind::Hover) {
                    return EventDisposition::Stale;
                }
                let text = self.surface.content();
                if !result
                    .result
                    .as_ref()
                    .is_none_or(|hover| hover.range.is_none_or(|range| range_fits(range, &text)))
                {
                    self.clear_hover();
                    return EventDisposition::Invalid;
                }
                let anchor = self.hover_request_anchor.take().unwrap_or_default();
                self.results.hover = result
                    .result
                    .clone()
                    .map(|result| AcceptedHover { result, anchor });
                self.outstanding.set(RequestKind::Hover, None);
                EventDisposition::Applied
            }
            Event::Definition(result) => {
                if !self.accepts_result(result, RequestKind::Definition) {
                    return EventDisposition::Stale;
                }
                let text = self.surface.content();
                if !definition_ranges_fit(
                    &result.result,
                    self.document.document.key.document_id,
                    &text,
                ) {
                    return EventDisposition::Invalid;
                }
                self.results.definition = Some(AcceptedDefinition {
                    origin: result.identity.state,
                    result: result.result.clone(),
                });
                self.outstanding.set(RequestKind::Definition, None);
                EventDisposition::Applied
            }
            Event::Formatting(result) => {
                if !self.accepts_result(result, RequestKind::Formatting) {
                    return EventDisposition::Stale;
                }
                let text = self.surface.content();
                if !simultaneous_text_edits_fit(&result.result.edits, &text) {
                    return EventDisposition::Invalid;
                }
                self.outstanding.set(RequestKind::Formatting, None);
                let Ok(changes) = self.surface.apply_text_edits(&result.result.edits) else {
                    return EventDisposition::Invalid;
                };
                if let Some(changes) = changes {
                    self.document_changed(changes);
                    self.service_edit_applied = true;
                }
                EventDisposition::Applied
            }
            Event::ProjectStatus(event) => {
                if event.identity.project != self.document.document.key.project
                    || self
                        .worker_generation
                        .is_some_and(|generation| generation != event.identity.worker_generation)
                {
                    return EventDisposition::Stale;
                }
                if self
                    .service_state
                    .is_some_and(|state| !project_identity_matches_document(event.identity, state))
                {
                    return EventDisposition::Stale;
                }
                self.worker_generation = Some(event.identity.worker_generation);
                self.project_state = Some(event.identity);
                self.status = match &event.status {
                    ProjectStatus::Ready => ServiceStatus::Ready,
                    ProjectStatus::Degraded { .. } => {
                        self.outstanding.clear();
                        self.results.clear();
                        ServiceStatus::Unavailable
                    }
                };
                EventDisposition::Applied
            }
            Event::RequestFailed(failure) => self.apply_request_failure(failure.scope),
        }
    }

    pub fn close(&mut self) {
        self.close_service_document();
    }

    fn document_changed(&mut self, changes: DocumentChanges) {
        let previous = self.document.document;
        let Some(new_version) = DocumentVersion::new(previous.version.get().saturating_add(1))
        else {
            self.request_service_close();
            self.clear_service_state();
            self.status = ServiceStatus::Unavailable;
            return;
        };

        self.document.document.version = new_version;
        self.clear_service_state();
        self.status = ServiceStatus::Starting;
        if changes.validate().is_err()
            || self.resync_pending
            || self.service_document != Some(previous)
        {
            self.schedule_service_resync();
            return;
        }
        let command = Command::ChangeDocument(ChangeDocument {
            document: previous,
            new_version,
            changes,
        });
        if self.send(command) {
            self.service_document = Some(self.document.document);
        } else {
            self.resync_pending = true;
        }
    }

    fn open_service_document(&mut self) {
        if self.service.is_none() {
            self.service_document = None;
            self.resync_pending = false;
            self.status = ServiceStatus::Unavailable;
            return;
        }
        let text = self.surface.content();
        if validate_document_text(&text).is_err() {
            self.service_document = None;
            self.resync_pending = false;
            self.status = ServiceStatus::Unavailable;
            return;
        }
        let command = Command::OpenDocument(OpenDocument {
            descriptor: self.document.clone(),
            text,
        });
        if self.send(command) {
            self.service_document = Some(self.document.document);
            self.resync_pending = false;
        } else {
            self.service_document = None;
            self.resync_pending = true;
        }
    }

    fn close_service_document(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(document) = self.service_document
            && self.send(Command::CloseDocument(CloseDocument { document }))
        {
            self.service_document = None;
        }
        self.resync_pending = false;
        self.clear_service_state();
    }

    fn request_service_close(&mut self) {
        let Some(document) = self.service_document else {
            return;
        };
        if self.send(Command::CloseDocument(CloseDocument { document })) {
            self.service_document = None;
        }
    }

    fn schedule_service_resync(&mut self) {
        self.resync_pending = true;
        self.retry_service_sync();
    }

    /// Retries a coalesced full-snapshot synchronization after a channel send
    /// failed. A close which was queued successfully is never repeated; if the
    /// following open was rejected, the next tick resumes there.
    pub fn retry_service_sync(&mut self) {
        if self.closed || !self.resync_pending {
            return;
        }
        if let Some(document) = self.service_document {
            if !self.send(Command::CloseDocument(CloseDocument { document })) {
                return;
            }
            self.service_document = None;
        }
        self.status = ServiceStatus::Starting;
        self.open_service_document();
    }

    fn clear_service_state(&mut self) {
        self.service_state = None;
        self.completion_request_anchor = None;
        self.clear_hover();
        self.pending_definition = None;
        self.pending_formatting = None;
        self.outstanding.clear();
        self.results.clear();
    }

    fn send(&mut self, command: Command) -> bool {
        let Some(service) = &mut self.service else {
            self.status = ServiceStatus::Unavailable;
            return false;
        };
        if service.send(command).is_err() {
            self.status = ServiceStatus::Unavailable;
            return false;
        }
        true
    }

    fn begin_request(
        &mut self,
        kind: RequestKind,
        request_id: RequestId,
        command: impl FnOnce(DocumentResultIdentity) -> Command,
    ) -> bool {
        let Some(state) = self.service_state else {
            return false;
        };
        if self.closed || self.status != ServiceStatus::Ready {
            return false;
        }
        let identity = DocumentResultIdentity { state, request_id };
        if !self.send(command(identity)) {
            return false;
        }
        self.outstanding.set(kind, Some(request_id));
        true
    }

    fn accepts_result<T>(&self, result: &DocumentResult<T>, kind: RequestKind) -> bool {
        let Some(state) = self.service_state else {
            return false;
        };
        let Some(request_id) = self.outstanding.get(kind) else {
            return false;
        };
        result.identity.is_current_for(&state, request_id)
            && result
                .analyzed_uri
                .as_deref()
                .is_none_or(|uri| uri == self.document.uri)
    }

    fn apply_acknowledgement(&mut self, state: AcknowledgedState) -> EventDisposition {
        if let AcknowledgedState::ProjectRefreshed(project) = state {
            if project.project != self.document.document.key.project
                || self
                    .worker_generation
                    .is_some_and(|expected| project.worker_generation != expected)
                || self.project_state.is_some_and(|current| {
                    project.graph_generation <= current.graph_generation
                        || project.service_generation < current.service_generation
                })
            {
                return EventDisposition::Stale;
            }
            self.worker_generation = Some(project.worker_generation);
            self.project_state = Some(project);
            if let Some(mut current) = self.service_state {
                current.graph_generation = project.graph_generation;
                current.service_generation = project.service_generation;
                current.worker_generation = project.worker_generation;
                self.service_state = Some(current);
            }
            self.outstanding.clear();
            self.completion_request_anchor = None;
            self.clear_hover();
            self.results.clear();
            self.status = ServiceStatus::Ready;
            return EventDisposition::Applied;
        }
        let document_state = match state {
            AcknowledgedState::DocumentOpened(state)
            | AcknowledgedState::DocumentChanged(state)
            | AcknowledgedState::DocumentSaved(state) => state,
            _ => return EventDisposition::Stale,
        };
        if document_state.document != self.document.document {
            return EventDisposition::Stale;
        }
        if self
            .worker_generation
            .is_some_and(|expected| document_state.worker_generation != expected)
        {
            return EventDisposition::Stale;
        }
        if let Some(project) = self.project_state
            && (project.project != document_state.document.key.project
                || project.worker_generation != document_state.worker_generation
                || document_state.graph_generation < project.graph_generation
                || document_state.service_generation < project.service_generation)
        {
            return EventDisposition::Stale;
        }
        if let Some(current) = self.service_state
            && (document_state.graph_generation < current.graph_generation
                || document_state.service_generation < current.service_generation)
        {
            return EventDisposition::Stale;
        }
        self.worker_generation = Some(document_state.worker_generation);
        self.project_state = Some(ProjectStateIdentity {
            project: document_state.document.key.project,
            graph_generation: document_state.graph_generation,
            service_generation: document_state.service_generation,
            worker_generation: document_state.worker_generation,
        });
        self.service_document = Some(document_state.document);
        self.resync_pending = false;
        self.service_state = Some(document_state);
        if self.status == ServiceStatus::Starting {
            self.status = ServiceStatus::Ready;
        }
        EventDisposition::Applied
    }

    fn apply_request_failure(&mut self, scope: FailureScope) -> EventDisposition {
        match scope {
            FailureScope::Document(identity) => {
                let Some(state) = self.service_state else {
                    return EventDisposition::Stale;
                };
                if identity.state != state || !self.outstanding.take_matching(identity.request_id) {
                    return EventDisposition::Stale;
                }
                EventDisposition::Applied
            }
            FailureScope::Project(project) => {
                let Some(state) = self.service_state else {
                    return EventDisposition::Stale;
                };
                if project.project != state.document.key.project
                    || project.graph_generation != state.graph_generation
                    || project.service_generation != state.service_generation
                    || project.worker_generation != state.worker_generation
                {
                    return EventDisposition::Stale;
                }
                self.outstanding.clear();
                self.results.clear();
                self.status = ServiceStatus::Unavailable;
                EventDisposition::Applied
            }
            FailureScope::Worker { worker_generation } => {
                if self
                    .worker_generation
                    .is_some_and(|current| worker_generation != current)
                {
                    return EventDisposition::Stale;
                }
                self.worker_generation = Some(worker_generation);
                self.project_state = None;
                self.service_document = None;
                self.resync_pending = false;
                self.clear_service_state();
                self.status = ServiceStatus::Unavailable;
                EventDisposition::Applied
            }
        }
    }
}

/// Concrete writable editor type used by the Automations window.
pub(super) type ActiveCodeEditor =
    AutomationCodeEditor<IcedCodeEditorSurface, LanguageServiceClient>;

/// An upstream editor message fenced to the document that produced it.
///
/// Some upstream actions complete asynchronously (notably clipboard reads). The
/// identity prevents their eventual result from mutating a document opened later.
#[derive(Debug, Clone)]
pub(crate) struct BoundEditorMessage {
    pub(super) document_id: DocumentId,
    pub(super) mount_generation: u64,
    pub(super) message: IcedEditorMessage,
}

impl ActiveCodeEditor {
    /// Renders the upstream widget using Smudgy's nested-theme bridge.
    pub(super) fn view(&self) -> crate::theme::Element<'_, IcedEditorMessage> {
        self.surface.view()
    }

    fn overlay_metrics(&self) -> OverlayMetrics {
        self.surface.overlay_metrics()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OverlayPlacement {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn completion_placement(
    anchor: SurfacePoint,
    metrics: OverlayMetrics,
    desired_height: f32,
) -> OverlayPlacement {
    let viewport_width = metrics.viewport_width.max(8.0);
    let viewport_height = metrics.viewport_height.max(8.0);
    let width = 340.0_f32.min((viewport_width - 8.0).max(1.0));
    let height = desired_height.min((viewport_height - 8.0).max(1.0));
    let adjusted_y = (anchor.y - metrics.viewport_scroll).max(0.0);
    let space_below = viewport_height - adjusted_y - metrics.line_height;
    let show_above = space_below < height + 4.0 && adjusted_y >= height + 4.0;
    let max_x = (viewport_width - width - 4.0).max(4.0);
    let x = anchor.x.clamp(4.0, max_x);
    let y = if show_above {
        (adjusted_y - height - 4.0).max(0.0)
    } else {
        (adjusted_y + metrics.line_height + 4.0).min((viewport_height - height).max(0.0))
    };
    OverlayPlacement {
        x,
        y,
        width,
        height,
    }
}

fn hover_placement(
    anchor: SurfacePoint,
    metrics: OverlayMetrics,
    desired_height: f32,
) -> OverlayPlacement {
    let viewport_width = metrics.viewport_width.max(8.0);
    let viewport_height = metrics.viewport_height.max(8.0);
    let width = 380.0_f32.min((viewport_width - 8.0).max(1.0));
    let height = desired_height.min((viewport_height - 8.0).max(1.0));
    let adjusted_y = (anchor.y - metrics.viewport_scroll).max(0.0);
    let gap_x = (metrics.char_width * 0.5).max(2.0);
    let max_x = (viewport_width - width - 4.0).max(0.0);
    let right = anchor.x + gap_x;
    let left = anchor.x - width - gap_x;
    let x = if right <= max_x {
        right
    } else if left >= 0.0 {
        left
    } else {
        right.clamp(0.0, max_x)
    };
    let y = if adjusted_y >= height + 4.0 {
        (adjusted_y - height - 4.0).max(0.0)
    } else {
        (adjusted_y + metrics.line_height + 4.0).min((viewport_height - height).max(0.0))
    };
    OverlayPlacement {
        x,
        y,
        width,
        height,
    }
}

impl super::AutomationsWindow {
    pub(super) fn bind_code_editor_message(
        &self,
        message: IcedEditorMessage,
    ) -> Option<BoundEditorMessage> {
        Some(BoundEditorMessage {
            document_id: self
                .code_editor
                .as_ref()?
                .document()
                .document
                .key
                .document_id,
            mount_generation: self.code_editor_mount_generation,
            message,
        })
    }

    pub(super) fn code_editor_message_is_current(&self, message: &BoundEditorMessage) -> bool {
        self.code_editor.as_ref().is_some_and(|editor| {
            editor.document().document.key.document_id == message.document_id
                && self.code_editor_mount_generation == message.mount_generation
        })
    }

    /// Opens or rebinds the window's single writable code editor.
    pub(super) fn bind_code_editor(
        &mut self,
        text: &str,
        language: Language,
        kind: CodeDocument,
    ) -> iced::Task<super::Message> {
        self.code_editor_mount_generation = self
            .code_editor_mount_generation
            .checked_add(1)
            .unwrap_or(1);
        let needs_service = supports_language_service(language);
        let context = language_project_context(self, kind);
        let context_changed = self.language_project_target_context.as_ref() != Some(&context);
        self.language_project_target_context = Some(context.clone());
        let refresh_needed =
            needs_service && !self.language_project_is_installed_or_pending(&context);
        let bound_with_service = self
            .code_editor
            .as_ref()
            .is_some_and(AutomationCodeEditor::has_language_service);
        if self.code_editor.is_some()
            && (bound_with_service != needs_service || (needs_service && context_changed))
        {
            self.clear_code_editor();
        }
        if refresh_needed && let Some(client) = self.ensure_language_service() {
            self.install_language_project(context.clone(), &client);
        }
        let descriptor = self.code_document_descriptor(language, kind);
        let task = if let Some(editor) = &mut self.code_editor {
            editor.rebind(descriptor, text)
        } else {
            let surface = IcedCodeEditorSurface::new(text, language);
            let client = if needs_service {
                self.ensure_language_service()
            } else {
                None
            };
            self.code_editor = Some(AutomationCodeEditor::new(surface, descriptor, client));
            iced::Task::none()
        };
        self.bind_code_editor_task(task)
    }

    /// Returns the current code text. Writable code panes always bind before rendering.
    pub(super) fn code_editor_text(&self) -> String {
        self.code_editor
            .as_ref()
            .map_or_else(String::new, AutomationCodeEditor::content)
    }

    /// Renders the active writable editor plus its visible language-service
    /// status, current problems, and clickable completion candidates.
    pub(super) fn code_editor_view(&self, height: f32) -> super::Elem<'_> {
        let Some(editor) = &self.code_editor else {
            return iced::widget::container(iced::widget::text(""))
                .height(iced::Length::Fixed(height))
                .into();
        };
        let mount_generation = self.code_editor_mount_generation;

        let status = if !supports_language_service(editor.document().language) {
            crate::i18n::t!("automation-code-intelligence-not-applicable")
        } else {
            match editor.service_status() {
                ServiceStatus::Starting => {
                    crate::i18n::t!("automation-code-intelligence-starting")
                }
                ServiceStatus::Ready => crate::i18n::t!("automation-code-intelligence-ready"),
                ServiceStatus::Unavailable => {
                    crate::i18n::t!("automation-code-intelligence-unavailable")
                }
            }
        };

        let document_id = editor.document().document.key.document_id;
        let mut editor_layers: Vec<super::Elem<'_>> = vec![
            iced::widget::container(editor.view().map(move |message| {
                super::Message::CodeEditorAction(BoundEditorMessage {
                    document_id,
                    mount_generation,
                    message,
                })
            }))
            .width(iced::Length::Fill)
            .height(iced::Length::Fixed(height))
            .into(),
        ];

        if !editor.is_dialog_open()
            && let Some(completion) = &editor.results().completion
            && !completion.result.items.is_empty()
        {
            let visible = completion.result.items.len().min(8);
            let visible = f32::from(u16::try_from(visible).unwrap_or(8));
            let placement = completion_placement(
                completion.anchor,
                editor.overlay_metrics(),
                30.0 + visible * 26.0,
            );
            let mut candidates = iced::widget::Column::new().spacing(0.0).push(
                iced::widget::text(crate::i18n::t!("automation-code-completions"))
                    .size(11.0)
                    .style(super::common::regular),
            );
            for (index, item) in completion.result.items.iter().take(8).enumerate() {
                let label = item.detail.as_ref().map_or_else(
                    || item.label.clone(),
                    |detail| format!("{}  —  {detail}", item.label),
                );
                let selection = CompletionSelection {
                    document_id,
                    mount_generation,
                    identity: completion.identity,
                    index,
                };
                candidates = candidates.push(
                    iced::widget::button(iced::widget::text(label).size(11.0))
                        .padding([3.0, 6.0])
                        .width(iced::Length::Fill)
                        .height(iced::Length::Fixed(26.0))
                        .style(if index == completion.selected {
                            crate::theme::builtins::button::list_item_selected
                        } else {
                            crate::theme::builtins::button::list_item
                        })
                        .on_press(super::Message::ApplyCodeCompletion(selection)),
                );
            }
            let card = iced::widget::container(
                iced::widget::scrollable(candidates)
                    .height(iced::Length::Fixed((placement.height - 8.0).max(1.0))),
            )
            .padding(4.0)
            .width(iced::Length::Fixed(placement.width))
            .height(iced::Length::Fixed(placement.height))
            .style(crate::theme::builtins::container::tooltip);
            editor_layers.push(
                iced::widget::container(
                    iced::widget::column![
                        iced::widget::space::vertical().height(iced::Length::Fixed(placement.y)),
                        iced::widget::row![
                            iced::widget::space::horizontal()
                                .width(iced::Length::Fixed(placement.x)),
                            card
                        ]
                    ]
                    .spacing(0.0),
                )
                .width(iced::Length::Fill)
                .height(iced::Length::Fixed(height))
                .into(),
            );
        } else if !editor.is_dialog_open()
            && let Some(hover) = &editor.results().hover
        {
            let lines = hover.result.contents.value.lines().count().clamp(1, 8);
            let lines = f32::from(u16::try_from(lines).unwrap_or(8));
            let placement = hover_placement(
                hover.anchor,
                editor.overlay_metrics(),
                (16.0 + lines * 18.0).clamp(42.0, 180.0),
            );
            // Keep the embedded service's bounded markup inert here. Rendering
            // plain text avoids creating links or actions from user-authored docs.
            let card = iced::widget::container(
                iced::widget::scrollable(
                    iced::widget::text(hover.result.contents.value.as_str())
                        .size(12.0)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                )
                .height(iced::Length::Fixed((placement.height - 12.0).max(1.0))),
            )
            .padding(6.0)
            .width(iced::Length::Fixed(placement.width))
            .height(iced::Length::Fixed(placement.height))
            .style(crate::theme::builtins::container::tooltip);
            editor_layers.push(
                iced::widget::container(
                    iced::widget::column![
                        iced::widget::space::vertical().height(iced::Length::Fixed(placement.y)),
                        iced::widget::row![
                            iced::widget::space::horizontal()
                                .width(iced::Length::Fixed(placement.x)),
                            card
                        ]
                    ]
                    .spacing(0.0),
                )
                .width(iced::Length::Fill)
                .height(iced::Length::Fixed(height))
                .into(),
            );
        }

        let editor_area = iced::widget::mouse_area(
            iced::widget::stack(editor_layers)
                .width(iced::Length::Fill)
                .height(iced::Length::Fixed(height)),
        )
        .on_exit(super::Message::DismissCodeOverlays);
        let suggestion_button = iced::widget::button(
            iced::widget::text(crate::i18n::t!("automation-code-show-completions")).size(11.0),
        )
        .padding([2.0, 6.0])
        .style(crate::theme::builtins::button::subtle)
        .on_press_maybe(
            (editor.service_status() == ServiceStatus::Ready)
                .then_some(super::Message::TriggerCodeCompletion),
        );
        let status_row = iced::widget::row![
            iced::widget::text(status)
                .size(11.0)
                .style(super::common::muted),
            iced::widget::space::horizontal(),
            suggestion_button,
        ]
        .align_y(iced::alignment::Vertical::Center);
        let mut chrome = iced::widget::Column::new()
            .spacing(4.0)
            .push(editor_area)
            .push(status_row);

        if !editor.results().diagnostics.is_empty() {
            let mut problems = iced::widget::Column::new().spacing(2.0).push(
                iced::widget::text(crate::i18n::t!("automation-code-problems"))
                    .size(11.0)
                    .style(super::common::regular),
            );
            for diagnostic in editor.results().diagnostics.iter().take(4) {
                problems = problems.push(
                    iced::widget::text(format!(
                        "{}:{}  {}",
                        diagnostic.range.start.line.saturating_add(1),
                        diagnostic.range.start.character.saturating_add(1),
                        diagnostic.message
                    ))
                    .size(11.0)
                    .style(super::common::muted),
                );
            }
            chrome = chrome.push(problems);
        }

        chrome.into()
    }

    /// Applies one upstream editor event and maps every follow-up task back to this window.
    pub(super) fn update_code_editor(
        &mut self,
        message: &BoundEditorMessage,
    ) -> (iced::Task<super::Message>, bool) {
        let Some(editor) = &mut self.code_editor else {
            return (iced::Task::none(), false);
        };
        if editor.document().document.key.document_id != message.document_id {
            return (iced::Task::none(), false);
        }
        if self.code_editor_mount_generation != message.mount_generation {
            return (iced::Task::none(), false);
        }
        if editor.results.completion.is_some() {
            let moved = match message.message {
                IcedEditorMessage::ArrowKey(iced_code_editor::ArrowDirection::Up, false) => {
                    editor.move_completion_selection(-1)
                }
                IcedEditorMessage::ArrowKey(iced_code_editor::ArrowDirection::Down, false) => {
                    editor.move_completion_selection(1)
                }
                _ => false,
            };
            if moved {
                return (iced::Task::none(), false);
            }
            if matches!(message.message, IcedEditorMessage::Enter)
                && let Some((task, changed)) = editor.apply_selected_completion()
            {
                let document_id = editor.document().document.key.document_id;
                let mount_generation = self.code_editor_mount_generation;
                return (
                    task.map(move |message| {
                        super::Message::CodeEditorAction(BoundEditorMessage {
                            document_id,
                            mount_generation,
                            message,
                        })
                    }),
                    changed,
                );
            }
        }
        let (task, changed) = editor.update_with_change(&message.message);
        if editor.service_status() == ServiceStatus::Ready && editor.service_state.is_some() {
            if editor.pending_completion.is_some() {
                let request = next_wire_value::<RequestId>(&mut self.next_language_request_id);
                let _ = editor.request_pending_completion(request);
            }
            if editor.pending_definition.is_some() {
                let request = next_wire_value::<RequestId>(&mut self.next_language_request_id);
                let _ = editor.request_pending_definition(request);
            }
            if editor.pending_formatting.is_some() {
                let request = next_wire_value::<RequestId>(&mut self.next_language_request_id);
                let _ = editor.request_pending_formatting(request);
            }
        }
        let document_id = editor.document().document.key.document_id;
        let mount_generation = self.code_editor_mount_generation;
        (
            task.map(move |message| {
                super::Message::CodeEditorAction(BoundEditorMessage {
                    document_id,
                    mount_generation,
                    message,
                })
            }),
            changed,
        )
    }

    /// Applies a completion selected from the visible candidate list.
    pub(super) fn apply_code_completion(
        &mut self,
        selection: CompletionSelection,
    ) -> (iced::Task<super::Message>, bool) {
        let Some(editor) = &mut self.code_editor else {
            return (iced::Task::none(), false);
        };
        if selection.document_id != editor.document().document.key.document_id
            || selection.mount_generation != self.code_editor_mount_generation
        {
            return (iced::Task::none(), false);
        }
        let Some((task, changed)) = editor.apply_completion(selection.index, selection.identity)
        else {
            return (iced::Task::none(), false);
        };
        let document_id = editor.document().document.key.document_id;
        let mount_generation = self.code_editor_mount_generation;
        (
            task.map(move |message| {
                super::Message::CodeEditorAction(BoundEditorMessage {
                    document_id,
                    mount_generation,
                    message,
                })
            }),
            changed,
        )
    }

    /// Closes the active document while retaining the resident window-scoped worker.
    pub(super) fn clear_code_editor(&mut self) {
        self.code_editor = None;
    }

    /// Records a successful durable save in both editor history and the service.
    pub(super) fn mark_code_editor_saved(&mut self) {
        if self.code_editor.is_none() {
            return;
        }
        let revision = next_wire_value::<DiskRevision>(&mut self.next_code_disk_revision);
        if let Some(editor) = &mut self.code_editor {
            editor.mark_saved(revision);
        }
    }

    pub(super) fn code_editor_is_modified(&self) -> bool {
        self.code_editor
            .as_ref()
            .is_some_and(AutomationCodeEditor::is_modified)
    }

    fn bind_code_editor_task(
        &self,
        task: iced::Task<IcedEditorMessage>,
    ) -> iced::Task<super::Message> {
        let Some(document_id) = self
            .code_editor
            .as_ref()
            .map(|editor| editor.document().document.key.document_id)
        else {
            return iced::Task::none();
        };
        let mount_generation = self.code_editor_mount_generation;
        task.map(move |message| {
            super::Message::CodeEditorAction(BoundEditorMessage {
                document_id,
                mount_generation,
                message,
            })
        })
    }

    /// Drains worker events without blocking the UI, applies a fenced format reply, and
    /// returns any one-target definition navigation task.
    pub(super) fn poll_language_service(&mut self) -> (iced::Task<super::Message>, bool) {
        let events = self
            .language_service
            .as_mut()
            .map(LanguageServiceHost::drain_events)
            .unwrap_or_default();
        let mut project_retry = None;
        for event in &events {
            if let Some(retry) = self.observe_language_project_event(event) {
                project_retry = Some(retry);
            }
        }
        let mut refresh_diagnostics = false;
        let mut accepted_definition = None;
        let mut service_edit_applied = false;
        if let Some(editor) = &mut self.code_editor {
            for event in &events {
                let disposition = editor.apply_service_event(event);
                refresh_diagnostics |= disposition == EventDisposition::Applied
                    && matches!(
                        event.event,
                        Event::StateAcknowledged(
                            AcknowledgedState::DocumentOpened(_)
                                | AcknowledgedState::DocumentChanged(_)
                                | AcknowledgedState::DocumentSaved(_)
                                | AcknowledgedState::ProjectRefreshed(_)
                        )
                    );
            }
            editor.retry_service_sync();
            accepted_definition = editor.take_definition();
            service_edit_applied = editor.take_service_edit_applied();
        }
        if let Some(retry) = project_retry
            && let Some(client) = self
                .language_service
                .as_ref()
                .map(LanguageServiceHost::client)
        {
            self.install_language_project_with_retries(
                retry.context,
                &client,
                retry.retries_remaining,
            );
        }
        if refresh_diagnostics && let Some(editor) = &mut self.code_editor {
            let request = next_wire_value::<RequestId>(&mut self.next_language_request_id);
            let _ = editor.request_diagnostics(request);
        }
        if let Some(editor) = &mut self.code_editor
            && editor.service_status() == ServiceStatus::Ready
            && editor.service_state.is_some()
        {
            if editor.pending_completion.is_some() {
                let request = next_wire_value::<RequestId>(&mut self.next_language_request_id);
                let _ = editor.request_pending_completion(request);
            }
            let now = Instant::now();
            if editor
                .pending_hover
                .as_ref()
                .is_some_and(|pending| now >= pending.ready_at)
            {
                let request = next_wire_value::<RequestId>(&mut self.next_language_request_id);
                let _ = editor.request_pending_hover(request, now);
            }
            if editor.pending_definition.is_some() {
                let request = next_wire_value::<RequestId>(&mut self.next_language_request_id);
                let _ = editor.request_pending_definition(request);
            }
            if editor.pending_formatting.is_some() {
                let request = next_wire_value::<RequestId>(&mut self.next_language_request_id);
                let _ = editor.request_pending_formatting(request);
            }
        }
        let task = accepted_definition.map_or_else(iced::Task::none, |definition| {
            self.definition_navigation_task(definition)
        });
        (task, service_edit_applied)
    }

    fn definition_navigation_task(
        &mut self,
        definition: AcceptedDefinition,
    ) -> iced::Task<super::Message> {
        let Some(editor) = self.code_editor.as_ref() else {
            return iced::Task::none();
        };
        if !editor.is_current_state(definition.origin) {
            return iced::Task::none();
        }
        let current_document = editor.document().document.key.document_id;
        let target = definition.result.targets.into_iter().find(|target| {
            target.document_id == current_document || self.definition_source_key(target).is_some()
        });
        let Some(target) = target else {
            return iced::Task::none();
        };

        if target.document_id == current_document {
            let Some(editor) = &mut self.code_editor else {
                return iced::Task::none();
            };
            let Some(task) = editor.goto_utf16_position(target.target_selection_range.start) else {
                return iced::Task::none();
            };
            let document_id = editor.document().document.key.document_id;
            let mount_generation = self.code_editor_mount_generation;
            return task.map(move |message| {
                super::Message::CodeEditorAction(BoundEditorMessage {
                    document_id,
                    mount_generation,
                    message,
                })
            });
        }

        iced::Task::done(super::Message::NavigateCodeDefinition(
            DefinitionNavigation {
                origin: definition.origin,
                origin_mount_generation: self.code_editor_mount_generation,
                target,
            },
        ))
    }

    fn definition_source_key(&self, target: &DefinitionTarget) -> Option<LanguageSourceKey> {
        let key = self
            .language_source_ids
            .iter()
            .find_map(|(key, document_id)| {
                (*document_id == target.document_id).then(|| key.clone())
            })?;
        if matches!(key, LanguageSourceKey::InlineBridge)
            || !self.language_source_key_is_current(&key)
        {
            return None;
        }
        let expected_uri = language_source_key_uri(&key)?;
        target
            .analyzed_uri
            .as_deref()
            .is_none_or(|uri| uri == expected_uri)
            .then_some(key)
    }

    fn language_source_key_is_current(&self, key: &LanguageSourceKey) -> bool {
        match (&self.language_project_context, key) {
            (Some(LanguageProjectContext::Modules), LanguageSourceKey::Module(_)) => true,
            (
                Some(LanguageProjectContext::OwnedPackage(current)),
                LanguageSourceKey::OwnedPackage { package, .. },
            ) => current == package,
            _ => false,
        }
    }

    pub(super) fn navigate_code_definition(
        &mut self,
        navigation: DefinitionNavigation,
    ) -> crate::update::Update<super::Message, super::Event> {
        self.navigate_code_definition_checked(navigation).0
    }

    /// Performs a definition jump and reports whether the origin draft was actually left.
    /// The outcome lets the Discard confirmation preserve its dirty state when a queued jump
    /// became stale while the confirmation banner was open.
    pub(super) fn navigate_code_definition_checked(
        &mut self,
        navigation: DefinitionNavigation,
    ) -> (crate::update::Update<super::Message, super::Event>, bool) {
        let Some(editor) = self.code_editor.as_ref() else {
            return (crate::update::Update::none(), false);
        };
        if self.code_editor_mount_generation != navigation.origin_mount_generation
            || !editor.is_current_state(navigation.origin)
        {
            return (crate::update::Update::none(), false);
        }
        let origin_document_id = editor.document().document.key.document_id;
        let origin_mount_generation = self.code_editor_mount_generation;
        let Some(key) = self.definition_source_key(&navigation.target) else {
            return (crate::update::Update::none(), false);
        };
        let mut update = match key {
            LanguageSourceKey::Module(subpath) => self.open_module(subpath),
            LanguageSourceKey::OwnedPackage { package, subpath }
                if self
                    .local_package
                    .as_ref()
                    .is_some_and(|open| open.name == package) =>
            {
                self.select_owned_file(subpath)
            }
            LanguageSourceKey::OwnedPackage { .. } | LanguageSourceKey::InlineBridge => {
                return (crate::update::Update::none(), false);
            }
        };
        let left_origin = !self.code_editor.as_ref().is_some_and(|editor| {
            editor.document().document.key.document_id == origin_document_id
                && self.code_editor_mount_generation == origin_mount_generation
        });
        let Some(editor) = &mut self.code_editor else {
            return (update, left_origin);
        };
        if editor.document().document.key.document_id != navigation.target.document_id {
            return (update, left_origin);
        }
        if !definition_target_ranges_fit(&navigation.target, &editor.content()) {
            return (update, true);
        }
        let Some(task) = editor.goto_utf16_position(navigation.target.target_selection_range.start)
        else {
            return (update, true);
        };
        let document_id = editor.document().document.key.document_id;
        let mount_generation = self.code_editor_mount_generation;
        update.task = iced::Task::batch([
            update.task,
            task.map(move |message| {
                super::Message::CodeEditorAction(BoundEditorMessage {
                    document_id,
                    mount_generation,
                    message,
                })
            }),
        ]);
        (update, true)
    }

    /// Replaces the saved-source base for the current authoring context.
    ///
    /// The mounted editor remains open and therefore continues to shadow its matching
    /// saved source until it is closed or rebound.
    pub(super) fn refresh_language_project(&mut self) {
        let (Some(context), Some(client)) = (
            self.language_project_target_context
                .clone()
                .or_else(|| self.language_project_context.clone()),
            self.language_service
                .as_ref()
                .map(LanguageServiceHost::client),
        ) else {
            return;
        };
        self.install_language_project(context, &client);
    }

    fn install_language_project(
        &mut self,
        context: LanguageProjectContext,
        client: &LanguageServiceClient,
    ) {
        self.install_language_project_with_retries(context, client, 1);
    }

    fn install_language_project_with_retries(
        &mut self,
        context: LanguageProjectContext,
        client: &LanguageServiceClient,
        retries_remaining: u8,
    ) {
        let sources = self.language_project_sources(&context);
        let graph_generation =
            next_wire_value::<GraphGeneration>(&mut self.next_language_graph_generation);
        match client.send(Command::RefreshProject(RefreshProject {
            project: language_service_project(),
            graph_generation,
            sources,
        })) {
            Ok(command_sequence) => {
                self.pending_language_project_refresh = Some(PendingLanguageProjectRefresh {
                    context,
                    graph_generation,
                    command_sequence,
                    retries_remaining,
                });
            }
            Err(error) => {
                log::warn!("Failed to refresh Automations language-service project: {error}");
            }
        }
    }

    fn observe_language_project_event(
        &mut self,
        envelope: &EventEnvelope,
    ) -> Option<ProjectRefreshRetry> {
        if envelope.validate().is_err()
            || self
                .pending_language_project_refresh
                .as_ref()
                .is_none_or(|pending| pending.command_sequence != envelope.command_sequence)
        {
            return None;
        }
        match &envelope.event {
            Event::StateAcknowledged(AcknowledgedState::ProjectRefreshed(project)) => {
                let pending = self.pending_language_project_refresh.as_ref()?;
                if project.project != language_service_project()
                    || project.graph_generation != pending.graph_generation
                {
                    return None;
                }
                let pending = self.pending_language_project_refresh.take()?;
                self.language_project_context = Some(pending.context);
                None
            }
            Event::RequestFailed(failure) => {
                let pending = self.pending_language_project_refresh.take()?;
                (failure.retryable
                    && pending.retries_remaining > 0
                    && self.language_project_target_context.as_ref() == Some(&pending.context))
                .then(|| ProjectRefreshRetry {
                    context: pending.context,
                    retries_remaining: pending.retries_remaining - 1,
                })
            }
            _ => None,
        }
    }

    fn language_project_is_installed_or_pending(&self, context: &LanguageProjectContext) -> bool {
        self.pending_language_project_refresh.as_ref().map_or_else(
            || self.language_project_context.as_ref() == Some(context),
            |pending| &pending.context == context,
        )
    }

    pub(super) fn language_project_context_matches(
        &self,
        context: &LanguageProjectContext,
    ) -> bool {
        self.language_project_target_context.as_ref().map_or_else(
            || {
                self.pending_language_project_refresh.as_ref().map_or_else(
                    || self.language_project_context.as_ref() == Some(context),
                    |pending| &pending.context == context,
                )
            },
            |target| target == context,
        )
    }

    /// Reconciles a newly loaded standalone-module inventory with the mounted editor.
    ///
    /// A dirty editor stays mounted as the authoritative overlay. A clean editor is closed
    /// before the refreshed base is queued and then reopened from disk, preserving the worker's
    /// close -> refresh -> open ordering and preventing a stale clean overlay from hiding reloads.
    pub(super) fn reconcile_module_language_project_reload(
        &mut self,
    ) -> iced::Task<super::Message> {
        let clean = !self.dirty && !self.code_editor_is_modified();
        let open_subpath = match &self.pane {
            super::Pane::Module(super::ModuleState {
                subpath,
                path: Some(_),
                ..
            }) if clean => Some(subpath.clone()),
            _ => None,
        };
        let reloaded = open_subpath.as_ref().map(|subpath| {
            let module = self
                .modules
                .iter()
                .find(|module| &module.subpath == subpath)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "module disappeared during reload",
                    )
                })?;
            let text = std::fs::read_to_string(&module.path)?;
            Ok::<_, std::io::Error>((module.path.clone(), text, path_language(subpath)))
        });

        if reloaded.is_some() {
            self.clear_code_editor();
        }
        if self.language_project_context_matches(&LanguageProjectContext::Modules) {
            self.language_project_target_context = Some(LanguageProjectContext::Modules);
            self.refresh_language_project();
        }

        match reloaded {
            Some(Ok((path, text, language))) => {
                if let super::Pane::Module(state) = &mut self.pane {
                    state.path = Some(path);
                }
                self.bind_code_editor(&text, language, CodeDocument::StandaloneModule)
            }
            Some(Err(error)) => {
                let subpath = open_subpath.unwrap_or_default();
                self.pane = super::Pane::Error(std::sync::Arc::new(vec![crate::i18n::t!(
                    "editor-failed-read",
                    "path" => subpath,
                    "error" => error.to_string()
                )]));
                iced::Task::none()
            }
            None => iced::Task::none(),
        }
    }

    /// Reloads the package currently shown in the owned-package pane and reconciles its graph.
    /// Dirty source text remains mounted over the new disk base; clean text is remounted from the
    /// package snapshot. Binary/unreadable files retain the existing read-error degradation.
    pub(super) fn reconcile_owned_package_language_project_reload(
        &mut self,
    ) -> iced::Task<super::Message> {
        let Some(name) = (match &self.selection {
            super::Selection::OwnedPackage(name) => Some(name.clone()),
            _ => None,
        }) else {
            return iced::Task::none();
        };
        let package =
            match smudgy_core::models::local_packages::load_local_package(&self.server_name, &name)
            {
                Ok(Some(package)) => package,
                Ok(None) => return iced::Task::none(),
                Err(error) => {
                    log::warn!("Failed to reload open local package {name}: {error}");
                    return iced::Task::none();
                }
            };

        let clean = !self.dirty && !self.code_editor_is_modified();
        let selected = self.owned_selected_file.clone();
        let reloaded = selected.as_ref().filter(|_| clean).map(|subpath| {
            let module = package
                .modules
                .iter()
                .find(|module| &module.subpath == subpath)
                .ok_or("file disappeared during reload")?;
            String::from_utf8(module.content.clone()).map_err(|_| "file is not UTF-8 text")
        });

        if reloaded.is_some() {
            self.clear_code_editor();
        }
        self.local_package = Some(Box::new(package));
        let context = LanguageProjectContext::OwnedPackage(name.clone());
        if self.language_project_context_matches(&context) {
            self.language_project_target_context = Some(context);
            self.refresh_language_project();
        }

        match (selected, reloaded) {
            (Some(subpath), Some(Ok(text))) => {
                self.bind_code_editor(&text, path_language(&subpath), CodeDocument::OwnedPackage)
            }
            (Some(subpath), Some(Err(error))) => {
                self.owned_selected_file = None;
                self.authoring_feedback = Some(crate::i18n::t!(
                    "package-file-read-failed",
                    "path" => &subpath,
                    "error" => error
                ));
                iced::Task::none()
            }
            _ => iced::Task::none(),
        }
    }

    fn language_project_sources(&mut self, context: &LanguageProjectContext) -> Vec<ProjectSource> {
        let mut pending = Vec::new();
        let mut total_bytes = 0_usize;
        match context {
            // Inline automations are classic scripts compiled without a relative/bare module
            // referrer. Keep their graph isolated: the inline-only bridge is scoped to this
            // project, absolute ambient modules such as node: and smudgy: come from managed
            // declarations, and relative import() remains unsupported exactly as it is at
            // runtime.
            LanguageProjectContext::Inline => pending.push(PendingProjectSource {
                key: LanguageSourceKey::InlineBridge,
                uri: "smudgy-project:///inline/context.d.ts".to_owned(),
                language: Language::TypeScript,
                kind: DocumentKind::Generated,
                text: smudgy_core::models::script_typings::language_service_inline_bridge()
                    .to_owned(),
            }),
            LanguageProjectContext::Modules => {
                for module in &self.modules {
                    if pending.len() == MAX_PROJECT_SOURCE_FILES {
                        break;
                    }
                    let Some(uri) = project_source_uri("modules", &module.subpath) else {
                        continue;
                    };
                    let language = path_language(&module.subpath);
                    if !supports_project_source(language) {
                        continue;
                    }
                    let Ok(metadata) = std::fs::symlink_metadata(&module.path) else {
                        continue;
                    };
                    let remaining = MAX_PROJECT_SOURCE_TEXT_BYTES.saturating_sub(total_bytes);
                    let admitted_bytes = MAX_DOCUMENT_BYTES.min(remaining);
                    if !metadata.is_file()
                        || usize::try_from(metadata.len())
                            .map_or(true, |length| length > admitted_bytes)
                    {
                        continue;
                    }
                    let Ok(file) = std::fs::File::open(&module.path) else {
                        continue;
                    };
                    let Ok(limit) = u64::try_from(admitted_bytes.saturating_add(1)) else {
                        continue;
                    };
                    let mut bytes = Vec::with_capacity(
                        usize::try_from(metadata.len()).unwrap_or(admitted_bytes),
                    );
                    if file.take(limit).read_to_end(&mut bytes).is_err()
                        || bytes.len() > admitted_bytes
                    {
                        continue;
                    }
                    let Ok(text) = String::from_utf8(bytes) else {
                        continue;
                    };
                    if validate_document_text(&text).is_err() {
                        continue;
                    };
                    total_bytes += text.len();
                    pending.push(PendingProjectSource {
                        key: LanguageSourceKey::Module(module.subpath.clone()),
                        uri,
                        language,
                        kind: DocumentKind::Dependency,
                        text,
                    });
                }
            }
            LanguageProjectContext::OwnedPackage(name) => {
                let modules = self
                    .local_package
                    .as_deref()
                    .filter(|package| package.name == *name)
                    .map(|package| package.modules.as_slice())
                    .unwrap_or_default();
                for module in modules {
                    if pending.len() == MAX_PROJECT_SOURCE_FILES {
                        break;
                    }
                    let Some(uri) = owned_package_source_uri(name, &module.subpath) else {
                        continue;
                    };
                    let language = path_language(&module.subpath);
                    if !supports_project_source(language) {
                        continue;
                    }
                    let remaining = MAX_PROJECT_SOURCE_TEXT_BYTES.saturating_sub(total_bytes);
                    let admitted_bytes = MAX_DOCUMENT_BYTES.min(remaining);
                    if module.content.len() > admitted_bytes {
                        continue;
                    }
                    let Ok(text) = std::str::from_utf8(&module.content) else {
                        continue;
                    };
                    if validate_document_text(text).is_err() {
                        continue;
                    }
                    total_bytes += text.len();
                    pending.push(PendingProjectSource {
                        key: LanguageSourceKey::OwnedPackage {
                            package: name.clone(),
                            subpath: module.subpath.clone(),
                        },
                        uri,
                        language,
                        kind: DocumentKind::Dependency,
                        text: text.to_owned(),
                    });
                }
            }
        }

        let mut sources = Vec::with_capacity(pending.len());
        for source in pending {
            let document_id = self.language_source_id(source.key);
            sources.push(ProjectSource {
                document_id,
                uri: source.uri,
                language: source.language,
                kind: source.kind,
                text: source.text,
            });
        }
        sources
    }

    fn code_document_descriptor(
        &mut self,
        language: Language,
        kind: CodeDocument,
    ) -> DocumentDescriptor {
        let logical = logical_editor_source(self, kind);
        let (document_id, uri) = logical.map_or_else(
            || {
                let document_id = allocate_document_id();
                let extension = language_extension(language);
                (
                    document_id,
                    format!("smudgy-authoring:///document/{document_id}.{extension}"),
                )
            },
            |(key, uri)| (self.language_source_id(key), uri),
        );
        document_descriptor(document_id, uri, language, kind)
    }

    fn language_source_id(&mut self, key: LanguageSourceKey) -> DocumentId {
        *self
            .language_source_ids
            .entry(key)
            .or_insert_with(allocate_document_id)
    }

    fn ensure_language_service(&mut self) -> Option<LanguageServiceClient> {
        if let Some(host) = &self.language_service {
            return Some(host.client());
        }

        let libraries = smudgy_core::models::script_typings::embedded_language_service_types()
            .into_iter()
            .map(|file| LanguageServiceLibrary {
                file_name: file.virtual_path,
                text: file.contents.into(),
                is_root: file.is_root,
            })
            .collect();
        let host = match LanguageServiceHost::try_spawn_with_libraries(libraries) {
            Ok(host) => host,
            Err(error) => {
                log::warn!("Failed to start Automations language service: {error}");
                return None;
            }
        };
        let client = host.client();
        if let Err(error) = client.send(Command::OpenProject(OpenProject {
            project: language_service_project(),
        })) {
            log::warn!("Failed to open Automations language-service project: {error}");
        }
        self.language_service = Some(host);
        Some(client)
    }
}

/// Maps a persisted script language to an authoring/highlighting language.
pub(super) const fn script_language(language: smudgy_core::models::ScriptLang) -> Language {
    match language {
        smudgy_core::models::ScriptLang::JS => Language::JavaScript,
        smudgy_core::models::ScriptLang::TS => Language::TypeScript,
        smudgy_core::models::ScriptLang::Plaintext => Language::PlainText,
    }
}

pub(super) const fn supports_language_service(language: Language) -> bool {
    matches!(
        language,
        Language::JavaScript
            | Language::TypeScript
            | Language::JavaScriptReact
            | Language::TypeScriptReact
    )
}

/// Chooses a JS/TS-family language from a writable module path.
pub(super) fn path_language(path: &str) -> Language {
    match std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("js" | "mjs" | "cjs") => Language::JavaScript,
        Some("ts" | "mts" | "cts") => Language::TypeScript,
        Some("jsx") => Language::JavaScriptReact,
        Some("tsx") => Language::TypeScriptReact,
        Some("json") => Language::Json,
        _ => Language::PlainText,
    }
}

fn language_service_project() -> ProjectScope {
    ProjectScope {
        client_id: ClientId::new(1).expect("one is a valid language-service client id"),
        project_id: ProjectId::new(1).expect("one is a valid language-service project id"),
    }
}

fn language_project_context(
    window: &super::AutomationsWindow,
    kind: CodeDocument,
) -> LanguageProjectContext {
    match kind {
        CodeDocument::Alias | CodeDocument::Trigger | CodeDocument::Hotkey => {
            LanguageProjectContext::Inline
        }
        CodeDocument::StandaloneModule => LanguageProjectContext::Modules,
        CodeDocument::OwnedPackage => window.local_package.as_ref().map_or_else(
            || LanguageProjectContext::OwnedPackage(String::new()),
            |package| LanguageProjectContext::OwnedPackage(package.name.clone()),
        ),
    }
}

fn logical_editor_source(
    window: &super::AutomationsWindow,
    kind: CodeDocument,
) -> Option<(LanguageSourceKey, String)> {
    match kind {
        CodeDocument::StandaloneModule => {
            let super::Pane::Module(state) = &window.pane else {
                return None;
            };
            state.path.as_ref()?;
            let uri = project_source_uri("modules", &state.subpath)?;
            Some((LanguageSourceKey::Module(state.subpath.clone()), uri))
        }
        CodeDocument::OwnedPackage => {
            let package = window.local_package.as_ref()?.name.clone();
            let subpath = window.owned_selected_file.clone()?;
            let uri = owned_package_source_uri(&package, &subpath)?;
            Some((LanguageSourceKey::OwnedPackage { package, subpath }, uri))
        }
        CodeDocument::Alias | CodeDocument::Trigger | CodeDocument::Hotkey => None,
    }
}

fn project_source_uri(namespace: &str, subpath: &str) -> Option<String> {
    valid_project_subpath(subpath).then(|| format!("smudgy-project:///{namespace}/{subpath}"))
}

fn owned_package_source_uri(package: &str, subpath: &str) -> Option<String> {
    if !valid_project_subpath(package) {
        return None;
    }
    project_source_uri(&format!("packages/{package}"), subpath)
}

fn language_source_key_uri(key: &LanguageSourceKey) -> Option<String> {
    match key {
        LanguageSourceKey::Module(subpath) => project_source_uri("modules", subpath),
        LanguageSourceKey::OwnedPackage { package, subpath } => {
            owned_package_source_uri(package, subpath)
        }
        LanguageSourceKey::InlineBridge => Some("smudgy-project:///inline/context.d.ts".to_owned()),
    }
}

fn valid_project_subpath(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', '#', '?'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

const fn supports_project_source(language: Language) -> bool {
    !matches!(language, Language::PlainText)
}

const fn language_extension(language: Language) -> &'static str {
    match language {
        Language::JavaScript => "js",
        Language::TypeScript => "ts",
        Language::JavaScriptReact => "jsx",
        Language::TypeScriptReact => "tsx",
        Language::Json => "json",
        Language::PlainText => "txt",
    }
}

fn allocate_document_id() -> DocumentId {
    loop {
        if let Some(id) = DocumentId::from_bytes(*smudgy_cloud::Uuid::new_v4().as_bytes()) {
            break id;
        }
    }
}

fn document_descriptor(
    document_id: DocumentId,
    uri: String,
    language: Language,
    kind: CodeDocument,
) -> DocumentDescriptor {
    let kind = match kind {
        CodeDocument::Alias => DocumentKind::InlineAutomation {
            automation_kind: AutomationKind::Alias,
        },
        CodeDocument::Trigger => DocumentKind::InlineAutomation {
            automation_kind: AutomationKind::Trigger,
        },
        CodeDocument::Hotkey => DocumentKind::InlineAutomation {
            automation_kind: AutomationKind::Hotkey,
        },
        CodeDocument::StandaloneModule => DocumentKind::StandaloneModule,
        CodeDocument::OwnedPackage => DocumentKind::OwnedPackage,
    };
    DocumentDescriptor {
        document: DocumentRef {
            key: DocumentKey {
                project: language_service_project(),
                document_id,
            },
            view: None,
            version: DocumentVersion::new(1).expect("one is a valid document version"),
        },
        uri,
        language,
        kind,
        analysis_context: AnalysisContextId::new(1)
            .expect("one is a valid language-service analysis context"),
        disk_revision: None,
    }
}

fn next_wire_value<T>(next: &mut u64) -> T
where
    T: TryFrom<u64>,
    T::Error: std::fmt::Debug,
{
    let current = (*next).max(1);
    *next = current.saturating_add(1);
    T::try_from(current).unwrap_or_else(|_| {
        *next = 2;
        T::try_from(1).expect("one is valid for every language-service wire counter")
    })
}

fn range_fits(range: Utf16Range, text: &str) -> bool {
    range.to_byte_range(text).is_ok()
}

fn scalar_to_utf16(position: ScalarPosition, text: &str) -> Option<Utf16Position> {
    let line = text.split('\n').nth(usize::try_from(position.line).ok()?)?;
    let scalar_column = usize::try_from(position.character).ok()?;
    let mut utf16_column = 0_usize;
    let mut scalars = line.chars();
    for _ in 0..scalar_column {
        utf16_column = utf16_column.checked_add(scalars.next()?.len_utf16())?;
    }
    Some(Utf16Position {
        line: position.line,
        character: u32::try_from(utf16_column).ok()?,
    })
}

fn utf16_to_scalar(position: Utf16Position, text: &str) -> Option<ScalarPosition> {
    let byte_offset = position.to_byte_offset(text).ok()?;
    let line_start = text[..byte_offset]
        .rfind('\n')
        .map_or(0, |offset| offset.saturating_add(1));
    Some(ScalarPosition {
        line: position.line,
        character: u32::try_from(text[line_start..byte_offset].chars().count()).ok()?,
    })
}

fn simultaneous_text_edits_fit(edits: &[TextEdit], text: &str) -> bool {
    let mut ranges = Vec::with_capacity(edits.len());
    for edit in edits {
        let Ok(range) = edit.range.to_byte_range(text) else {
            return false;
        };
        ranges.push(range);
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges.windows(2).all(|pair| {
        let left = &pair[0];
        let right = &pair[1];
        let duplicate_empty = left.is_empty() && right.is_empty() && left.start == right.start;
        left.end <= right.start && !duplicate_empty
    })
}

fn project_identity_matches_document(
    project: ProjectStateIdentity,
    document: DocumentStateIdentity,
) -> bool {
    project.project == document.document.key.project
        && project.graph_generation == document.graph_generation
        && project.service_generation == document.service_generation
        && project.worker_generation == document.worker_generation
}

fn diagnostic_ranges_fit(
    result: &DiagnosticsResult,
    current_document: DocumentId,
    text: &str,
) -> bool {
    result.items.iter().all(|diagnostic| {
        range_fits(diagnostic.range, text)
            && diagnostic.related_information.iter().all(|related| {
                related.document_id != current_document || range_fits(related.range, text)
            })
    })
}

fn completion_ranges_fit(result: &CompletionResult, text: &str) -> bool {
    result.items.iter().all(|item| {
        item.primary_edit
            .iter()
            .chain(&item.additional_edits)
            .all(|edit| range_fits(edit.range, text))
    })
}

fn definition_ranges_fit(
    result: &DefinitionResult,
    current_document: DocumentId,
    text: &str,
) -> bool {
    result.targets.iter().all(|target| {
        target.document_id != current_document
            || (range_fits(target.target_range, text)
                && range_fits(target.target_selection_range, text))
    })
}

fn definition_target_ranges_fit(target: &DefinitionTarget, text: &str) -> bool {
    range_fits(target.target_range, text) && range_fits(target.target_selection_range, text)
}

impl<S, C> Drop for AutomationCodeEditor<S, C>
where
    S: EditorSurface,
    C: LanguageServiceChannel,
{
    fn drop(&mut self) {
        self.close_service_document();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use smudgy_script::language_service::{
        AnalysisContextId, AutomationKind, ClientId, CommandSequence, CompletionItem,
        CompletionItemId, CompletionKind, DiagnosticCode, DiagnosticSeverity, DiagnosticsResult,
        DocumentId, DocumentKey, DocumentKind, DocumentRef, DocumentResult, DocumentResultIdentity,
        EventEnvelope, FormattingResult, GraphGeneration, InsertTextFormat, Language,
        MAX_DOCUMENT_CHANGES, MarkupContent, MarkupKind, PROTOCOL_VERSION, ProjectId, ProjectScope,
        ProjectStateIdentity, ProjectStatusEvent, ServiceGeneration, TextChange, Utf16Range,
        WorkerGeneration,
    };

    use super::*;

    #[test]
    fn module_path_languages_are_explicit_and_unknown_files_are_plaintext() {
        assert_eq!(path_language("main.js"), Language::JavaScript);
        assert_eq!(path_language("main.mjs"), Language::JavaScript);
        assert_eq!(path_language("main.ts"), Language::TypeScript);
        assert_eq!(path_language("main.mts"), Language::TypeScript);
        assert_eq!(path_language("view.tsx"), Language::TypeScriptReact);
        assert_eq!(path_language("data.json"), Language::Json);
        assert_eq!(path_language("notes.md"), Language::PlainText);
        assert_eq!(path_language("LICENSE"), Language::PlainText);
    }

    #[test]
    fn project_source_uris_preserve_safe_logical_paths_and_reject_ambiguous_ones() {
        assert_eq!(
            project_source_uri("modules", "combat/helpers.ts").as_deref(),
            Some("smudgy-project:///modules/combat/helpers.ts")
        );
        assert_eq!(
            owned_package_source_uri("my_package", "lib/view.tsx").as_deref(),
            Some("smudgy-project:///packages/my_package/lib/view.tsx")
        );
        assert!(project_source_uri("modules", "../escape.ts").is_none());
        assert!(project_source_uri("modules", "ambiguous#name.ts").is_none());
        assert!(project_source_uri("modules", "query?name.ts").is_none());
        assert!(project_source_uri("modules", "back\\slash.ts").is_none());
    }

    #[test]
    fn scalar_completion_positions_convert_to_utf16() {
        assert_eq!(
            scalar_to_utf16(
                ScalarPosition {
                    line: 0,
                    character: 2,
                },
                "🙂x"
            ),
            Some(Utf16Position {
                line: 0,
                character: 3,
            })
        );
        assert_eq!(
            scalar_to_utf16(
                ScalarPosition {
                    line: 1,
                    character: 1,
                },
                "first\n🙂x"
            ),
            Some(Utf16Position {
                line: 1,
                character: 2,
            })
        );
    }

    #[test]
    fn anchored_overlays_follow_scroll_and_stay_inside_the_editor() {
        let metrics = OverlayMetrics {
            viewport_width: 400.0,
            viewport_height: 240.0,
            viewport_scroll: 80.0,
            line_height: 18.0,
            char_width: 8.0,
        };
        let completion = completion_placement(SurfacePoint { x: 390.0, y: 290.0 }, metrics, 100.0);
        assert!(completion.x + completion.width <= metrics.viewport_width);
        assert!(completion.y + completion.height <= metrics.viewport_height);
        assert!(completion.y < 210.0, "insufficient room places it above");

        let hover = hover_placement(SurfacePoint { x: 395.0, y: 90.0 }, metrics, 80.0);
        assert!(hover.x + hover.width <= metrics.viewport_width);
        assert!(hover.y + hover.height <= metrics.viewport_height);
    }

    #[test]
    fn cross_file_definition_requires_complete_target_ranges_to_fit() {
        let mut target = DefinitionTarget {
            document_id: DocumentId::try_from([9_u8; 16]).expect("non-nil target ID"),
            target_range: Utf16Range {
                start: Utf16Position {
                    line: 0,
                    character: 0,
                },
                end: Utf16Position {
                    line: 0,
                    character: 8,
                },
            },
            target_selection_range: Utf16Range {
                start: Utf16Position {
                    line: 0,
                    character: 2,
                },
                end: Utf16Position {
                    line: 0,
                    character: 8,
                },
            },
            analyzed_uri: Some("smudgy-project:///modules/target.ts".to_owned()),
        };
        assert!(definition_target_ranges_fit(&target, "🙂target\n"));

        target.target_selection_range.end.character = 99;
        assert!(!definition_target_ranges_fit(&target, "🙂target\n"));
    }

    #[test]
    fn confirmed_same_package_navigation_reseeds_discarded_manifest_draft() {
        let manifest = smudgy_script::PackageManifest::parse(r#"{"version":"1.0.0"}"#)
            .expect("parse canonical manifest");
        let mut window = super::super::AutomationsWindow::new(
            iced::window::Id::unique(),
            "definition-manifest-discard-test".to_owned(),
            crate::cloud_account::test_handles(),
            smudgy_core::session::SessionId::from(1),
        );
        window.local_package = Some(Box::new(
            smudgy_core::models::local_packages::LocalPackage {
                name: "demo".to_owned(),
                manifest: manifest.clone(),
                readme: None,
                modules: Vec::new(),
            },
        ));
        let mut draft = super::super::manifest::ManifestDraft::from_manifest(&manifest);
        draft.version = "unsaved".to_owned();
        window.manifest_draft = Some(draft);
        window.manifest_dirty = true;
        window.manifest_editing = true;
        window.dirty = true;

        window.accept_discarded_navigation();

        assert!(!window.dirty);
        assert!(!window.manifest_dirty);
        assert!(!window.manifest_editing);
        assert_eq!(
            window
                .manifest_draft
                .as_ref()
                .expect("reseeded manifest")
                .version,
            "1.0.0"
        );
    }

    #[derive(Debug)]
    enum SurfaceMessage {
        Replace(String),
        Navigate,
        Passive,
        RequestCompletion(ScalarPosition),
        Hover(HoverUpdate),
    }

    #[derive(Debug)]
    struct FakeSurface {
        text: String,
        modified: bool,
        focused: Cell<bool>,
        goto: Cell<Option<ScalarPosition>>,
    }

    impl FakeSurface {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_owned(),
                modified: false,
                focused: Cell::new(false),
                goto: Cell::new(None),
            }
        }
    }

    impl EditorSurface for FakeSurface {
        type Message = SurfaceMessage;
        type Effect = ();

        fn content(&self) -> String {
            self.text.clone()
        }

        fn update(&mut self, message: &Self::Message) -> SurfaceUpdate<Self::Effect> {
            match message {
                SurfaceMessage::Replace(text) => {
                    self.text.clone_from(text);
                    self.modified = true;
                    SurfaceUpdate {
                        effect: (),
                        changes: Some(DocumentChanges {
                            changes: vec![TextChange {
                                range: None,
                                text: text.clone(),
                            }],
                        }),
                        completion: None,
                        hover: HoverUpdate::Unchanged,
                        semantic_context_changed: true,
                        definition: None,
                        formatting: None,
                    }
                }
                SurfaceMessage::Navigate => SurfaceUpdate {
                    effect: (),
                    changes: None,
                    completion: None,
                    hover: HoverUpdate::Unchanged,
                    semantic_context_changed: true,
                    definition: None,
                    formatting: None,
                },
                SurfaceMessage::Passive => SurfaceUpdate {
                    effect: (),
                    changes: None,
                    completion: None,
                    hover: HoverUpdate::Unchanged,
                    semantic_context_changed: false,
                    definition: None,
                    formatting: None,
                },
                SurfaceMessage::RequestCompletion(position) => SurfaceUpdate {
                    effect: (),
                    changes: None,
                    completion: Some(CompletionIntent {
                        position: *position,
                        anchor: SurfacePoint { x: 24.0, y: 36.0 },
                    }),
                    hover: HoverUpdate::Unchanged,
                    semantic_context_changed: false,
                    definition: None,
                    formatting: None,
                },
                SurfaceMessage::Hover(hover) => SurfaceUpdate {
                    effect: (),
                    changes: None,
                    completion: None,
                    hover: *hover,
                    semantic_context_changed: false,
                    definition: None,
                    formatting: None,
                },
            }
        }

        fn apply_completion(
            &mut self,
            item: &smudgy_script::language_service::CompletionItem,
        ) -> SurfaceUpdate<Self::Effect> {
            let text = item.insert_text.as_deref().unwrap_or(&item.label);
            self.text.push_str(text);
            self.modified = true;
            SurfaceUpdate {
                effect: (),
                changes: Some(DocumentChanges {
                    changes: vec![TextChange {
                        range: None,
                        text: self.text.clone(),
                    }],
                }),
                completion: None,
                hover: HoverUpdate::Clear,
                semantic_context_changed: true,
                definition: None,
                formatting: None,
            }
        }

        fn apply_text_edits(&mut self, edits: &[TextEdit]) -> Result<Option<DocumentChanges>, ()> {
            let before = self.text.clone();
            let mut replacements = edits
                .iter()
                .map(|edit| {
                    edit.range
                        .to_byte_range(&before)
                        .map(|range| (range, edit.new_text.clone()))
                        .map_err(|_| ())
                })
                .collect::<Result<Vec<_>, ()>>()?;
            replacements.sort_by_key(|(range, _)| range.start);
            for (range, text) in replacements.into_iter().rev() {
                self.text.replace_range(range, &text);
            }
            if self.text == before {
                return Ok(None);
            }
            self.modified = true;
            Ok(Some(DocumentChanges {
                changes: vec![TextChange {
                    range: None,
                    text: self.text.clone(),
                }],
            }))
        }

        fn goto_position(&mut self, position: ScalarPosition) -> Self::Effect {
            self.goto.set(Some(position));
        }

        fn reset(&mut self, text: &str, _language: Language) -> Self::Effect {
            self.text = text.to_owned();
            self.modified = false;
        }

        fn is_modified(&self) -> bool {
            self.modified
        }

        fn mark_saved(&mut self) {
            self.modified = false;
        }

        fn request_focus(&self) {
            self.focused.set(true);
        }

        fn lose_focus(&mut self) {
            self.focused.set(false);
        }

        fn is_dialog_open(&self) -> bool {
            false
        }
    }

    #[derive(Clone)]
    struct FakeChannel {
        commands: Rc<RefCell<Vec<Command>>>,
        fail: Rc<Cell<bool>>,
    }

    type FakeChannelParts = (FakeChannel, Rc<RefCell<Vec<Command>>>, Rc<Cell<bool>>);

    impl FakeChannel {
        fn new() -> FakeChannelParts {
            let commands = Rc::new(RefCell::new(Vec::new()));
            let fail = Rc::new(Cell::new(false));
            (
                Self {
                    commands: Rc::clone(&commands),
                    fail: Rc::clone(&fail),
                },
                commands,
                fail,
            )
        }
    }

    impl LanguageServiceChannel for FakeChannel {
        type Error = ();

        fn send(&mut self, command: Command) -> Result<(), Self::Error> {
            if self.fail.get() {
                return Err(());
            }
            self.commands.borrow_mut().push(command);
            Ok(())
        }
    }

    fn number<T>(value: u64) -> T
    where
        T: TryFrom<u64>,
        T::Error: std::fmt::Debug,
    {
        T::try_from(value).expect("test identity must be valid")
    }

    fn document_id(value: u8) -> DocumentId {
        DocumentId::try_from([value; 16]).expect("test document identity must be valid")
    }

    fn descriptor(document_id_byte: u8, version: u64, uri: &str) -> DocumentDescriptor {
        DocumentDescriptor {
            document: DocumentRef {
                key: DocumentKey {
                    project: ProjectScope {
                        client_id: number::<ClientId>(1),
                        project_id: number::<ProjectId>(2),
                    },
                    document_id: document_id(document_id_byte),
                },
                view: None,
                version: number::<DocumentVersion>(version),
            },
            uri: uri.to_owned(),
            language: Language::TypeScript,
            kind: DocumentKind::InlineAutomation {
                automation_kind: AutomationKind::Alias,
            },
            analysis_context: number::<AnalysisContextId>(7),
            disk_revision: Some(number::<DiskRevision>(8)),
        }
    }

    fn state(document: DocumentRef) -> DocumentStateIdentity {
        state_with_worker(document, 13)
    }

    fn state_with_worker(document: DocumentRef, worker_generation: u64) -> DocumentStateIdentity {
        DocumentStateIdentity {
            document,
            graph_generation: number::<GraphGeneration>(11),
            service_generation: number::<ServiceGeneration>(12),
            worker_generation: number::<WorkerGeneration>(worker_generation),
        }
    }

    fn event(event: Event) -> EventEnvelope {
        EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            command_sequence: number::<CommandSequence>(1),
            event,
        }
    }

    fn completion_item(id: u64) -> CompletionItem {
        CompletionItem {
            id: CompletionItemId::new(id).unwrap(),
            label: format!("item {id}"),
            detail: None,
            documentation: None,
            kind: CompletionKind::Text,
            deprecated: false,
            filter_text: None,
            sort_text: None,
            insert_text: None,
            insert_text_format: InsertTextFormat::PlainText,
            primary_edit: None,
            additional_edits: Vec::new(),
        }
    }

    #[test]
    fn navigation_invalidates_pending_and_outstanding_completions() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("console"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        editor.pending_completion = Some(PendingCompletion {
            position: Utf16Position {
                line: 0,
                character: 7,
            },
            anchor: SurfacePoint::default(),
        });
        editor
            .outstanding
            .set(RequestKind::Completion, Some(number::<RequestId>(9)));
        editor.results.completion = Some(AcceptedCompletion {
            identity: DocumentResultIdentity {
                state: state(editor.document.document),
                request_id: number::<RequestId>(9),
            },
            result: CompletionResult {
                is_incomplete: false,
                items: vec![completion_item(1)],
            },
            anchor: SurfacePoint::default(),
            selected: 0,
        });

        editor.update(&SurfaceMessage::Navigate);

        assert!(editor.pending_completion.is_none());
        assert!(editor.outstanding.completion.is_none());
        assert!(editor.results.completion.is_none());
    }

    #[test]
    fn passive_editor_messages_preserve_pending_and_visible_completion() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("console"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );

        editor.update(&SurfaceMessage::RequestCompletion(ScalarPosition {
            line: 0,
            character: 7,
        }));
        editor.update(&SurfaceMessage::Passive);
        assert!(editor.pending_completion.is_some());

        let request_id = number::<RequestId>(41);
        assert!(editor.request_pending_completion(request_id));
        let completion = DocumentResult {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id,
            },
            analyzed_uri: Some(editor.document.uri.clone()),
            result: CompletionResult {
                is_incomplete: false,
                items: vec![completion_item(1)],
            },
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Completion(completion))),
            EventDisposition::Applied
        );

        editor.update(&SurfaceMessage::Passive);

        let accepted = editor
            .results
            .completion
            .as_ref()
            .expect("passive messages keep the accepted result visible");
        assert_eq!(accepted.identity.request_id, request_id);
        assert_eq!(accepted.anchor, SurfacePoint { x: 24.0, y: 36.0 });
    }

    #[test]
    fn completion_keyboard_selection_wraps_within_the_eight_visible_rows() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("console"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        editor.service_state = Some(current_state);
        editor.results.completion = Some(AcceptedCompletion {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id: number::<RequestId>(60),
            },
            result: CompletionResult {
                is_incomplete: false,
                items: (1..=10).map(completion_item).collect(),
            },
            anchor: SurfacePoint::default(),
            selected: 0,
        });

        for _ in 0..8 {
            assert!(editor.move_completion_selection(1));
        }
        assert_eq!(editor.results.completion.as_ref().unwrap().selected, 0);
        assert!(editor.move_completion_selection(-1));
        assert_eq!(editor.results.completion.as_ref().unwrap().selected, 7);
    }

    #[test]
    fn delayed_completion_click_cannot_apply_the_same_index_from_a_newer_result() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new(""),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );

        let first_request = number::<RequestId>(70);
        assert!(editor.request_completion(first_request, Utf16Position::default()));
        assert_eq!(
            editor.apply_service_event(&event(Event::Completion(DocumentResult {
                identity: DocumentResultIdentity {
                    state: current_state,
                    request_id: first_request,
                },
                analyzed_uri: Some(editor.document.uri.clone()),
                result: CompletionResult {
                    is_incomplete: false,
                    items: vec![completion_item(1)],
                },
            }))),
            EventDisposition::Applied
        );
        let stale_identity = editor.results.completion.as_ref().unwrap().identity;

        editor.update(&SurfaceMessage::RequestCompletion(ScalarPosition {
            line: 0,
            character: 0,
        }));
        let second_request = number::<RequestId>(71);
        assert!(editor.request_pending_completion(second_request));
        assert_eq!(
            editor.apply_service_event(&event(Event::Completion(DocumentResult {
                identity: DocumentResultIdentity {
                    state: current_state,
                    request_id: second_request,
                },
                analyzed_uri: Some(editor.document.uri.clone()),
                result: CompletionResult {
                    is_incomplete: false,
                    items: vec![completion_item(2)],
                },
            }))),
            EventDisposition::Applied
        );

        assert!(editor.apply_completion(0, stale_identity).is_none());
        assert_eq!(editor.content(), "");
        let current_identity = editor.results.completion.as_ref().unwrap().identity;
        assert!(editor.apply_completion(0, current_identity).is_some());
        assert_eq!(editor.content(), "item 2");
    }

    #[test]
    fn empty_completion_result_does_not_suppress_the_next_hover() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("value"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );
        editor.update(&SurfaceMessage::RequestCompletion(ScalarPosition {
            line: 0,
            character: 5,
        }));
        let request_id = number::<RequestId>(61);
        assert!(editor.request_pending_completion(request_id));
        assert_eq!(
            editor.apply_service_event(&event(Event::Completion(DocumentResult {
                identity: DocumentResultIdentity {
                    state: current_state,
                    request_id,
                },
                analyzed_uri: Some(editor.document.uri.clone()),
                result: CompletionResult {
                    is_incomplete: false,
                    items: Vec::new(),
                },
            }))),
            EventDisposition::Applied
        );
        assert!(editor.results.completion.is_none());

        editor.update(&SurfaceMessage::Hover(HoverUpdate::At(HoverIntent {
            position: ScalarPosition {
                line: 0,
                character: 0,
            },
            anchor: SurfacePoint { x: 8.0, y: 8.0 },
        })));

        assert!(editor.pending_hover.is_some());
    }

    #[test]
    fn dismiss_overlays_invalidates_pending_and_accepted_transient_results() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("value"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        editor.pending_completion = Some(PendingCompletion {
            position: Utf16Position::default(),
            anchor: SurfacePoint::default(),
        });
        editor.results.completion = Some(AcceptedCompletion {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id: number::<RequestId>(62),
            },
            result: CompletionResult {
                is_incomplete: false,
                items: vec![completion_item(1)],
            },
            anchor: SurfacePoint::default(),
            selected: 0,
        });
        editor.hover_position = Some(Utf16Position::default());
        editor.results.hover = Some(AcceptedHover {
            result: HoverResult {
                range: None,
                contents: MarkupContent {
                    kind: MarkupKind::PlainText,
                    value: "docs".to_owned(),
                },
            },
            anchor: SurfacePoint::default(),
        });

        editor.dismiss_overlays();

        assert!(editor.pending_completion.is_none());
        assert!(editor.results.completion.is_none());
        assert!(editor.hover_position.is_none());
        assert!(editor.results.hover.is_none());
        assert_eq!(editor.content(), "value");
    }

    #[test]
    fn hover_debounces_by_word_and_accepts_only_the_exact_request() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("🙂value"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );
        let intent = HoverUpdate::At(HoverIntent {
            position: ScalarPosition {
                line: 0,
                character: 1,
            },
            anchor: SurfacePoint { x: 30.0, y: 18.0 },
        });

        editor.update(&SurfaceMessage::Hover(intent));
        let ready_at = editor.pending_hover.as_ref().unwrap().ready_at;
        editor.update(&SurfaceMessage::Hover(intent));
        assert_eq!(editor.pending_hover.as_ref().unwrap().ready_at, ready_at);
        assert!(!editor.request_pending_hover(
            number::<RequestId>(50),
            ready_at.checked_sub(Duration::from_millis(1)).unwrap()
        ));

        let request_id = number::<RequestId>(51);
        assert!(editor.request_pending_hover(request_id, ready_at));
        let stale = DocumentResult {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id: number::<RequestId>(52),
            },
            analyzed_uri: Some(editor.document.uri.clone()),
            result: Some(HoverResult {
                range: None,
                contents: MarkupContent {
                    kind: MarkupKind::PlainText,
                    value: "wrong".to_owned(),
                },
            }),
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Hover(stale))),
            EventDisposition::Stale
        );
        let accepted = DocumentResult {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id,
            },
            analyzed_uri: Some(editor.document.uri.clone()),
            result: Some(HoverResult {
                range: None,
                contents: MarkupContent {
                    kind: MarkupKind::PlainText,
                    value: "value docs".to_owned(),
                },
            }),
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Hover(accepted))),
            EventDisposition::Applied
        );
        assert_eq!(
            editor.results.hover.as_ref().map(|hover| hover.anchor),
            Some(SurfacePoint { x: 30.0, y: 18.0 })
        );
        assert_eq!(
            editor.hover_position,
            Some(Utf16Position {
                line: 0,
                character: 2,
            })
        );

        editor.update(&SurfaceMessage::Hover(HoverUpdate::Clear));
        assert!(editor.results.hover.is_none());
        assert!(editor.hover_position.is_none());
    }

    #[test]
    fn invalid_matching_completion_and_hover_release_their_request_state() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("value"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );

        let completion_request = number::<RequestId>(80);
        assert!(editor.request_completion(completion_request, Utf16Position::default()));
        let mut item = completion_item(1);
        item.primary_edit = Some(TextEdit {
            range: Utf16Range {
                start: Utf16Position {
                    line: 99,
                    character: 0,
                },
                end: Utf16Position {
                    line: 99,
                    character: 1,
                },
            },
            new_text: "bad".to_owned(),
        });
        assert_eq!(
            editor.apply_service_event(&event(Event::Completion(DocumentResult {
                identity: DocumentResultIdentity {
                    state: current_state,
                    request_id: completion_request,
                },
                analyzed_uri: Some(editor.document.uri.clone()),
                result: CompletionResult {
                    is_incomplete: false,
                    items: vec![item],
                },
            }))),
            EventDisposition::Invalid
        );
        assert!(editor.outstanding.completion.is_none());
        assert!(editor.completion_request_anchor.is_none());

        editor.update(&SurfaceMessage::Hover(HoverUpdate::At(HoverIntent {
            position: ScalarPosition {
                line: 0,
                character: 0,
            },
            anchor: SurfacePoint { x: 8.0, y: 8.0 },
        })));
        let ready_at = editor.pending_hover.as_ref().unwrap().ready_at;
        let hover_request = number::<RequestId>(81);
        assert!(editor.request_pending_hover(hover_request, ready_at));
        assert_eq!(
            editor.apply_service_event(&event(Event::Hover(DocumentResult {
                identity: DocumentResultIdentity {
                    state: current_state,
                    request_id: hover_request,
                },
                analyzed_uri: Some(editor.document.uri.clone()),
                result: Some(HoverResult {
                    range: Some(Utf16Range {
                        start: Utf16Position {
                            line: 99,
                            character: 0,
                        },
                        end: Utf16Position {
                            line: 99,
                            character: 1,
                        },
                    }),
                    contents: MarkupContent {
                        kind: MarkupKind::PlainText,
                        value: "bad".to_owned(),
                    },
                }),
            }))),
            EventDisposition::Invalid
        );
        assert!(editor.outstanding.hover.is_none());
        assert!(editor.hover_request_anchor.is_none());
        assert!(editor.hover_position.is_none());
    }

    #[test]
    fn lifecycle_sends_open_change_save_and_one_close() {
        let (channel, commands, _) = FakeChannel::new();
        {
            let mut editor = AutomationCodeEditor::new(
                FakeSurface::new("let x = 1;"),
                descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
                Some(channel),
            );
            editor.update(&SurfaceMessage::Replace("let x = 2;".to_owned()));
            editor.mark_saved(number::<DiskRevision>(9));
            editor.close();
            editor.close();
            assert!(!editor.is_modified());
        }

        let commands = commands.borrow();
        assert!(
            matches!(&commands[0], Command::OpenDocument(command) if command.text == "let x = 1;")
        );
        assert!(matches!(
            &commands[1],
            Command::ChangeDocument(command)
                if command.document.version.get() == 1 && command.new_version.get() == 2
        ));
        assert!(
            matches!(&commands[2], Command::SaveDocument(command) if command.text == "let x = 2;")
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, Command::CloseDocument(_)))
                .count(),
            1
        );
    }

    #[test]
    fn exact_result_identity_and_uri_are_required() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("const value = 1;"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );
        let request_id = number::<RequestId>(20);
        assert!(editor.request_diagnostics(request_id));

        let result = DocumentResult {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id,
            },
            analyzed_uri: Some(editor.document.uri.clone()),
            result: DiagnosticsResult {
                items: vec![Diagnostic {
                    range: smudgy_script::language_service::Utf16Range::default(),
                    severity: DiagnosticSeverity::Warning,
                    code: None,
                    source: Some("typescript".to_owned()),
                    message: "test".to_owned(),
                    related_information: Vec::new(),
                }],
            },
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Diagnostics(result.clone()))),
            EventDisposition::Applied
        );
        assert_eq!(editor.results().diagnostics.len(), 1);

        let next_request = number::<RequestId>(21);
        assert!(editor.request_diagnostics(next_request));
        let wrong_uri = DocumentResult {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id: next_request,
            },
            analyzed_uri: Some("smudgy-inline:///alias/other.ts".to_owned()),
            result: result.result,
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Diagnostics(wrong_uri))),
            EventDisposition::Stale
        );
    }

    #[test]
    fn formatting_requires_the_exact_state_and_advances_one_authoritative_version() {
        let (channel, commands, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("🙂x\nz"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );
        let request_id = number::<RequestId>(30);
        assert!(editor.request_formatting(
            request_id,
            FormattingOptions {
                tab_size: 2,
                insert_spaces: true,
            },
        ));
        let result = FormattingResult {
            edits: vec![
                TextEdit {
                    range: Utf16Range {
                        start: Utf16Position {
                            line: 0,
                            character: 2,
                        },
                        end: Utf16Position {
                            line: 0,
                            character: 3,
                        },
                    },
                    new_text: "long".to_owned(),
                },
                TextEdit {
                    range: Utf16Range {
                        start: Utf16Position {
                            line: 1,
                            character: 0,
                        },
                        end: Utf16Position {
                            line: 1,
                            character: 0,
                        },
                    },
                    new_text: "  ".to_owned(),
                },
            ],
        };
        let stale_state = DocumentStateIdentity {
            graph_generation: number::<GraphGeneration>(10),
            ..current_state
        };
        let stale = DocumentResult {
            identity: DocumentResultIdentity {
                state: stale_state,
                request_id,
            },
            analyzed_uri: Some(editor.document.uri.clone()),
            result: result.clone(),
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Formatting(stale))),
            EventDisposition::Stale
        );
        assert_eq!(editor.content(), "🙂x\nz");
        assert_eq!(editor.document.document.version.get(), 1);

        let current = DocumentResult {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id,
            },
            analyzed_uri: Some(editor.document.uri.clone()),
            result,
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Formatting(current))),
            EventDisposition::Applied
        );
        assert_eq!(editor.content(), "🙂long\n  z");
        assert_eq!(editor.document.document.version.get(), 2);
        assert!(editor.take_service_edit_applied());
        assert!(!editor.take_service_edit_applied());
        assert!(matches!(
            commands.borrow().last(),
            Some(Command::ChangeDocument(change))
                if change.document == current_state.document
                    && change.new_version == editor.document.document.version
                    && change.changes.changes == vec![TextChange {
                        range: None,
                        text: "🙂long\n  z".to_owned(),
                    }]
        ));
    }

    #[test]
    fn formatting_ranges_must_fit_and_must_not_overlap() {
        let text = "🙂abc";
        let edit = |start, end| TextEdit {
            range: Utf16Range {
                start: Utf16Position {
                    line: 0,
                    character: start,
                },
                end: Utf16Position {
                    line: 0,
                    character: end,
                },
            },
            new_text: "x".to_owned(),
        };

        assert!(simultaneous_text_edits_fit(&[edit(2, 3), edit(3, 5)], text));
        assert!(!simultaneous_text_edits_fit(
            &[edit(2, 4), edit(3, 5)],
            text
        ));
        assert!(!simultaneous_text_edits_fit(&[edit(1, 2)], text));
        assert!(!simultaneous_text_edits_fit(&[edit(99, 99)], text));
        assert!(!simultaneous_text_edits_fit(
            &[edit(3, 3), edit(3, 3)],
            text
        ));
    }

    #[test]
    fn same_document_definition_keeps_its_origin_and_moves_to_a_scalar_column() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("🙂target"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );
        let request_id = number::<RequestId>(31);
        assert!(editor.request_definition(
            request_id,
            Utf16Position {
                line: 0,
                character: 3,
            },
        ));
        let target = DefinitionTarget {
            document_id: current_state.document.key.document_id,
            target_range: Utf16Range {
                start: Utf16Position {
                    line: 0,
                    character: 2,
                },
                end: Utf16Position {
                    line: 0,
                    character: 8,
                },
            },
            target_selection_range: Utf16Range {
                start: Utf16Position {
                    line: 0,
                    character: 2,
                },
                end: Utf16Position {
                    line: 0,
                    character: 8,
                },
            },
            analyzed_uri: Some(editor.document.uri.clone()),
        };
        let definition = DocumentResult {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id,
            },
            analyzed_uri: Some(editor.document.uri.clone()),
            result: DefinitionResult {
                targets: vec![target],
            },
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Definition(definition))),
            EventDisposition::Applied
        );
        let accepted = editor.take_definition().expect("accepted definition");
        assert_eq!(accepted.origin, current_state);
        let selection = accepted.result.targets[0].target_selection_range.start;

        assert!(editor.goto_utf16_position(selection).is_some());
        assert_eq!(
            editor.surface.goto.get(),
            Some(ScalarPosition {
                line: 0,
                character: 1,
            })
        );
    }

    #[test]
    fn edit_invalidates_old_service_state_without_losing_text_on_channel_failure() {
        let (channel, commands, fail) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("old"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let old_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(old_state),
            ))),
            EventDisposition::Applied
        );
        fail.set(true);
        editor.update(&SurfaceMessage::Replace("unsaved edit".to_owned()));

        assert_eq!(editor.content(), "unsaved edit");
        assert!(editor.is_modified());
        assert_eq!(editor.document.document.version.get(), 2);
        assert_eq!(editor.service_status(), ServiceStatus::Unavailable);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentChanged(old_state),
            ))),
            EventDisposition::Stale
        );

        fail.set(false);
        editor.retry_service_sync();
        let commands = commands.borrow();
        assert!(matches!(
            commands.get(1),
            Some(Command::CloseDocument(command)) if command.document == old_state.document
        ));
        assert!(matches!(
            commands.get(2),
            Some(Command::OpenDocument(command))
                if command.descriptor.document == editor.document.document
                    && command.text == "unsaved edit"
        ));
    }

    #[test]
    fn rebind_closes_old_identity_before_opening_new_identity() {
        let (channel, commands, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("old"),
            descriptor(3, 1, "smudgy-inline:///alias/old.ts"),
            Some(channel),
        );
        editor.rebind(descriptor(4, 1, "smudgy-inline:///alias/new.ts"), "new");

        let commands = commands.borrow();
        assert!(matches!(
            &commands[1],
            Command::CloseDocument(command) if command.document.key.document_id == document_id(3)
        ));
        assert!(matches!(
            &commands[2],
            Command::OpenDocument(command)
                if command.descriptor.document.key.document_id == document_id(4)
        ));
        assert_eq!(editor.content(), "new");
        assert!(!editor.is_modified());
    }

    #[test]
    fn restart_replays_current_snapshot_and_fences_old_worker_events() {
        let (channel, commands, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("unsaved"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let old_state = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(old_state),
            ))),
            EventDisposition::Applied
        );

        assert_eq!(
            editor.apply_service_event(&event(Event::WorkerRestarted {
                worker_generation: number::<WorkerGeneration>(14),
            })),
            EventDisposition::Applied
        );
        assert!(matches!(
            commands.borrow().last(),
            Some(Command::OpenDocument(command)) if command.text == "unsaved"
        ));
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(old_state),
            ))),
            EventDisposition::Stale
        );
        let current_state = state_with_worker(editor.document.document, 14);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Applied
        );
    }

    #[test]
    fn degraded_status_is_not_overridden_by_a_late_document_ack() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("text"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        let project = ProjectStateIdentity {
            project: current_state.document.key.project,
            graph_generation: current_state.graph_generation,
            service_generation: current_state.service_generation,
            worker_generation: current_state.worker_generation,
        };
        editor.apply_service_event(&event(Event::ProjectStatus(ProjectStatusEvent {
            identity: project,
            status: ProjectStatus::Degraded {
                code: "fixture".to_owned(),
                message: "fixture".to_owned(),
            },
        })));
        editor.apply_service_event(&event(Event::StateAcknowledged(
            AcknowledgedState::DocumentOpened(current_state),
        )));
        editor.apply_service_event(&event(Event::StateAcknowledged(
            AcknowledgedState::DocumentSaved(current_state),
        )));

        assert_eq!(editor.service_status(), ServiceStatus::Unavailable);
        assert!(!editor.request_diagnostics(number::<RequestId>(99)));
    }

    #[test]
    fn document_acknowledgements_advance_the_cached_project_generation() {
        let (channel, _, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("let value = 1;"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let project = editor.document.document.key.project;
        let worker_generation = number::<WorkerGeneration>(13);
        assert_eq!(
            editor.apply_service_event(&event(Event::ProjectStatus(ProjectStatusEvent {
                identity: ProjectStateIdentity {
                    project,
                    graph_generation: number::<GraphGeneration>(11),
                    service_generation: number::<ServiceGeneration>(12),
                    worker_generation,
                },
                status: ProjectStatus::Ready,
            }))),
            EventDisposition::Applied
        );

        let opened = DocumentStateIdentity {
            document: editor.document.document,
            graph_generation: number::<GraphGeneration>(11),
            service_generation: number::<ServiceGeneration>(13),
            worker_generation,
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(opened),
            ))),
            EventDisposition::Applied
        );
        assert_eq!(editor.service_status(), ServiceStatus::Ready);

        editor.update(&SurfaceMessage::Replace("let value = 2;".to_owned()));
        let changed = DocumentStateIdentity {
            document: editor.document.document,
            service_generation: number::<ServiceGeneration>(14),
            ..opened
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentChanged(changed),
            ))),
            EventDisposition::Applied
        );
        assert!(editor.request_diagnostics(number::<RequestId>(99)));
    }

    #[test]
    fn project_refresh_advances_the_open_overlay_fence() {
        let (channel, commands, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("let value = 1;"),
            descriptor(3, 1, "smudgy-project:///modules/main.ts"),
            Some(channel),
        );
        let opened = state(editor.document.document);
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(opened),
            ))),
            EventDisposition::Applied
        );
        let refreshed = ProjectStateIdentity {
            project: opened.document.key.project,
            graph_generation: number::<GraphGeneration>(12),
            service_generation: number::<ServiceGeneration>(13),
            worker_generation: opened.worker_generation,
        };
        editor.status = ServiceStatus::Unavailable;
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::ProjectRefreshed(refreshed),
            ))),
            EventDisposition::Applied
        );
        assert_eq!(editor.service_status(), ServiceStatus::Ready);
        assert!(editor.request_diagnostics(number::<RequestId>(55)));
        assert!(matches!(
            commands.borrow().last(),
            Some(Command::RequestDiagnostics(request))
                if request.identity.state.graph_generation == refreshed.graph_generation
                    && request.identity.state.service_generation == refreshed.service_generation
        ));
    }

    #[test]
    fn invalid_delta_resyncs_with_a_full_current_snapshot() {
        let (channel, commands, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("old"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        editor.surface.text = "current".to_owned();
        editor.document_changed(DocumentChanges {
            changes: vec![
                TextChange {
                    range: None,
                    text: "current".to_owned(),
                };
                MAX_DOCUMENT_CHANGES + 1
            ],
        });

        let commands = commands.borrow();
        assert!(matches!(commands.get(1), Some(Command::CloseDocument(_))));
        assert!(matches!(
            commands.get(2),
            Some(Command::OpenDocument(command))
                if command.text == "current" && command.descriptor.document.version.get() == 2
        ));
    }

    #[test]
    fn out_of_bounds_results_and_post_close_events_are_rejected() {
        let (channel, commands, _) = FakeChannel::new();
        let mut editor = AutomationCodeEditor::new(
            FakeSurface::new("text"),
            descriptor(3, 1, "smudgy-inline:///alias/test.ts"),
            Some(channel),
        );
        let current_state = state(editor.document.document);
        editor.apply_service_event(&event(Event::StateAcknowledged(
            AcknowledgedState::DocumentOpened(current_state),
        )));
        let request_id = number::<RequestId>(20);
        assert!(editor.request_diagnostics(request_id));
        let invalid = DocumentResult {
            identity: DocumentResultIdentity {
                state: current_state,
                request_id,
            },
            analyzed_uri: Some(editor.document.uri.clone()),
            result: DiagnosticsResult {
                items: vec![Diagnostic {
                    range: Utf16Range {
                        start: Utf16Position {
                            line: 99,
                            character: 0,
                        },
                        end: Utf16Position {
                            line: 99,
                            character: 1,
                        },
                    },
                    severity: DiagnosticSeverity::Warning,
                    code: None,
                    source: None,
                    message: "invalid".to_owned(),
                    related_information: Vec::new(),
                }],
            },
        };
        assert_eq!(
            editor.apply_service_event(&event(Event::Diagnostics(invalid))),
            EventDisposition::Invalid
        );

        editor.close();
        let command_count = commands.borrow().len();
        editor.update(&SurfaceMessage::Replace("after close".to_owned()));
        editor.mark_saved(number::<DiskRevision>(100));
        assert_eq!(
            editor.apply_service_event(&event(Event::StateAcknowledged(
                AcknowledgedState::DocumentOpened(current_state),
            ))),
            EventDisposition::Stale
        );
        assert_eq!(commands.borrow().len(), command_count);
    }

    #[test]
    fn inline_dynamic_import_uses_ambient_types_without_attaching_module_sources() {
        let mut window = super::super::AutomationsWindow::new(
            iced::window::Id::unique(),
            "inline-dynamic-import-test".to_owned(),
            crate::cloud_account::test_handles(),
            smudgy_core::session::SessionId::from(1),
        );
        window.modules = vec![smudgy_core::models::modules::ModuleFile {
            subpath: "must-not-join-inline.ts".to_owned(),
            path: std::path::PathBuf::from("must-not-join-inline.ts"),
        }];
        let sources = window.language_project_sources(&LanguageProjectContext::Inline);
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].uri, "smudgy-project:///inline/context.d.ts",
            "classic inline bodies must not acquire a false relative module graph"
        );
        assert_eq!(
            sources[0].text,
            smudgy_core::models::script_typings::language_service_inline_bridge(),
            "the inline-only declarations must be installed directly in the inline project"
        );
        assert!(
            !sources[0].text.contains("<reference path"),
            "the inline project must not bypass its scope through a global declaration path"
        );

        let _ = window.bind_code_editor(
            "void import(\"smudgy:core\").then((core) => core.createAlias);\n",
            Language::TypeScript,
            CodeDocument::Alias,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let _ = window.poll_language_service();
            let analyzed = window.code_editor.as_ref().is_some_and(|editor| {
                editor.service_state.is_some()
                    && editor.outstanding.diagnostics.is_none()
                    && editor.service_status() == ServiceStatus::Ready
            });
            if analyzed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "inline dynamic-import diagnostics timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !window
                .code_editor
                .as_ref()
                .unwrap()
                .results()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2307))),
            "managed ambient declarations must type supported absolute dynamic imports"
        );
    }

    #[test]
    fn oversized_saved_module_is_rejected_before_graph_read() {
        let directory = tempfile::tempdir().expect("temporary module directory");
        let path = directory.path().join("oversized.ts");
        let file = std::fs::File::create(&path).expect("create sparse oversized module");
        file.set_len(u64::try_from(MAX_DOCUMENT_BYTES + 1).expect("wire size fits u64"))
            .expect("size sparse oversized module");

        let mut window = super::super::AutomationsWindow::new(
            iced::window::Id::unique(),
            "oversized-language-graph-test".to_owned(),
            crate::cloud_account::test_handles(),
            smudgy_core::session::SessionId::from(1),
        );
        window.modules = vec![smudgy_core::models::modules::ModuleFile {
            subpath: "oversized.ts".to_owned(),
            path,
        }];

        assert!(
            window
                .language_project_sources(&LanguageProjectContext::Modules)
                .is_empty(),
            "per-file graph cap must be applied from metadata before reading the file"
        );
    }

    #[test]
    fn cross_file_definition_is_mount_and_graph_fenced_and_uses_the_dirty_guard() {
        let directory = tempfile::tempdir().expect("temporary module directory");
        let origin_path = directory.path().join("origin.ts");
        let target_path = directory.path().join("target.ts");
        std::fs::write(&origin_path, "target();\n").expect("seed origin module");
        std::fs::write(&target_path, "🙂export const target = 1;\n").expect("seed target module");

        let mut window = super::super::AutomationsWindow::new(
            iced::window::Id::unique(),
            "definition-navigation-test".to_owned(),
            crate::cloud_account::test_handles(),
            smudgy_core::session::SessionId::from(1),
        );
        window.modules = vec![
            smudgy_core::models::modules::ModuleFile {
                subpath: "origin.ts".to_owned(),
                path: origin_path.clone(),
            },
            smudgy_core::models::modules::ModuleFile {
                subpath: "target.ts".to_owned(),
                path: target_path,
            },
        ];
        window.selection = super::super::Selection::Module("origin.ts".to_owned());
        window.pane = super::super::Pane::Module(super::super::ModuleState {
            mode: super::super::ModuleMode::View,
            subpath: "origin.ts".to_owned(),
            path: Some(origin_path),
            name: String::new(),
            error: None,
        });
        window.language_project_context = Some(LanguageProjectContext::Modules);
        window.language_project_target_context = Some(LanguageProjectContext::Modules);
        let _ = window.bind_code_editor(
            "target();\n",
            Language::TypeScript,
            CodeDocument::StandaloneModule,
        );
        let target_id =
            window.language_source_id(LanguageSourceKey::Module("target.ts".to_owned()));
        let origin_document = window
            .code_editor
            .as_ref()
            .expect("origin editor")
            .document()
            .document;
        let origin = state(origin_document);
        let editor = window.code_editor.as_mut().expect("origin editor");
        editor.service_state = Some(origin);
        editor.worker_generation = Some(origin.worker_generation);
        editor.project_state = Some(ProjectStateIdentity {
            project: origin.document.key.project,
            graph_generation: origin.graph_generation,
            service_generation: origin.service_generation,
            worker_generation: origin.worker_generation,
        });
        editor.status = ServiceStatus::Ready;
        let navigation = DefinitionNavigation {
            origin,
            origin_mount_generation: window.code_editor_mount_generation,
            target: DefinitionTarget {
                document_id: target_id,
                target_range: Utf16Range {
                    start: Utf16Position {
                        line: 0,
                        character: 2,
                    },
                    end: Utf16Position {
                        line: 0,
                        character: 8,
                    },
                },
                target_selection_range: Utf16Range {
                    start: Utf16Position {
                        line: 0,
                        character: 2,
                    },
                    end: Utf16Position {
                        line: 0,
                        character: 8,
                    },
                },
                analyzed_uri: Some("smudgy-project:///modules/target.ts".to_owned()),
            },
        };

        let mut stale_mount = navigation.clone();
        stale_mount.origin_mount_generation = stale_mount.origin_mount_generation.saturating_add(1);
        let _ = window.navigate_code_definition(stale_mount);
        assert!(matches!(
            window.selection,
            super::super::Selection::Module(ref subpath) if subpath == "origin.ts"
        ));

        let mut stale_graph = navigation.clone();
        stale_graph.origin.graph_generation = number::<GraphGeneration>(10);
        let _ = window.navigate_code_definition(stale_graph);
        assert!(matches!(
            window.selection,
            super::super::Selection::Module(ref subpath) if subpath == "origin.ts"
        ));

        window.dirty = true;
        let _ = window.update(super::super::Message::NavigateCodeDefinition(
            navigation.clone(),
        ));
        assert!(matches!(
            window.pending_nav.as_deref(),
            Some(super::super::Message::NavigateCodeDefinition(_))
        ));
        assert_eq!(window.code_editor_text(), "target();\n");

        window
            .code_editor
            .as_mut()
            .expect("origin editor")
            .service_state
            .as_mut()
            .expect("origin service state")
            .graph_generation = number::<GraphGeneration>(10);
        let _ = window.update(super::super::Message::ConfirmDiscardNav);
        assert!(
            window.dirty,
            "a stale confirmed jump must retain the dirty guard"
        );
        assert!(matches!(
            window.selection,
            super::super::Selection::Module(ref subpath) if subpath == "origin.ts"
        ));
        assert_eq!(window.code_editor_text(), "target();\n");

        window
            .code_editor
            .as_mut()
            .expect("origin editor")
            .service_state = Some(navigation.origin);
        let _ = window.update(super::super::Message::NavigateCodeDefinition(navigation));
        let _ = window.update(super::super::Message::ConfirmDiscardNav);
        assert!(!window.dirty);
        assert!(matches!(
            window.selection,
            super::super::Selection::Module(ref subpath) if subpath == "target.ts"
        ));
        assert_eq!(window.code_editor_text(), "🙂export const target = 1;\n");
        assert_eq!(
            window
                .code_editor
                .as_ref()
                .expect("target editor")
                .surface
                .cursor_position(),
            (0, 1),
            "the UTF-16 target after the emoji must become the editor's scalar column"
        );
    }

    #[test]
    fn project_context_commits_only_on_its_exact_refresh_ack_and_retries_failure() {
        let mut window = super::super::AutomationsWindow::new(
            iced::window::Id::unique(),
            "project-refresh-fence-test".to_owned(),
            crate::cloud_account::test_handles(),
            smudgy_core::session::SessionId::from(1),
        );
        let previous = LanguageProjectContext::Modules;
        let next = LanguageProjectContext::OwnedPackage("next".to_owned());
        let graph_generation = number::<GraphGeneration>(20);
        let command_sequence = number::<CommandSequence>(21);
        let worker_generation = number::<WorkerGeneration>(22);
        window.language_project_context = Some(previous.clone());
        window.language_project_target_context = Some(next.clone());
        window.pending_language_project_refresh = Some(PendingLanguageProjectRefresh {
            context: next.clone(),
            graph_generation,
            command_sequence,
            retries_remaining: 1,
        });

        let project = ProjectStateIdentity {
            project: language_service_project(),
            graph_generation,
            service_generation: number::<ServiceGeneration>(23),
            worker_generation,
        };
        let wrong_sequence = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            command_sequence: number::<CommandSequence>(19),
            event: Event::StateAcknowledged(AcknowledgedState::ProjectRefreshed(project)),
        };
        assert!(
            window
                .observe_language_project_event(&wrong_sequence)
                .is_none()
        );
        assert_eq!(window.language_project_context, Some(previous.clone()));
        assert!(window.pending_language_project_refresh.is_some());

        let failed = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            command_sequence,
            event: Event::RequestFailed(smudgy_script::language_service::RequestFailure {
                scope: FailureScope::Project(ProjectStateIdentity {
                    graph_generation: number::<GraphGeneration>(19),
                    ..project
                }),
                code: "fixture".to_owned(),
                retryable: true,
                user_message: "retry".to_owned(),
                log_detail: None,
            }),
        };
        let retry = window
            .observe_language_project_event(&failed)
            .expect("matching retryable refresh failure");
        assert_eq!(retry.context, next);
        assert_eq!(retry.retries_remaining, 0);
        assert_eq!(window.language_project_context, Some(previous));
        assert!(window.pending_language_project_refresh.is_none());

        let retry_sequence = number::<CommandSequence>(24);
        window.pending_language_project_refresh = Some(PendingLanguageProjectRefresh {
            context: retry.context.clone(),
            graph_generation,
            command_sequence: retry_sequence,
            retries_remaining: retry.retries_remaining,
        });
        let acknowledged = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            command_sequence: retry_sequence,
            event: Event::StateAcknowledged(AcknowledgedState::ProjectRefreshed(project)),
        };
        assert!(
            window
                .observe_language_project_event(&acknowledged)
                .is_none()
        );
        assert_eq!(window.language_project_context, Some(retry.context));
        assert!(window.pending_language_project_refresh.is_none());
    }
}
