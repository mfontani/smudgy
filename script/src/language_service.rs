//! Transport-neutral protocol types for Smudgy's language-service worker.
//!
//! The worker implementation and its process transport intentionally live outside this
//! module. These data types make the parent/worker boundary explicit, versioned, and
//! independently testable.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::ops::Range;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Current language-service protocol version.
pub const PROTOCOL_VERSION: u16 = 1;
/// Largest encoded JSON payload accepted by the eventual framed transport.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Largest editable document body.
pub const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;
/// Largest number of source files in one atomic project refresh.
pub const MAX_PROJECT_SOURCE_FILES: usize = 2_048;
/// Largest aggregate decoded source text in one atomic project refresh.
pub const MAX_PROJECT_SOURCE_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// Largest protocol URI.
pub const MAX_URI_BYTES: usize = 32 * 1024;
/// Largest number of ordered changes in one document update.
pub const MAX_DOCUMENT_CHANGES: usize = 256;
/// Largest diagnostic result for one document.
pub const MAX_DIAGNOSTICS_PER_DOCUMENT: usize = 2_000;
/// Largest aggregate related-information list in one diagnostic result.
pub const MAX_DIAGNOSTIC_RELATED_INFORMATION: usize = 2_000;
/// Largest completion list accepted from the worker.
pub const MAX_COMPLETION_ITEMS: usize = 500;
/// Largest aggregate additional-edit list in one completion result.
pub const MAX_COMPLETION_ADDITIONAL_EDITS: usize = 256;
/// Largest definition result.
pub const MAX_DEFINITION_TARGETS: usize = 64;
/// Largest hover or documentation payload.
pub const MAX_HOVER_BYTES: usize = 256 * 1024;
/// Largest formatting edit list.
pub const MAX_FORMATTING_EDITS: usize = 4_096;
/// Largest aggregate formatting replacement text.
pub const MAX_FORMATTING_REPLACEMENT_BYTES: usize = 4 * 1024 * 1024;
/// Largest non-formatting result metadata payload retained after decoding.
pub const MAX_RESULT_METADATA_BYTES: usize = 1024 * 1024;
/// Largest logical line count in an editable document.
pub const MAX_DOCUMENT_LINES: usize = 100_000;
/// Largest Unicode-scalar count on one editable logical line.
pub const MAX_SCALARS_PER_LINE: usize = 100_000;

const MAX_WIRE_INTEGER: u64 = (1_u64 << 53) - 1;

macro_rules! wire_number {
    ($name:ident) => {
        #[doc = concat!("A validated wire value for ", stringify!($name), ".")]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            /// Constructs a nonzero value that round-trips exactly through JavaScript.
            #[must_use]
            pub const fn new(value: u64) -> Option<Self> {
                if value > 0 && value <= MAX_WIRE_INTEGER {
                    Some(Self(value))
                } else {
                    None
                }
            }

            /// Returns the underlying wire integer.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            fn validate(self) -> Result<(), ProtocolError> {
                validate_wire_number(stringify!($name), self.0)
            }
        }

        impl TryFrom<u64> for $name {
            type Error = ProtocolError;

            fn try_from(value: u64) -> Result<Self, Self::Error> {
                validate_wire_number(stringify!($name), value)?;
                Ok(Self(value))
            }
        }
    };
}

wire_number!(ClientId);
wire_number!(ProjectId);
wire_number!(ViewId);
wire_number!(AnalysisContextId);
wire_number!(CompletionItemId);
wire_number!(CommandSequence);
wire_number!(RequestId);
wire_number!(DocumentVersion);
wire_number!(ViewGeneration);
wire_number!(GraphGeneration);
wire_number!(ServiceGeneration);
wire_number!(WorkerGeneration);
wire_number!(DiskRevision);

/// Opaque UUID used as the sole routing authority for one authoring document.
///
/// The wire form is a canonical, hyphenated UUID string. The bytes have no ordering or
/// path semantics and must come from the core-owned document identity registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId([u8; 16]);

impl DocumentId {
    /// Constructs a document ID from non-nil UUID bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Option<Self> {
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != 0 {
                return Some(Self(bytes));
            }
            index += 1;
        }
        None
    }

    /// Returns the UUID bytes without assigning meaning to them.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    fn validate(self) -> Result<(), ProtocolError> {
        if Self::from_bytes(self.0).is_some() {
            Ok(())
        } else {
            Err(ProtocolError::InvalidDocumentId)
        }
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.0;
        write!(
            formatter,
            "{0:02x}{1:02x}{2:02x}{3:02x}-{4:02x}{5:02x}-{6:02x}{7:02x}-{8:02x}{9:02x}-{10:02x}{11:02x}{12:02x}{13:02x}{14:02x}{15:02x}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15],
        )
    }
}

impl FromStr for DocumentId {
    type Err = ProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_document_id(value)
    }
}

impl TryFrom<[u8; 16]> for DocumentId {
    type Error = ProtocolError;

    fn try_from(bytes: [u8; 16]) -> Result<Self, Self::Error> {
        Self::from_bytes(bytes).ok_or(ProtocolError::InvalidDocumentId)
    }
}

impl Serialize for DocumentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DocumentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DocumentIdVisitor;

        impl<'de> Visitor<'de> for DocumentIdVisitor {
            type Value = DocumentId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical non-nil UUID string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                DocumentId::from_str(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(DocumentIdVisitor)
    }
}

/// A client/project pair. Neither value contains a filesystem path or display name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectScope {
    pub client_id: ClientId,
    pub project_id: ProjectId,
}

impl ProjectScope {
    fn validate(self) -> Result<(), ProtocolError> {
        self.client_id.validate()?;
        self.project_id.validate()
    }
}

/// A generation-fenced view attachment within one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ViewRef {
    pub view_id: ViewId,
    pub generation: ViewGeneration,
}

impl ViewRef {
    fn validate(self) -> Result<(), ProtocolError> {
        self.view_id.validate()?;
        self.generation.validate()
    }
}

/// The routing identity for one document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentKey {
    pub project: ProjectScope,
    pub document_id: DocumentId,
}

impl DocumentKey {
    fn validate(self) -> Result<(), ProtocolError> {
        self.project.validate()?;
        self.document_id.validate()
    }
}

/// A document at an exact parent-authored version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentRef {
    pub key: DocumentKey,
    pub view: Option<ViewRef>,
    pub version: DocumentVersion,
}

impl DocumentRef {
    fn validate(self) -> Result<(), ProtocolError> {
        self.key.validate()?;
        if let Some(view) = self.view {
            view.validate()?;
        }
        self.version.validate()
    }
}

/// Every state dimension needed to reject a stale document result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentStateIdentity {
    pub document: DocumentRef,
    pub graph_generation: GraphGeneration,
    pub service_generation: ServiceGeneration,
    pub worker_generation: WorkerGeneration,
}

impl DocumentStateIdentity {
    fn validate(self) -> Result<(), ProtocolError> {
        self.document.validate()?;
        self.graph_generation.validate()?;
        self.service_generation.validate()?;
        self.worker_generation.validate()
    }
}

/// A result identity, including the exact request which produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentResultIdentity {
    pub state: DocumentStateIdentity,
    pub request_id: RequestId,
}

impl DocumentResultIdentity {
    /// Returns true only for the complete live state and outstanding request.
    #[must_use]
    pub fn is_current_for(
        &self,
        state: &DocumentStateIdentity,
        outstanding_request: RequestId,
    ) -> bool {
        self.state == *state && self.request_id == outstanding_request
    }

    fn validate(self) -> Result<(), ProtocolError> {
        self.state.validate()?;
        self.request_id.validate()
    }
}

/// Project state echoed by project lifecycle events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectStateIdentity {
    pub project: ProjectScope,
    pub graph_generation: GraphGeneration,
    pub service_generation: ServiceGeneration,
    pub worker_generation: WorkerGeneration,
}

impl ProjectStateIdentity {
    fn validate(self) -> Result<(), ProtocolError> {
        self.project.validate()?;
        self.graph_generation.validate()?;
        self.service_generation.validate()?;
        self.worker_generation.validate()
    }
}

/// Zero-based UTF-16 line/character coordinates used by TypeScript and LSP.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Utf16Position {
    pub line: u32,
    pub character: u32,
}

impl Utf16Position {
    /// Resolves this position to a UTF-8 byte offset.
    ///
    /// The conversion rejects positions beyond a line and positions in the middle of a
    /// surrogate pair.
    pub fn to_byte_offset(self, text: &str) -> Result<usize, ProtocolError> {
        let (start, end) = line_bounds(text, self.line)?;
        let target = usize::try_from(self.character)
            .map_err(|_| ProtocolError::PositionOutOfBounds(self))?;
        let mut utf16_column = 0_usize;

        if target == 0 {
            return Ok(start);
        }

        for (relative_offset, character) in text[start..end].char_indices() {
            let next_column = utf16_column + character.len_utf16();
            if target == utf16_column {
                return Ok(start + relative_offset);
            }
            if target < next_column {
                return Err(ProtocolError::PositionInsideSurrogatePair(self));
            }
            utf16_column = next_column;
        }

        if target == utf16_column {
            Ok(end)
        } else {
            Err(ProtocolError::PositionOutOfBounds(self))
        }
    }
}

/// A half-open range in UTF-16 coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Utf16Range {
    pub start: Utf16Position,
    pub end: Utf16Position,
}

impl Utf16Range {
    /// Validates range ordering without requiring document text.
    pub fn validate(self) -> Result<(), ProtocolError> {
        if self.start <= self.end {
            Ok(())
        } else {
            Err(ProtocolError::InvalidRange(self))
        }
    }

    /// Resolves this range to a half-open UTF-8 byte range in the supplied text.
    pub fn to_byte_range(self, text: &str) -> Result<Range<usize>, ProtocolError> {
        self.validate()?;
        let start = self.start.to_byte_offset(text)?;
        let end = self.end.to_byte_offset(text)?;
        if start <= end {
            Ok(start..end)
        } else {
            Err(ProtocolError::InvalidRange(self))
        }
    }

    fn contains(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// Language mode of an opened document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Language {
    JavaScript,
    TypeScript,
    JavaScriptReact,
    TypeScriptReact,
    Json,
    PlainText,
}

/// One immutable declaration supplied when a language-service worker starts.
///
/// Libraries live outside the editable document table. Root libraries participate in
/// every Program; non-root libraries are available to TypeScript's reference resolver.
/// Borrowed text lets callers pass declarations embedded in the binary without copying
/// them on the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageServiceLibrary {
    pub file_name: String,
    pub text: Cow<'static, str>,
    pub is_root: bool,
}

/// Inline automation family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AutomationKind {
    Alias,
    Trigger,
    Hotkey,
}

/// Semantic origin and editability class of a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DocumentKind {
    InlineAutomation { automation_kind: AutomationKind },
    StandaloneModule,
    OwnedPackage,
    Dependency,
    Generated,
    ReadOnlyPreview,
}

/// Immutable metadata for opening a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentDescriptor {
    pub document: DocumentRef,
    pub uri: String,
    pub language: Language,
    pub kind: DocumentKind,
    pub analysis_context: AnalysisContextId,
    pub disk_revision: Option<DiskRevision>,
}

impl DocumentDescriptor {
    fn validate(&self) -> Result<(), ProtocolError> {
        self.document.validate()?;
        validate_uri(&self.uri)?;
        self.analysis_context.validate()?;
        if let Some(revision) = self.disk_revision {
            revision.validate()?;
        }
        Ok(())
    }
}

/// One ordered document-content change. A missing range replaces the full document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextChange {
    pub range: Option<Utf16Range>,
    pub text: String,
}

/// A sequence of LSP-style changes, each relative to the result of its predecessor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentChanges {
    pub changes: Vec<TextChange>,
}

impl DocumentChanges {
    /// Checks structural count and replacement-byte bounds without document content.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_count("document changes", self.changes.len(), MAX_DOCUMENT_CHANGES)?;
        let mut replacement_bytes = 0_usize;
        for change in &self.changes {
            if let Some(range) = change.range {
                range.validate()?;
            }
            add_bytes(
                "document change text",
                &mut replacement_bytes,
                change.text.len(),
                MAX_DOCUMENT_BYTES,
            )?;
        }
        Ok(())
    }

    /// Applies and validates the ordered changes against exact current text.
    pub fn apply_to(&self, current_text: &str) -> Result<String, ProtocolError> {
        self.validate()?;
        validate_document_text(current_text)?;
        let mut result = current_text.to_owned();

        for change in &self.changes {
            if let Some(range) = change.range {
                let byte_range = range.to_byte_range(&result)?;
                let prospective_len = result
                    .len()
                    .checked_sub(byte_range.len())
                    .and_then(|length| length.checked_add(change.text.len()))
                    .ok_or(ProtocolError::SizeOverflow("document text"))?;
                validate_bytes("document text", prospective_len, MAX_DOCUMENT_BYTES)?;
                result.replace_range(byte_range, &change.text);
            } else {
                result.clear();
                result.push_str(&change.text);
            }
            validate_document_text(&result)?;
        }

        Ok(result)
    }
}

/// A simultaneous same-document replacement returned by the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextEdit {
    pub range: Utf16Range,
    pub new_text: String,
}

/// Formatting preferences needed by TypeScript's formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormattingOptions {
    pub tab_size: u8,
    pub insert_spaces: bool,
}

impl FormattingOptions {
    fn validate(self) -> Result<(), ProtocolError> {
        if (1..=16).contains(&self.tab_size) {
            Ok(())
        } else {
            Err(ProtocolError::InvalidValue("formatting tab size"))
        }
    }
}

/// Opens an isolated project table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenProject {
    pub project: ProjectScope,
}

/// One immutable source in an atomic project-graph snapshot.
///
/// Project sources are the saved base beneath any separately opened editable
/// document. The opaque document ID is routing authority; the URI supplies only
/// analysis topology and an integrity assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectSource {
    pub document_id: DocumentId,
    pub uri: String,
    pub language: Language,
    pub kind: DocumentKind,
    pub text: String,
}

impl ProjectSource {
    fn validate(&self) -> Result<(), ProtocolError> {
        self.document_id.validate()?;
        validate_uri(&self.uri)?;
        validate_document_text(&self.text)
    }
}

/// Atomically replaces the saved sources for a newer project graph generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RefreshProject {
    pub project: ProjectScope,
    pub graph_generation: GraphGeneration,
    pub sources: Vec<ProjectSource>,
}

impl RefreshProject {
    fn validate(&self) -> Result<(), ProtocolError> {
        self.project.validate()?;
        self.graph_generation.validate()?;
        validate_count(
            "project sources",
            self.sources.len(),
            MAX_PROJECT_SOURCE_FILES,
        )?;

        let mut source_bytes = 0_usize;
        let mut document_ids = HashSet::with_capacity(self.sources.len());
        let mut uris = HashSet::with_capacity(self.sources.len());
        for source in &self.sources {
            source.validate()?;
            add_bytes(
                "project source text",
                &mut source_bytes,
                source.text.len(),
                MAX_PROJECT_SOURCE_TEXT_BYTES,
            )?;
            if !document_ids.insert(source.document_id) {
                return Err(ProtocolError::InvalidValue(
                    "duplicate project source document ID",
                ));
            }
            if !uris.insert(source.uri.as_str()) {
                return Err(ProtocolError::InvalidValue("duplicate project source URI"));
            }
        }
        Ok(())
    }
}

/// Closes an isolated project table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloseProject {
    pub project: ProjectScope,
}

/// Attaches a view generation to a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachView {
    pub project: ProjectScope,
    pub view: ViewRef,
}

/// Detaches an exact view generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DetachView {
    pub project: ProjectScope,
    pub view: ViewRef,
}

/// Opens one authoritative document snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenDocument {
    pub descriptor: DocumentDescriptor,
    pub text: String,
}

/// Changes a document from an exact base version to a newer version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeDocument {
    pub document: DocumentRef,
    pub new_version: DocumentVersion,
    pub changes: DocumentChanges,
}

impl ChangeDocument {
    /// Applies this command to exact current text after checking version monotonicity.
    pub fn apply_to(&self, current_text: &str) -> Result<String, ProtocolError> {
        ensure_newer_version(self.document.version, self.new_version)?;
        self.changes.apply_to(current_text)
    }
}

/// Marks exact document text as saved at a disk revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SaveDocument {
    pub document: DocumentRef,
    pub text: String,
    pub disk_revision: DiskRevision,
}

/// Closes an exact document version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloseDocument {
    pub document: DocumentRef,
}

/// Full identity for a document request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentRequest {
    pub identity: DocumentResultIdentity,
}

/// Full identity plus a UTF-16 cursor position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PositionRequest {
    pub identity: DocumentResultIdentity,
    pub position: Utf16Position,
}

/// Full identity plus formatter preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormattingRequest {
    pub identity: DocumentResultIdentity,
    pub options: FormattingOptions,
}

/// Cancels one request in one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelRequest {
    pub project: ProjectScope,
    pub request_id: RequestId,
}

/// Parent-to-worker command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum Command {
    OpenProject(OpenProject),
    RefreshProject(RefreshProject),
    CloseProject(CloseProject),
    AttachView(AttachView),
    DetachView(DetachView),
    OpenDocument(OpenDocument),
    ChangeDocument(ChangeDocument),
    SaveDocument(SaveDocument),
    CloseDocument(CloseDocument),
    RequestDiagnostics(DocumentRequest),
    RequestCompletion(PositionRequest),
    RequestHover(PositionRequest),
    RequestDefinition(PositionRequest),
    RequestFormatting(FormattingRequest),
    Cancel(CancelRequest),
    Shutdown,
}

/// Versioned parent-to-worker command envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandEnvelope {
    pub protocol_version: u16,
    pub command_sequence: CommandSequence,
    pub command: Command,
}

/// Shared validation entry point for protocol messages.
pub trait Validate {
    /// Checks identity, range, count, and byte invariants which do not require external state.
    fn validate(&self) -> Result<(), ProtocolError>;
}

impl Validate for CommandEnvelope {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        self.command_sequence.validate()?;
        match &self.command {
            Command::OpenProject(command) => command.project.validate(),
            Command::RefreshProject(command) => command.validate(),
            Command::CloseProject(command) => command.project.validate(),
            Command::AttachView(command) => {
                command.project.validate()?;
                command.view.validate()
            }
            Command::DetachView(command) => {
                command.project.validate()?;
                command.view.validate()
            }
            Command::OpenDocument(command) => {
                command.descriptor.validate()?;
                validate_document_text(&command.text)
            }
            Command::ChangeDocument(command) => {
                command.document.validate()?;
                command.new_version.validate()?;
                ensure_newer_version(command.document.version, command.new_version)?;
                command.changes.validate()
            }
            Command::SaveDocument(command) => {
                command.document.validate()?;
                command.disk_revision.validate()?;
                validate_document_text(&command.text)
            }
            Command::CloseDocument(command) => command.document.validate(),
            Command::RequestDiagnostics(command) => command.identity.validate(),
            Command::RequestCompletion(command)
            | Command::RequestHover(command)
            | Command::RequestDefinition(command) => command.identity.validate(),
            Command::RequestFormatting(command) => {
                command.identity.validate()?;
                command.options.validate()
            }
            Command::Cancel(command) => {
                command.project.validate()?;
                command.request_id.validate()
            }
            Command::Shutdown => Ok(()),
        }
    }
}

/// Diagnostic severity independent of any UI framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// Diagnostic code supplied by TypeScript or a host-owned analyzer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum DiagnosticCode {
    Number(i64),
    String(String),
}

/// A related diagnostic location in the committed project snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticRelatedInformation {
    pub document_id: DocumentId,
    pub range: Utf16Range,
    pub message: String,
    pub analyzed_uri: Option<String>,
}

/// One rich diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diagnostic {
    pub range: Utf16Range,
    pub severity: DiagnosticSeverity,
    pub code: Option<DiagnosticCode>,
    pub source: Option<String>,
    pub message: String,
    pub related_information: Vec<DiagnosticRelatedInformation>,
}

/// Rich diagnostic result for one exact document state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticsResult {
    pub items: Vec<Diagnostic>,
}

/// Markup carried by hover and completion documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MarkupKind {
    PlainText,
    Markdown,
}

/// Bounded markup content. Rendering and link policy are enforced by the consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarkupContent {
    pub kind: MarkupKind,
    pub value: String,
}

/// Completion presentation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompletionKind {
    Text,
    Method,
    Function,
    Constructor,
    Field,
    Variable,
    Class,
    Interface,
    Module,
    Property,
    Unit,
    Value,
    Enum,
    Keyword,
    Snippet,
    Color,
    File,
    Reference,
    Folder,
    EnumMember,
    Constant,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

/// How completion insertion text is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InsertTextFormat {
    PlainText,
    Snippet,
}

/// One rich completion item with same-document edits only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionItem {
    pub id: CompletionItemId,
    pub label: String,
    pub detail: Option<String>,
    pub documentation: Option<MarkupContent>,
    pub kind: CompletionKind,
    pub deprecated: bool,
    pub filter_text: Option<String>,
    pub sort_text: Option<String>,
    pub insert_text: Option<String>,
    pub insert_text_format: InsertTextFormat,
    pub primary_edit: Option<TextEdit>,
    pub additional_edits: Vec<TextEdit>,
}

/// Rich completion result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionResult {
    pub is_incomplete: bool,
    pub items: Vec<CompletionItem>,
}

/// Rich hover result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HoverResult {
    pub range: Option<Utf16Range>,
    pub contents: MarkupContent,
}

/// One same-project definition target. The opaque ID is the routing authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DefinitionTarget {
    pub document_id: DocumentId,
    pub target_range: Utf16Range,
    pub target_selection_range: Utf16Range,
    pub analyzed_uri: Option<String>,
}

/// Rich definition result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DefinitionResult {
    pub targets: Vec<DefinitionTarget>,
}

/// Rich formatting result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormattingResult {
    pub edits: Vec<TextEdit>,
}

/// Generic envelope for a document-scoped result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentResult<T> {
    pub identity: DocumentResultIdentity,
    pub analyzed_uri: Option<String>,
    pub result: T,
}

/// Project lifecycle state acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "identity", rename_all = "camelCase")]
pub enum AcknowledgedState {
    ProjectOpened(ProjectStateIdentity),
    ProjectRefreshed(ProjectStateIdentity),
    ProjectClosed(ProjectStateIdentity),
    ViewAttached {
        project: ProjectStateIdentity,
        view: ViewRef,
    },
    ViewDetached {
        project: ProjectStateIdentity,
        view: ViewRef,
    },
    DocumentOpened(DocumentStateIdentity),
    DocumentChanged(DocumentStateIdentity),
    DocumentSaved(DocumentStateIdentity),
    DocumentClosed(DocumentStateIdentity),
    RequestCanceled {
        project: ProjectStateIdentity,
        request_id: RequestId,
    },
    ShutdownAccepted {
        worker_generation: WorkerGeneration,
    },
}

/// Worker project availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "detail", rename_all = "camelCase")]
pub enum ProjectStatus {
    Ready,
    Degraded { code: String, message: String },
}

/// Project status at an exact generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectStatusEvent {
    pub identity: ProjectStateIdentity,
    pub status: ProjectStatus,
}

/// Scope of a failed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "identity", rename_all = "camelCase")]
pub enum FailureScope {
    Project(ProjectStateIdentity),
    Document(DocumentResultIdentity),
    Worker { worker_generation: WorkerGeneration },
}

/// Structured bounded request failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestFailure {
    pub scope: FailureScope,
    pub code: String,
    pub retryable: bool,
    pub user_message: String,
    pub log_detail: Option<String>,
}

/// Worker-to-parent event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum Event {
    StateAcknowledged(AcknowledgedState),
    ProjectStatus(ProjectStatusEvent),
    WorkerRestarted { worker_generation: WorkerGeneration },
    Diagnostics(DocumentResult<DiagnosticsResult>),
    Completion(DocumentResult<CompletionResult>),
    Hover(DocumentResult<Option<HoverResult>>),
    Definition(DocumentResult<DefinitionResult>),
    Formatting(DocumentResult<FormattingResult>),
    RequestFailed(RequestFailure),
}

/// Versioned worker-to-parent event envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventEnvelope {
    pub protocol_version: u16,
    pub command_sequence: CommandSequence,
    pub event: Event,
}

impl Validate for EventEnvelope {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        self.command_sequence.validate()?;
        match &self.event {
            Event::StateAcknowledged(state) => validate_acknowledged_state(*state),
            Event::ProjectStatus(event) => validate_project_status(event),
            Event::WorkerRestarted { worker_generation } => worker_generation.validate(),
            Event::Diagnostics(result) => validate_diagnostics_result(result),
            Event::Completion(result) => validate_completion_result(result),
            Event::Hover(result) => validate_hover_result(result),
            Event::Definition(result) => validate_definition_result(result),
            Event::Formatting(result) => validate_formatting_result(result),
            Event::RequestFailed(failure) => validate_request_failure(failure),
        }
    }
}

/// Protocol validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    UnsupportedProtocolVersion {
        expected: u16,
        actual: u16,
    },
    InvalidWireNumber {
        field: &'static str,
        value: u64,
    },
    InvalidDocumentId,
    InvalidValue(&'static str),
    TooManyItems {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    TooManyBytes {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    SizeOverflow(&'static str),
    InvalidRange(Utf16Range),
    PositionOutOfBounds(Utf16Position),
    PositionInsideSurrogatePair(Utf16Position),
    OverlappingEdits,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProtocolVersion { expected, actual } => {
                write!(
                    formatter,
                    "unsupported protocol version {actual}; expected {expected}"
                )
            }
            Self::InvalidWireNumber { field, value } => {
                write!(formatter, "invalid {field} wire value {value}")
            }
            Self::InvalidDocumentId => formatter.write_str("invalid document UUID"),
            Self::InvalidValue(field) => write!(formatter, "invalid {field}"),
            Self::TooManyItems {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "{field} contains {actual} items; maximum is {maximum}"
            ),
            Self::TooManyBytes {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "{field} contains {actual} bytes; maximum is {maximum}"
            ),
            Self::SizeOverflow(field) => write!(formatter, "{field} size overflow"),
            Self::InvalidRange(range) => write!(
                formatter,
                "invalid UTF-16 range {}:{}..{}:{}",
                range.start.line, range.start.character, range.end.line, range.end.character
            ),
            Self::PositionOutOfBounds(position) => write!(
                formatter,
                "UTF-16 position {}:{} is out of bounds",
                position.line, position.character
            ),
            Self::PositionInsideSurrogatePair(position) => write!(
                formatter,
                "UTF-16 position {}:{} splits a surrogate pair",
                position.line, position.character
            ),
            Self::OverlappingEdits => formatter.write_str("text edits overlap"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Validates the complete editable-document admission limits.
pub fn validate_document_text(text: &str) -> Result<(), ProtocolError> {
    validate_bytes("document text", text.len(), MAX_DOCUMENT_BYTES)?;

    let mut lines = 1_usize;
    let mut scalars_on_line = 0_usize;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if is_line_terminator(character) {
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
            }
            lines = lines
                .checked_add(1)
                .ok_or(ProtocolError::SizeOverflow("document lines"))?;
            validate_count("document lines", lines, MAX_DOCUMENT_LINES)?;
            scalars_on_line = 0;
        } else {
            scalars_on_line = scalars_on_line
                .checked_add(1)
                .ok_or(ProtocolError::SizeOverflow("line scalars"))?;
            validate_count(
                "Unicode scalars on line",
                scalars_on_line,
                MAX_SCALARS_PER_LINE,
            )?;
        }
    }

    Ok(())
}

fn validate_acknowledged_state(state: AcknowledgedState) -> Result<(), ProtocolError> {
    match state {
        AcknowledgedState::ProjectOpened(identity)
        | AcknowledgedState::ProjectRefreshed(identity)
        | AcknowledgedState::ProjectClosed(identity) => identity.validate(),
        AcknowledgedState::ViewAttached { project, view }
        | AcknowledgedState::ViewDetached { project, view } => {
            project.validate()?;
            view.validate()
        }
        AcknowledgedState::DocumentOpened(identity)
        | AcknowledgedState::DocumentChanged(identity)
        | AcknowledgedState::DocumentSaved(identity)
        | AcknowledgedState::DocumentClosed(identity) => identity.validate(),
        AcknowledgedState::RequestCanceled {
            project,
            request_id,
        } => {
            project.validate()?;
            request_id.validate()
        }
        AcknowledgedState::ShutdownAccepted { worker_generation } => worker_generation.validate(),
    }
}

fn validate_project_status(event: &ProjectStatusEvent) -> Result<(), ProtocolError> {
    event.identity.validate()?;
    if let ProjectStatus::Degraded { code, message } = &event.status {
        let mut bytes = 0;
        add_string_bytes(
            "project status",
            &mut bytes,
            code,
            MAX_RESULT_METADATA_BYTES,
        )?;
        add_string_bytes(
            "project status",
            &mut bytes,
            message,
            MAX_RESULT_METADATA_BYTES,
        )?;
    }
    Ok(())
}

fn validate_document_result<T>(result: &DocumentResult<T>) -> Result<(), ProtocolError> {
    result.identity.validate()?;
    if let Some(uri) = &result.analyzed_uri {
        validate_uri(uri)?;
    }
    Ok(())
}

fn validate_diagnostics_result(
    result: &DocumentResult<DiagnosticsResult>,
) -> Result<(), ProtocolError> {
    validate_document_result(result)?;
    validate_count(
        "diagnostics",
        result.result.items.len(),
        MAX_DIAGNOSTICS_PER_DOCUMENT,
    )?;

    let mut metadata_bytes = 0;
    let mut related_count = 0_usize;
    for diagnostic in &result.result.items {
        diagnostic.range.validate()?;
        add_string_bytes(
            "diagnostic metadata",
            &mut metadata_bytes,
            &diagnostic.message,
            MAX_RESULT_METADATA_BYTES,
        )?;
        if let Some(source) = &diagnostic.source {
            add_string_bytes(
                "diagnostic metadata",
                &mut metadata_bytes,
                source,
                MAX_RESULT_METADATA_BYTES,
            )?;
        }
        if let Some(DiagnosticCode::String(code)) = &diagnostic.code {
            add_string_bytes(
                "diagnostic metadata",
                &mut metadata_bytes,
                code,
                MAX_RESULT_METADATA_BYTES,
            )?;
        }

        related_count = related_count
            .checked_add(diagnostic.related_information.len())
            .ok_or(ProtocolError::SizeOverflow(
                "diagnostic related information",
            ))?;
        validate_count(
            "diagnostic related information",
            related_count,
            MAX_DIAGNOSTIC_RELATED_INFORMATION,
        )?;
        for related in &diagnostic.related_information {
            related.document_id.validate()?;
            related.range.validate()?;
            add_string_bytes(
                "diagnostic metadata",
                &mut metadata_bytes,
                &related.message,
                MAX_RESULT_METADATA_BYTES,
            )?;
            if let Some(uri) = &related.analyzed_uri {
                validate_uri(uri)?;
                add_string_bytes(
                    "diagnostic metadata",
                    &mut metadata_bytes,
                    uri,
                    MAX_RESULT_METADATA_BYTES,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_completion_result(
    result: &DocumentResult<CompletionResult>,
) -> Result<(), ProtocolError> {
    validate_document_result(result)?;
    validate_count(
        "completion items",
        result.result.items.len(),
        MAX_COMPLETION_ITEMS,
    )?;

    let mut metadata_bytes = 0;
    let mut additional_edit_count = 0_usize;
    for item in &result.result.items {
        item.id.validate()?;
        add_string_bytes(
            "completion metadata",
            &mut metadata_bytes,
            &item.label,
            MAX_RESULT_METADATA_BYTES,
        )?;
        for value in [
            item.detail.as_deref(),
            item.filter_text.as_deref(),
            item.sort_text.as_deref(),
            item.insert_text.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            add_string_bytes(
                "completion metadata",
                &mut metadata_bytes,
                value,
                MAX_RESULT_METADATA_BYTES,
            )?;
        }
        if let Some(documentation) = &item.documentation {
            validate_bytes(
                "completion documentation",
                documentation.value.len(),
                MAX_HOVER_BYTES,
            )?;
            add_string_bytes(
                "completion metadata",
                &mut metadata_bytes,
                &documentation.value,
                MAX_RESULT_METADATA_BYTES,
            )?;
        }

        additional_edit_count = additional_edit_count
            .checked_add(item.additional_edits.len())
            .ok_or(ProtocolError::SizeOverflow("completion additional edits"))?;
        validate_count(
            "completion additional edits",
            additional_edit_count,
            MAX_COMPLETION_ADDITIONAL_EDITS,
        )?;

        let mut edits = Vec::with_capacity(
            item.additional_edits.len() + usize::from(item.primary_edit.is_some()),
        );
        if let Some(edit) = &item.primary_edit {
            edits.push(edit);
        }
        edits.extend(&item.additional_edits);
        validate_text_edits(
            &edits,
            MAX_COMPLETION_ADDITIONAL_EDITS + 1,
            MAX_RESULT_METADATA_BYTES,
            "completion edits",
        )?;
        for edit in edits {
            add_string_bytes(
                "completion metadata",
                &mut metadata_bytes,
                &edit.new_text,
                MAX_RESULT_METADATA_BYTES,
            )?;
        }
    }
    Ok(())
}

fn validate_hover_result(
    result: &DocumentResult<Option<HoverResult>>,
) -> Result<(), ProtocolError> {
    validate_document_result(result)?;
    if let Some(hover) = &result.result {
        if let Some(range) = hover.range {
            range.validate()?;
        }
        validate_bytes(
            "hover contents",
            hover.contents.value.len(),
            MAX_HOVER_BYTES,
        )?;
    }
    Ok(())
}

fn validate_definition_result(
    result: &DocumentResult<DefinitionResult>,
) -> Result<(), ProtocolError> {
    validate_document_result(result)?;
    validate_count(
        "definition targets",
        result.result.targets.len(),
        MAX_DEFINITION_TARGETS,
    )?;
    let mut metadata_bytes = 0;
    for target in &result.result.targets {
        target.document_id.validate()?;
        target.target_range.validate()?;
        target.target_selection_range.validate()?;
        if !target.target_range.contains(target.target_selection_range) {
            return Err(ProtocolError::InvalidValue(
                "definition target selection range",
            ));
        }
        if let Some(uri) = &target.analyzed_uri {
            validate_uri(uri)?;
            add_string_bytes(
                "definition metadata",
                &mut metadata_bytes,
                uri,
                MAX_RESULT_METADATA_BYTES,
            )?;
        }
    }
    Ok(())
}

fn validate_formatting_result(
    result: &DocumentResult<FormattingResult>,
) -> Result<(), ProtocolError> {
    validate_document_result(result)?;
    let edits: Vec<_> = result.result.edits.iter().collect();
    validate_text_edits(
        &edits,
        MAX_FORMATTING_EDITS,
        MAX_FORMATTING_REPLACEMENT_BYTES,
        "formatting edits",
    )
}

fn validate_request_failure(failure: &RequestFailure) -> Result<(), ProtocolError> {
    match failure.scope {
        FailureScope::Project(identity) => identity.validate()?,
        FailureScope::Document(identity) => identity.validate()?,
        FailureScope::Worker { worker_generation } => worker_generation.validate()?,
    }

    let mut bytes = 0;
    for value in [
        Some(failure.code.as_str()),
        Some(failure.user_message.as_str()),
        failure.log_detail.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        add_string_bytes(
            "request failure",
            &mut bytes,
            value,
            MAX_RESULT_METADATA_BYTES,
        )?;
    }
    Ok(())
}

fn validate_text_edits(
    edits: &[&TextEdit],
    maximum_count: usize,
    maximum_replacement_bytes: usize,
    field: &'static str,
) -> Result<(), ProtocolError> {
    validate_count(field, edits.len(), maximum_count)?;
    let mut replacement_bytes = 0;
    for edit in edits {
        edit.range.validate()?;
        add_bytes(
            field,
            &mut replacement_bytes,
            edit.new_text.len(),
            maximum_replacement_bytes,
        )?;
    }

    let mut ordered = edits.to_vec();
    ordered.sort_unstable_by_key(|edit| (edit.range.start, edit.range.end));
    for pair in ordered.windows(2) {
        let left = pair[0].range;
        let right = pair[1].range;
        let duplicate_empty =
            left.start == left.end && right.start == right.end && left.start == right.start;
        if left.end > right.start || duplicate_empty {
            return Err(ProtocolError::OverlappingEdits);
        }
    }
    Ok(())
}

fn validate_protocol_version(actual: u16) -> Result<(), ProtocolError> {
    if actual == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedProtocolVersion {
            expected: PROTOCOL_VERSION,
            actual,
        })
    }
}

fn validate_wire_number(field: &'static str, value: u64) -> Result<(), ProtocolError> {
    if value > 0 && value <= MAX_WIRE_INTEGER {
        Ok(())
    } else {
        Err(ProtocolError::InvalidWireNumber { field, value })
    }
}

fn parse_document_id(value: &str) -> Result<DocumentId, ProtocolError> {
    let encoded = value.as_bytes();
    if encoded.len() != 36
        || encoded[8] != b'-'
        || encoded[13] != b'-'
        || encoded[18] != b'-'
        || encoded[23] != b'-'
    {
        return Err(ProtocolError::InvalidDocumentId);
    }

    let mut bytes = [0_u8; 16];
    let mut encoded_index = 0_usize;
    let mut byte_index = 0_usize;
    while byte_index < bytes.len() {
        if matches!(encoded_index, 8 | 13 | 18 | 23) {
            encoded_index += 1;
        }
        let high = decode_hex(encoded[encoded_index])?;
        let low = decode_hex(encoded[encoded_index + 1])?;
        bytes[byte_index] = (high << 4) | low;
        encoded_index += 2;
        byte_index += 1;
    }

    DocumentId::try_from(bytes)
}

fn decode_hex(value: u8) -> Result<u8, ProtocolError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(ProtocolError::InvalidDocumentId),
    }
}

fn ensure_newer_version(
    current: DocumentVersion,
    next: DocumentVersion,
) -> Result<(), ProtocolError> {
    if next.get() > current.get() {
        Ok(())
    } else {
        Err(ProtocolError::InvalidValue("document version transition"))
    }
}

fn validate_uri(uri: &str) -> Result<(), ProtocolError> {
    validate_bytes("URI", uri.len(), MAX_URI_BYTES)?;
    if uri.is_empty() || uri.chars().any(char::is_control) {
        Err(ProtocolError::InvalidValue("URI"))
    } else {
        Ok(())
    }
}

fn validate_count(field: &'static str, actual: usize, maximum: usize) -> Result<(), ProtocolError> {
    if actual <= maximum {
        Ok(())
    } else {
        Err(ProtocolError::TooManyItems {
            field,
            actual,
            maximum,
        })
    }
}

fn validate_bytes(field: &'static str, actual: usize, maximum: usize) -> Result<(), ProtocolError> {
    if actual <= maximum {
        Ok(())
    } else {
        Err(ProtocolError::TooManyBytes {
            field,
            actual,
            maximum,
        })
    }
}

fn add_bytes(
    field: &'static str,
    total: &mut usize,
    amount: usize,
    maximum: usize,
) -> Result<(), ProtocolError> {
    *total = total
        .checked_add(amount)
        .ok_or(ProtocolError::SizeOverflow(field))?;
    validate_bytes(field, *total, maximum)
}

fn add_string_bytes(
    field: &'static str,
    total: &mut usize,
    value: &str,
    maximum: usize,
) -> Result<(), ProtocolError> {
    add_bytes(field, total, value.len(), maximum)
}

fn line_bounds(text: &str, requested_line: u32) -> Result<(usize, usize), ProtocolError> {
    let mut current_line = 0_u32;
    let mut line_start = 0_usize;
    let mut characters = text.char_indices().peekable();

    while let Some((offset, character)) = characters.next() {
        if !is_line_terminator(character) {
            continue;
        }
        if current_line == requested_line {
            return Ok((line_start, offset));
        }

        let mut next_start = offset + character.len_utf8();
        if character == '\r' && characters.peek().is_some_and(|(_, next)| *next == '\n') {
            if let Some((next_offset, next)) = characters.next() {
                next_start = next_offset + next.len_utf8();
            }
        }
        current_line = current_line
            .checked_add(1)
            .ok_or(ProtocolError::PositionOutOfBounds(Utf16Position {
                line: requested_line,
                character: 0,
            }))?;
        line_start = next_start;
    }

    if current_line == requested_line {
        Ok((line_start, text.len()))
    } else {
        Err(ProtocolError::PositionOutOfBounds(Utf16Position {
            line: requested_line,
            character: 0,
        }))
    }
}

fn is_line_terminator(character: char) -> bool {
    matches!(character, '\r' | '\n' | '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T>(value: u64) -> T
    where
        T: TryFrom<u64, Error = ProtocolError>,
    {
        T::try_from(value).expect("valid fixture wire value")
    }

    fn project() -> ProjectScope {
        ProjectScope {
            client_id: wire(1),
            project_id: wire(2),
        }
    }

    fn view() -> ViewRef {
        ViewRef {
            view_id: wire(3),
            generation: wire(4),
        }
    }

    fn document_id(seed: u8) -> DocumentId {
        DocumentId::try_from([seed; 16]).expect("valid fixture document UUID")
    }

    fn indexed_document_id(index: usize) -> DocumentId {
        let mut bytes = [0_u8; 16];
        let value = u64::try_from(index)
            .expect("fixture index fits u64")
            .checked_add(1)
            .expect("fixture document ID does not overflow");
        bytes[8..].copy_from_slice(&value.to_be_bytes());
        DocumentId::try_from(bytes).expect("nonzero fixture document UUID")
    }

    fn project_source(
        index: usize,
        uri: impl Into<String>,
        text: impl Into<String>,
    ) -> ProjectSource {
        ProjectSource {
            document_id: indexed_document_id(index),
            uri: uri.into(),
            language: Language::TypeScript,
            kind: DocumentKind::StandaloneModule,
            text: text.into(),
        }
    }

    fn project_refresh(sources: Vec<ProjectSource>) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: PROTOCOL_VERSION,
            command_sequence: wire(12),
            command: Command::RefreshProject(RefreshProject {
                project: project(),
                graph_generation: wire(13),
                sources,
            }),
        }
    }

    fn maximal_document_text() -> String {
        let complete_line = format!("{}\n", "x".repeat(MAX_SCALARS_PER_LINE));
        let complete_lines = MAX_DOCUMENT_BYTES / complete_line.len();
        let remainder = MAX_DOCUMENT_BYTES % complete_line.len();
        let mut text = complete_line.repeat(complete_lines);
        text.push_str(&"x".repeat(remainder));
        text
    }

    fn document_ref() -> DocumentRef {
        DocumentRef {
            key: DocumentKey {
                project: project(),
                document_id: document_id(5),
            },
            view: Some(view()),
            version: wire(6),
        }
    }

    fn result_identity() -> DocumentResultIdentity {
        DocumentResultIdentity {
            state: DocumentStateIdentity {
                document: document_ref(),
                graph_generation: wire(7),
                service_generation: wire(8),
                worker_generation: wire(9),
            },
            request_id: wire(10),
        }
    }

    fn event<T>(result: T, make_event: impl FnOnce(DocumentResult<T>) -> Event) -> EventEnvelope {
        EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            command_sequence: wire(11),
            event: make_event(DocumentResult {
                identity: result_identity(),
                analyzed_uri: Some("smudgy-inline://fixture/alias/5.ts".into()),
                result,
            }),
        }
    }

    #[test]
    fn wire_values_reject_zero_and_non_javascript_safe_values() {
        assert_eq!(
            ClientId::try_from(0),
            Err(ProtocolError::InvalidWireNumber {
                field: "ClientId",
                value: 0
            })
        );
        assert!(ClientId::try_from(MAX_WIRE_INTEGER).is_ok());
        assert_eq!(
            ClientId::try_from(MAX_WIRE_INTEGER + 1),
            Err(ProtocolError::InvalidWireNumber {
                field: "ClientId",
                value: MAX_WIRE_INTEGER + 1
            })
        );

        let decoded: CommandEnvelope = serde_json::from_str(
            r#"{"protocolVersion":1,"commandSequence":0,"command":{"type":"shutdown"}}"#,
        )
        .expect("shape is typed before semantic validation");
        assert!(matches!(
            decoded.validate(),
            Err(ProtocolError::InvalidWireNumber {
                field: "CommandSequence",
                value: 0
            })
        ));
    }

    #[test]
    fn document_ids_use_canonical_non_nil_uuid_strings() {
        let id = DocumentId::from_str("01234567-89AB-CDEF-0123-456789ABCDEF")
            .expect("valid UUID fixture");
        assert_eq!(id.to_string(), "01234567-89ab-cdef-0123-456789abcdef");
        assert_eq!(
            serde_json::to_string(&id).expect("serialize document ID"),
            r#""01234567-89ab-cdef-0123-456789abcdef""#
        );
        assert!(
            serde_json::from_str::<DocumentId>(r#""00000000-0000-0000-0000-000000000000""#)
                .is_err()
        );
        assert!(DocumentId::from_str("not-a-uuid").is_err());
    }

    #[test]
    fn project_refresh_round_trip_preserves_atomic_sources() {
        let envelope = project_refresh(vec![
            project_source(0, "smudgy-project:///modules/main.ts", "export {};\n"),
            ProjectSource {
                document_id: indexed_document_id(1),
                uri: "smudgy-project:///packages/example/data.json".to_owned(),
                language: Language::Json,
                kind: DocumentKind::OwnedPackage,
                text: "{\"ready\":true}\n".to_owned(),
            },
        ]);
        envelope.validate().expect("project refresh is valid");

        let json = serde_json::to_string(&envelope).expect("serialize project refresh");
        let decoded: CommandEnvelope =
            serde_json::from_str(&json).expect("deserialize project refresh");
        assert_eq!(decoded, envelope);
        assert!(json.contains("\"sources\""));
        assert!(json.contains("smudgy-project:///modules/main.ts"));
    }

    #[test]
    fn project_refresh_rejects_duplicate_document_ids_and_exact_uris() {
        let first = project_source(0, "smudgy-project:///modules/main.ts", "");
        let mut duplicate_id = project_source(1, "smudgy-project:///modules/other.ts", "");
        duplicate_id.document_id = first.document_id;
        assert_eq!(
            project_refresh(vec![first.clone(), duplicate_id]).validate(),
            Err(ProtocolError::InvalidValue(
                "duplicate project source document ID"
            ))
        );

        let duplicate_uri = project_source(1, &first.uri, "");
        assert_eq!(
            project_refresh(vec![first, duplicate_uri]).validate(),
            Err(ProtocolError::InvalidValue("duplicate project source URI"))
        );
    }

    #[test]
    fn project_refresh_enforces_file_and_decoded_text_caps() {
        let too_many = (0..=MAX_PROJECT_SOURCE_FILES)
            .map(|index| project_source(index, format!("smudgy-project:///modules/{index}.ts"), ""))
            .collect();
        assert!(matches!(
            project_refresh(too_many).validate(),
            Err(ProtocolError::TooManyItems {
                field: "project sources",
                actual,
                maximum: MAX_PROJECT_SOURCE_FILES,
            }) if actual == MAX_PROJECT_SOURCE_FILES + 1
        ));

        let maximal_text = maximal_document_text();
        assert_eq!(maximal_text.len(), MAX_DOCUMENT_BYTES);
        let mut at_cap = (0..(MAX_PROJECT_SOURCE_TEXT_BYTES / MAX_DOCUMENT_BYTES))
            .map(|index| {
                project_source(
                    index,
                    format!("smudgy-project:///modules/{index}.ts"),
                    maximal_text.clone(),
                )
            })
            .collect::<Vec<_>>();
        project_refresh(at_cap.clone())
            .validate()
            .expect("aggregate decoded text at the cap is valid");

        let next_index = at_cap.len();
        at_cap.push(project_source(
            next_index,
            format!("smudgy-project:///modules/{next_index}.ts"),
            "x",
        ));
        assert!(matches!(
            project_refresh(at_cap).validate(),
            Err(ProtocolError::TooManyBytes {
                field: "project source text",
                actual,
                maximum: MAX_PROJECT_SOURCE_TEXT_BYTES,
            }) if actual == MAX_PROJECT_SOURCE_TEXT_BYTES + 1
        ));
    }

    #[test]
    fn utf16_positions_cover_astral_and_all_line_terminators() {
        let text = "a😀b\r\nc\rd\n\u{2028}e\u{2029}";
        assert_eq!(
            Utf16Position {
                line: 0,
                character: 1
            }
            .to_byte_offset(text),
            Ok(1)
        );
        assert_eq!(
            Utf16Position {
                line: 0,
                character: 3
            }
            .to_byte_offset(text),
            Ok(5)
        );
        assert!(matches!(
            Utf16Position {
                line: 0,
                character: 2
            }
            .to_byte_offset(text),
            Err(ProtocolError::PositionInsideSurrogatePair(_))
        ));

        for (line, expected) in [(1, 8), (2, 10), (3, 12), (4, 15), (5, 19)] {
            assert_eq!(
                Utf16Position { line, character: 0 }.to_byte_offset(text),
                Ok(expected)
            );
        }
        assert!(matches!(
            Utf16Position {
                line: 6,
                character: 0
            }
            .to_byte_offset(text),
            Err(ProtocolError::PositionOutOfBounds(_))
        ));
    }

    #[test]
    fn ordered_changes_use_each_predecessors_utf16_state() {
        let changes = DocumentChanges {
            changes: vec![
                TextChange {
                    range: Some(Utf16Range {
                        start: Utf16Position {
                            line: 0,
                            character: 1,
                        },
                        end: Utf16Position {
                            line: 0,
                            character: 3,
                        },
                    }),
                    text: "x".into(),
                },
                TextChange {
                    range: Some(Utf16Range {
                        start: Utf16Position {
                            line: 0,
                            character: 2,
                        },
                        end: Utf16Position {
                            line: 0,
                            character: 3,
                        },
                    }),
                    text: "z".into(),
                },
            ],
        };

        assert_eq!(changes.apply_to("a😀b"), Ok("axz".into()));
    }

    #[test]
    fn command_round_trip_preserves_every_routing_identity() {
        let envelope = CommandEnvelope {
            protocol_version: PROTOCOL_VERSION,
            command_sequence: wire(20),
            command: Command::RequestCompletion(PositionRequest {
                identity: result_identity(),
                position: Utf16Position {
                    line: 12,
                    character: 34,
                },
            }),
        };
        envelope.validate().expect("fixture is valid");

        let json = serde_json::to_string(&envelope).expect("serialize command");
        let decoded: CommandEnvelope = serde_json::from_str(&json).expect("deserialize command");
        assert_eq!(decoded, envelope);
        assert!(json.contains("\"clientId\":1"));
        assert!(json.contains("\"projectId\":2"));
        assert!(json.contains("\"viewId\":3"));
        assert!(json.contains("\"documentId\":\"05050505-0505-0505-0505-050505050505\""));
        assert!(json.contains("\"requestId\":10"));
    }

    #[test]
    fn complete_state_and_request_are_required_for_current_result() {
        let identity = result_identity();
        assert!(identity.is_current_for(&identity.state, identity.request_id));

        let mut stale_state = identity.state;
        stale_state.worker_generation = wire(99);
        assert!(!identity.is_current_for(&stale_state, identity.request_id));
        assert!(!identity.is_current_for(&identity.state, wire(99)));
    }

    #[test]
    fn diagnostics_enforce_item_related_and_metadata_bounds() {
        let diagnostic = Diagnostic {
            range: Utf16Range::default(),
            severity: DiagnosticSeverity::Error,
            code: Some(DiagnosticCode::Number(2322)),
            source: Some("typescript".into()),
            message: "mismatch".into(),
            related_information: Vec::new(),
        };
        let valid = event(
            DiagnosticsResult {
                items: vec![diagnostic.clone(); MAX_DIAGNOSTICS_PER_DOCUMENT],
            },
            Event::Diagnostics,
        );
        valid.validate().expect("exact diagnostic count is valid");

        let too_many = event(
            DiagnosticsResult {
                items: vec![diagnostic; MAX_DIAGNOSTICS_PER_DOCUMENT + 1],
            },
            Event::Diagnostics,
        );
        assert!(matches!(
            too_many.validate(),
            Err(ProtocolError::TooManyItems {
                field: "diagnostics",
                ..
            })
        ));
    }

    #[test]
    fn completion_rejects_overlapping_and_excess_additional_edits() {
        let edit = TextEdit {
            range: Utf16Range::default(),
            new_text: "x".into(),
        };
        let item = CompletionItem {
            id: wire(1),
            label: "value".into(),
            detail: None,
            documentation: None,
            kind: CompletionKind::Variable,
            deprecated: false,
            filter_text: None,
            sort_text: None,
            insert_text: None,
            insert_text_format: InsertTextFormat::PlainText,
            primary_edit: Some(edit.clone()),
            additional_edits: vec![edit.clone()],
        };
        let overlapping = event(
            CompletionResult {
                is_incomplete: false,
                items: vec![item],
            },
            Event::Completion,
        );
        assert_eq!(overlapping.validate(), Err(ProtocolError::OverlappingEdits));

        let oversized_documentation = event(
            CompletionResult {
                is_incomplete: false,
                items: vec![CompletionItem {
                    id: wire(2),
                    label: "value".into(),
                    detail: None,
                    documentation: Some(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: "x".repeat(MAX_HOVER_BYTES + 1),
                    }),
                    kind: CompletionKind::Variable,
                    deprecated: false,
                    filter_text: None,
                    sort_text: None,
                    insert_text: None,
                    insert_text_format: InsertTextFormat::PlainText,
                    primary_edit: None,
                    additional_edits: Vec::new(),
                }],
            },
            Event::Completion,
        );
        assert!(matches!(
            oversized_documentation.validate(),
            Err(ProtocolError::TooManyBytes {
                field: "completion documentation",
                ..
            })
        ));

        let excessive = CompletionItem {
            id: wire(3),
            label: "value".into(),
            detail: None,
            documentation: None,
            kind: CompletionKind::Variable,
            deprecated: false,
            filter_text: None,
            sort_text: None,
            insert_text: None,
            insert_text_format: InsertTextFormat::PlainText,
            primary_edit: None,
            additional_edits: vec![
                TextEdit {
                    range: Utf16Range {
                        start: Utf16Position {
                            line: 0,
                            character: 1,
                        },
                        end: Utf16Position {
                            line: 0,
                            character: 2,
                        },
                    },
                    new_text: "x".into(),
                };
                MAX_COMPLETION_ADDITIONAL_EDITS + 1
            ],
        };
        let excessive = event(
            CompletionResult {
                is_incomplete: false,
                items: vec![excessive],
            },
            Event::Completion,
        );
        assert!(matches!(
            excessive.validate(),
            Err(ProtocolError::TooManyItems {
                field: "completion additional edits",
                ..
            })
        ));
    }

    #[test]
    fn hover_definition_and_formatting_enforce_exact_caps() {
        let hover = event(
            Some(HoverResult {
                range: None,
                contents: MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: "x".repeat(MAX_HOVER_BYTES + 1),
                },
            }),
            Event::Hover,
        );
        assert!(matches!(
            hover.validate(),
            Err(ProtocolError::TooManyBytes {
                field: "hover contents",
                ..
            })
        ));

        let target = DefinitionTarget {
            document_id: document_id(1),
            target_range: Utf16Range::default(),
            target_selection_range: Utf16Range::default(),
            analyzed_uri: None,
        };
        let definitions = event(
            DefinitionResult {
                targets: vec![target; MAX_DEFINITION_TARGETS + 1],
            },
            Event::Definition,
        );
        assert!(matches!(
            definitions.validate(),
            Err(ProtocolError::TooManyItems {
                field: "definition targets",
                ..
            })
        ));

        let formatting = event(
            FormattingResult {
                edits: vec![TextEdit {
                    range: Utf16Range::default(),
                    new_text: "x".repeat(MAX_FORMATTING_REPLACEMENT_BYTES + 1),
                }],
            },
            Event::Formatting,
        );
        assert!(matches!(
            formatting.validate(),
            Err(ProtocolError::TooManyBytes {
                field: "formatting edits",
                ..
            })
        ));
    }

    #[test]
    fn document_admission_checks_bytes_lines_and_line_scalars() {
        let complete_line = format!("{}\n", "x".repeat(MAX_SCALARS_PER_LINE));
        let complete_lines = MAX_DOCUMENT_BYTES / complete_line.len();
        let remainder = MAX_DOCUMENT_BYTES % complete_line.len();
        let mut valid_at_byte_cap = complete_line.repeat(complete_lines);
        valid_at_byte_cap.push_str(&"x".repeat(remainder));
        assert_eq!(valid_at_byte_cap.len(), MAX_DOCUMENT_BYTES);
        assert!(validate_document_text(&valid_at_byte_cap).is_ok());
        assert!(matches!(
            validate_document_text(&"x".repeat(MAX_DOCUMENT_BYTES + 1)),
            Err(ProtocolError::TooManyBytes {
                field: "document text",
                ..
            })
        ));
        assert!(matches!(
            validate_document_text(&"\n".repeat(MAX_DOCUMENT_LINES)),
            Err(ProtocolError::TooManyItems {
                field: "document lines",
                ..
            })
        ));
        assert!(matches!(
            validate_document_text(&"x".repeat(MAX_SCALARS_PER_LINE + 1)),
            Err(ProtocolError::TooManyItems {
                field: "Unicode scalars on line",
                ..
            })
        ));
    }

    #[test]
    fn lifecycle_ack_round_trips_and_validates_generation_fields() {
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            command_sequence: wire(42),
            event: Event::StateAcknowledged(AcknowledgedState::DocumentOpened(
                result_identity().state,
            )),
        };
        envelope.validate().expect("ack is valid");
        let json = serde_json::to_string(&envelope).expect("serialize ack");
        assert_eq!(
            serde_json::from_str::<EventEnvelope>(&json).expect("deserialize ack"),
            envelope
        );
    }
}
