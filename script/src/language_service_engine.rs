//! Embedded TypeScript language-service engine for authoring-time intelligence.
//!
//! The engine runs the vendored TypeScript compiler inside Smudgy's existing Deno
//! runtime. Its compiler host is an immutable in-memory table: it has no filesystem,
//! network, environment, subprocess, or FFI access. A UI-facing worker owns one engine
//! and replaces its project snapshot whenever editable authoring state changes.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use deno_core::{FastString, serde_v8};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::language_service::{
    CompletionItem, CompletionItemId, CompletionKind, CompletionResult, DefinitionResult,
    DefinitionTarget, Diagnostic, DiagnosticCode, DiagnosticSeverity, DiagnosticsResult,
    DocumentId, FormattingOptions, FormattingResult, HoverResult, InsertTextFormat, Language,
    LanguageServiceLibrary, MAX_DIAGNOSTICS_PER_DOCUMENT, MarkupContent, MarkupKind, TextEdit,
    Utf16Position, Utf16Range, validate_document_text,
};
use crate::{
    ModulePolicy, Permissions, PermissionsContainer, ScriptRuntime, ScriptRuntimeOptions,
    WorkerMode, permission_descriptor_parser,
};

const TYPESCRIPT_JS: &str = include_str!("../vendor/typescript/lib/typescript.js");
const DRIVER_JS: &str = include_str!("language_service_driver.js");

include!(concat!(env!("OUT_DIR"), "/dts_libs.rs"));

const SHIM_JS: &str = r#"
globalThis.module = { exports: {} };
globalThis.exports = globalThis.module.exports;
globalThis.process = { argv: [], env: {}, platform: "smudgy", cwd: () => "/", nextTick: (f) => f() };
globalThis.console = { log() {}, error() {}, warn() {}, info() {}, debug() {} };
"#;

const BIND_JS: &str = r#"
globalThis.ts = globalThis.module.exports;
if (!globalThis.ts || typeof globalThis.ts.createLanguageService !== "function") {
  throw new Error("failed to load embedded TypeScript LanguageService");
}
"#;

/// One source file in the authoring project snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    pub document_id: DocumentId,
    pub file_name: String,
    pub uri: String,
    pub language: Language,
    pub text: String,
}

impl ProjectFile {
    /// Builds a validated file with a normalized, absolute VFS name.
    pub fn new(
        document_id: DocumentId,
        file_name: impl Into<String>,
        uri: impl Into<String>,
        language: Language,
        text: impl Into<String>,
    ) -> Result<Self> {
        let file_name = normalize_file_name(&file_name.into())?;
        let uri = uri.into();
        if uri.is_empty() {
            bail!("language-service URI cannot be empty");
        }
        let text = text.into();
        validate_document_text(&text).context("validate language-service document")?;
        Ok(Self {
            document_id,
            file_name,
            uri,
            language,
            text,
        })
    }
}

/// Persistent authoring-time TypeScript service over host-owned snapshots.
pub struct EmbeddedLanguageService {
    runtime: ScriptRuntime,
    files: BTreeMap<DocumentId, ProjectFile>,
    ids_by_file_name: BTreeMap<String, DocumentId>,
    _data_dir: TemporaryDataDir,
}

impl EmbeddedLanguageService {
    /// Boots the embedded compiler and installs the vendored standard libraries.
    #[cfg(test)]
    pub fn new() -> Result<Self> {
        Self::new_with_libraries(Vec::new())
    }

    /// Boots the embedded compiler with additional immutable declaration libraries.
    pub fn new_with_libraries(extra_libraries: Vec<LanguageServiceLibrary>) -> Result<Self> {
        let data_dir = TemporaryDataDir::new()?;
        let tokio = Rc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build language-service Tokio runtime")?,
        );
        let permissions = PermissionsContainer::new(
            permission_descriptor_parser(),
            Permissions::none_without_prompt(),
        );
        let mut runtime = ScriptRuntime::new(ScriptRuntimeOptions {
            extensions: Vec::new(),
            data_dir: data_dir.path().to_path_buf(),
            webstorage_dir: None,
            module_policy: ModulePolicy {
                allow_https: false,
                ..Default::default()
            },
            inspector: None,
            tokio,
            package_provider: None,
            permissions: Some(permissions),
            broadcast_channel: None,
            workers: WorkerMode::Disabled,
            max_live_workers_override: None,
        })
        .context("boot embedded language-service runtime")?;

        execute(&mut runtime, "[smudgy:lsp:shim]", SHIM_JS)?;
        execute(&mut runtime, "[smudgy:lsp:typescript]", TYPESCRIPT_JS)?;
        execute(&mut runtime, "[smudgy:lsp:bind]", BIND_JS)?;
        execute(&mut runtime, "[smudgy:lsp:driver]", DRIVER_JS)?;

        let mut service = Self {
            runtime,
            files: BTreeMap::new(),
            ids_by_file_name: BTreeMap::new(),
            _data_dir: data_dir,
        };
        let mut libraries = esnext_standard_libraries(LIBS)?;

        let mut library_roots = Vec::new();
        for library in extra_libraries {
            let file_name = normalize_file_name(&library.file_name)?;
            validate_document_text(&library.text)
                .with_context(|| format!("validate language-service library {file_name}"))?;
            if libraries.contains_key(&file_name) {
                bail!("duplicate language-service library {file_name}");
            }
            if library.is_root {
                library_roots.push(file_name.clone());
            }
            libraries.insert(file_name, library.text);
        }
        library_roots.sort();
        library_roots.dedup();
        let initialized: InitializeResult = service.call(
            "initialize",
            &json!({ "libraries": libraries, "libraryRoots": library_roots }),
        )?;
        if initialized.typescript_version.is_empty() {
            bail!("embedded TypeScript service reported no version");
        }
        Ok(service)
    }

    /// Atomically replaces the complete project snapshot.
    pub fn replace_project(&mut self, files: Vec<ProjectFile>) -> Result<()> {
        let mut next_files = BTreeMap::new();
        let mut next_ids = BTreeMap::new();
        let mut wire_files = BTreeMap::new();
        for mut file in files {
            file.file_name = normalize_file_name(&file.file_name)?;
            validate_document_text(&file.text)
                .with_context(|| format!("validate language-service file {}", file.file_name))?;
            if next_files.contains_key(&file.document_id) {
                bail!("duplicate language-service document {}", file.document_id);
            }
            if next_ids
                .insert(file.file_name.clone(), file.document_id)
                .is_some()
            {
                bail!("duplicate language-service VFS name {}", file.file_name);
            }
            wire_files.insert(file.file_name.clone(), file.text.clone());
            next_files.insert(file.document_id, file);
        }

        let _: ReplaceProjectResult =
            self.call("replaceProject", &json!({ "files": wire_files }))?;
        self.files = next_files;
        self.ids_by_file_name = next_ids;
        Ok(())
    }

    /// Returns TypeScript syntactic, semantic, and suggestion diagnostics.
    pub fn diagnostics(&mut self, document_id: DocumentId) -> Result<DiagnosticsResult> {
        let file_name = self.file(document_id)?.file_name.clone();
        let raw: RawDiagnostics = self.call("diagnostics", &json!({ "fileName": file_name }))?;
        let items = raw
            .diagnostics
            .into_iter()
            .take(MAX_DIAGNOSTICS_PER_DOCUMENT)
            .map(|diagnostic| Diagnostic {
                range: diagnostic.range.unwrap_or_default(),
                severity: diagnostic_severity(&diagnostic.category),
                code: Some(DiagnosticCode::Number(diagnostic.code)),
                source: Some("typescript".to_owned()),
                message: diagnostic.message,
                related_information: Vec::new(),
            })
            .collect();
        Ok(DiagnosticsResult { items })
    }

    /// Returns completion entries at an exact UTF-16 position.
    pub fn completion(
        &mut self,
        document_id: DocumentId,
        position: Utf16Position,
    ) -> Result<CompletionResult> {
        self.validate_position(document_id, position)?;
        let file_name = self.file(document_id)?.file_name.clone();
        let raw: RawCompletion = self.call(
            "completion",
            &json!({ "fileName": file_name, "position": position }),
        )?;
        let mut items = Vec::with_capacity(raw.entries.len());
        for (index, entry) in raw.entries.into_iter().enumerate() {
            let id = CompletionItemId::new(
                u64::try_from(index)
                    .ok()
                    .and_then(|value| value.checked_add(1))
                    .context("completion item count overflow")?,
            )
            .context("completion item identity overflow")?;
            let inserted = entry
                .insert_text
                .clone()
                .unwrap_or_else(|| entry.name.clone());
            let primary_edit = entry.replacement_span.map(|range| TextEdit {
                range,
                new_text: inserted.clone(),
            });
            items.push(CompletionItem {
                id,
                label: entry.name,
                detail: None,
                documentation: None,
                kind: completion_kind(&entry.kind),
                deprecated: entry.deprecated,
                filter_text: None,
                sort_text: entry.sort_text,
                insert_text: Some(inserted),
                insert_text_format: InsertTextFormat::PlainText,
                primary_edit,
                additional_edits: Vec::new(),
            });
        }
        Ok(CompletionResult {
            is_incomplete: raw.is_incomplete,
            items,
        })
    }

    /// Returns quick information at an exact UTF-16 position.
    pub fn hover(
        &mut self,
        document_id: DocumentId,
        position: Utf16Position,
    ) -> Result<Option<HoverResult>> {
        self.validate_position(document_id, position)?;
        let file_name = self.file(document_id)?.file_name.clone();
        let raw: RawHoverResponse = self.call(
            "hover",
            &json!({ "fileName": file_name, "position": position }),
        )?;
        Ok(raw.hover.map(|hover| {
            let mut value = String::new();
            if !hover.display.is_empty() {
                value.push_str("```ts\n");
                value.push_str(&hover.display);
                value.push_str("\n```");
            }
            if !hover.documentation.is_empty() {
                if !value.is_empty() {
                    value.push_str("\n\n");
                }
                value.push_str(&hover.documentation);
            }
            HoverResult {
                range: hover.range,
                contents: MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                },
            }
        }))
    }

    /// Resolves definition targets that are part of the current project snapshot.
    pub fn definition(
        &mut self,
        document_id: DocumentId,
        position: Utf16Position,
    ) -> Result<DefinitionResult> {
        self.validate_position(document_id, position)?;
        let file_name = self.file(document_id)?.file_name.clone();
        let raw: RawDefinitions = self.call(
            "definition",
            &json!({ "fileName": file_name, "position": position }),
        )?;
        let targets = raw
            .definitions
            .into_iter()
            .filter_map(|definition| {
                let target_id = self.ids_by_file_name.get(&definition.file_name).copied()?;
                let file = self.files.get(&target_id)?;
                Some(DefinitionTarget {
                    document_id: target_id,
                    target_range: definition.range,
                    target_selection_range: definition.range,
                    analyzed_uri: Some(file.uri.clone()),
                })
            })
            .collect();
        Ok(DefinitionResult { targets })
    }

    /// Returns whole-document TypeScript formatting edits.
    pub fn formatting(
        &mut self,
        document_id: DocumentId,
        options: FormattingOptions,
    ) -> Result<FormattingResult> {
        let file_name = self.file(document_id)?.file_name.clone();
        let raw: RawFormatting = self.call(
            "format",
            &json!({
                "fileName": file_name,
                "options": {
                    "tabSize": options.tab_size,
                    "insertSpaces": options.insert_spaces,
                }
            }),
        )?;
        Ok(FormattingResult { edits: raw.edits })
    }

    fn file(&self, document_id: DocumentId) -> Result<&ProjectFile> {
        self.files
            .get(&document_id)
            .ok_or_else(|| anyhow!("unknown language-service document {document_id}"))
    }

    fn validate_position(&self, document_id: DocumentId, position: Utf16Position) -> Result<()> {
        position
            .to_byte_offset(&self.file(document_id)?.text)
            .context("validate language-service position")?;
        Ok(())
    }

    fn call<P, R>(&mut self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let request = serde_json::to_string(&json!({
            "method": method,
            "params": params,
        }))
        .context("serialize language-service request")?;
        let source = format!(
            "globalThis.__SMUDGY_LANGUAGE_SERVICE_HANDLE({})",
            serde_json::to_string(&request)?
        );
        let value = self
            .runtime
            .deno_runtime()
            .execute_script("[smudgy:lsp:request]", FastString::from(source))
            .with_context(|| format!("execute language-service {method}"))?;
        let response: String = {
            deno_core::scope!(scope, self.runtime.deno_runtime());
            let local = deno_core::v8::Local::new(scope, value);
            serde_v8::from_v8(scope, local)
                .with_context(|| format!("read language-service {method} response"))?
        };
        let response: DriverResponse<R> = serde_json::from_str(&response)
            .with_context(|| format!("decode language-service {method} response"))?;
        if response.ok {
            response
                .result
                .ok_or_else(|| anyhow!("language-service {method} returned no result"))
        } else {
            bail!(
                "language-service {method} failed: {}",
                response.error.unwrap_or_else(|| "unknown error".to_owned())
            )
        }
    }
}

impl Drop for EmbeddedLanguageService {
    fn drop(&mut self) {
        let result: Result<DisposeResult> = self.call("dispose", &json!({}));
        let _ = result;
    }
}

fn execute(runtime: &mut ScriptRuntime, name: &'static str, source: &str) -> Result<()> {
    runtime
        .deno_runtime()
        .execute_script(name, FastString::from(source.to_owned()))
        .with_context(|| format!("execute {name}"))?;
    Ok(())
}

fn normalize_file_name(value: &str) -> Result<String> {
    let replaced = value.replace('\\', "/");
    let mut parts = Vec::new();
    for part in replaced.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("language-service VFS path escapes its root: {value}");
                }
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        bail!("language-service VFS path cannot be empty");
    }
    Ok(format!("/{}", parts.join("/")))
}

/// Builds the only TypeScript standard-library surface visible to authored code.
///
/// `LIBS` contains every declaration shipped with the compiler because other users of
/// that generated table need the full set. The authoring service deliberately exposes
/// only ESNext and its recursive `/// <reference lib>` dependencies: making DOM or
/// WebWorker files visible to `fileExists` would let an authored reference directive opt
/// into host capabilities which Smudgy did not grant.
fn esnext_standard_libraries(
    available: &'static [(&'static str, &'static str)],
) -> Result<BTreeMap<String, Cow<'static, str>>> {
    const ROOT_NAME: &str = "lib.esnext.d.ts";

    let mut by_name = BTreeMap::new();
    for (name, contents) in available {
        if by_name.insert(*name, *contents).is_some() {
            bail!("duplicate vendored TypeScript library {name}");
        }
    }
    let root = *by_name
        .get(ROOT_NAME)
        .context("vendored lib.esnext.d.ts is missing")?;
    let mut libraries = BTreeMap::from([
        ("/lib.d.ts".to_owned(), Cow::Borrowed(root)),
        (format!("/{ROOT_NAME}"), Cow::Borrowed(root)),
    ]);
    let mut pending =
        referenced_lib_names(root).context("parse references in vendored lib.esnext.d.ts")?;
    let mut visited = BTreeSet::new();

    while let Some(reference) = pending.pop() {
        if !visited.insert(reference.clone()) {
            continue;
        }
        if is_host_environment_lib(&reference) {
            bail!(
                "vendored ESNext library closure unexpectedly references host library {reference}"
            );
        }
        let name = format!("lib.{reference}.d.ts");
        let contents = *by_name
            .get(name.as_str())
            .with_context(|| format!("vendored ESNext dependency {name} is missing"))?;
        pending.extend(
            referenced_lib_names(contents)
                .with_context(|| format!("parse references in vendored {name}"))?,
        );
        libraries.insert(format!("/{name}"), Cow::Borrowed(contents));
    }

    Ok(libraries)
}

fn referenced_lib_names(source: &str) -> Result<Vec<String>> {
    let mut references = Vec::new();
    for (line_index, line) in source.lines().enumerate() {
        let Some(directive) = line.trim_start().strip_prefix("///") else {
            continue;
        };
        let directive = directive.trim_start();
        let Some(attributes) = directive.strip_prefix("<reference") else {
            continue;
        };
        if !attributes.chars().next().is_some_and(|character| {
            character.is_ascii_whitespace() || matches!(character, '/' | '>')
        }) {
            continue;
        }
        let Some(tag_end) = attributes.find('>') else {
            bail!(
                "unterminated TypeScript reference directive on line {}",
                line_index + 1
            );
        };
        if let Some(name) =
            reference_attribute(&attributes[..tag_end], "lib").with_context(|| {
                format!(
                    "parse TypeScript reference directive on line {}",
                    line_index + 1
                )
            })?
        {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            {
                bail!("invalid TypeScript lib reference {name:?}");
            }
            references.push(name);
        }
    }
    Ok(references)
}

fn reference_attribute(attributes: &str, requested: &str) -> Result<Option<String>> {
    let bytes = attributes.as_bytes();
    let mut cursor = 0;
    let mut value = None;
    while cursor < bytes.len() {
        while cursor < bytes.len() && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b'/')
        {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }

        let name_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || matches!(bytes[cursor], b'-' | b'_'))
        {
            cursor += 1;
        }
        if cursor == name_start {
            bail!("invalid reference attribute name");
        }
        let name = &attributes[name_start..cursor];
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            bail!("reference attribute {name} has no value");
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let quote = *bytes
            .get(cursor)
            .filter(|quote| matches!(quote, b'\'' | b'"'))
            .with_context(|| format!("reference attribute {name} is not quoted"))?;
        cursor += 1;
        let value_start = cursor;
        while cursor < bytes.len() && bytes[cursor] != quote {
            cursor += 1;
        }
        if cursor == bytes.len() {
            bail!("reference attribute {name} is unterminated");
        }
        let attribute_value = &attributes[value_start..cursor];
        cursor += 1;

        if name.eq_ignore_ascii_case(requested)
            && value.replace(attribute_value.to_owned()).is_some()
        {
            bail!("duplicate reference attribute {requested}");
        }
    }
    Ok(value)
}

fn is_host_environment_lib(name: &str) -> bool {
    name == "dom"
        || name.starts_with("dom.")
        || name == "webworker"
        || name.starts_with("webworker.")
        || name == "scripthost"
}

fn diagnostic_severity(category: &str) -> DiagnosticSeverity {
    match category {
        "Error" => DiagnosticSeverity::Error,
        "Warning" => DiagnosticSeverity::Warning,
        "Suggestion" => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Information,
    }
}

fn completion_kind(kind: &str) -> CompletionKind {
    match kind {
        "method" => CompletionKind::Method,
        "function" | "local function" => CompletionKind::Function,
        "constructor" => CompletionKind::Constructor,
        "property" | "getter" | "setter" | "JSX attribute" => CompletionKind::Property,
        "class" => CompletionKind::Class,
        "interface" | "type" => CompletionKind::Interface,
        "module" | "external module name" => CompletionKind::Module,
        "enum" => CompletionKind::Enum,
        "enum member" => CompletionKind::EnumMember,
        "const" => CompletionKind::Constant,
        "var" | "let" | "local var" | "parameter" => CompletionKind::Variable,
        "type parameter" => CompletionKind::TypeParameter,
        "keyword" | "primitive type" => CompletionKind::Keyword,
        "directory" => CompletionKind::Folder,
        "script" => CompletionKind::File,
        "alias" => CompletionKind::Reference,
        _ => CompletionKind::Text,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriverResponse<T> {
    ok: bool,
    result: Option<T>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
    typescript_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaceProjectResult {
    #[allow(dead_code)]
    project_version: u64,
    #[allow(dead_code)]
    roots: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisposeResult {
    #[allow(dead_code)]
    disposed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDiagnostics {
    diagnostics: Vec<RawDiagnostic>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDiagnostic {
    #[allow(dead_code)]
    file_name: Option<String>,
    code: i64,
    category: String,
    message: String,
    range: Option<Utf16Range>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCompletion {
    is_incomplete: bool,
    entries: Vec<RawCompletionEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCompletionEntry {
    name: String,
    kind: String,
    sort_text: Option<String>,
    insert_text: Option<String>,
    replacement_span: Option<Utf16Range>,
    deprecated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHover {
    range: Option<Utf16Range>,
    display: String,
    documentation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHoverResponse {
    hover: Option<RawHover>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDefinitions {
    definitions: Vec<RawDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDefinition {
    file_name: String,
    range: Utf16Range,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFormatting {
    edits: Vec<TextEdit>,
}

struct TemporaryDataDir(PathBuf);

impl TemporaryDataDir {
    fn new() -> Result<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir();
        for _ in 0..100 {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(
                "smudgy-language-service-{}-{id}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("create {}", path.display()));
                }
            }
        }
        bail!("could not allocate a unique language-service data directory")
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDataDir {
    fn drop(&mut self) {
        let Some(name) = self.0.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let expected_prefix = format!("smudgy-language-service-{}-", std::process::id());
        if !name.starts_with(&expected_prefix)
            || self.0.parent() != Some(std::env::temp_dir().as_path())
        {
            log::error!(
                "refusing to remove unexpected language-service directory {}",
                self.0.display()
            );
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "failed to remove language-service directory {}: {error}",
                    self.0.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document_id(byte: u8) -> DocumentId {
        DocumentId::try_from([byte; 16]).expect("non-nil test document ID")
    }

    fn file(byte: u8, name: &str, language: Language, text: &str) -> ProjectFile {
        ProjectFile::new(
            document_id(byte),
            name,
            format!("smudgy-test://{name}"),
            language,
            text,
        )
        .expect("valid project file")
    }

    #[test]
    fn embedded_service_provides_project_aware_typescript_intelligence() {
        let mut service = EmbeddedLanguageService::new().expect("boot language service");
        service
            .replace_project(vec![
                file(
                    1,
                    "/dep.ts",
                    Language::TypeScript,
                    "export function greet(name: string): string { return `Hello ${name}`; }\n",
                ),
                file(
                    2,
                    "/main.ts",
                    Language::TypeScript,
                    concat!(
                        "import { greet } from \"./dep.ts\";\n",
                        "const rocket = \"🚀\";\n",
                        "const wrong: number = \"bad\";\n",
                        "greet(rocket);\n",
                    ),
                ),
            ])
            .expect("open project");

        let diagnostics = service
            .diagnostics(document_id(2))
            .expect("typescript diagnostics");
        assert!(diagnostics.items.iter().any(|item| {
            item.code == Some(DiagnosticCode::Number(2322))
                && item.severity == DiagnosticSeverity::Error
        }));

        let hover = service
            .hover(
                document_id(2),
                Utf16Position {
                    line: 3,
                    character: 1,
                },
            )
            .expect("hover request")
            .expect("hover result");
        assert!(hover.contents.value.contains("greet"));

        let definitions = service
            .definition(
                document_id(2),
                Utf16Position {
                    line: 3,
                    character: 1,
                },
            )
            .expect("definition request");
        assert_eq!(definitions.targets.len(), 1);
        assert_eq!(definitions.targets[0].document_id, document_id(1));

        service
            .replace_project(vec![
                file(
                    1,
                    "/dep.ts",
                    Language::TypeScript,
                    "export function greet(name: string): string { return `Hello ${name}`; }\n",
                ),
                file(
                    2,
                    "/main.ts",
                    Language::TypeScript,
                    concat!(
                        "interface User { name: string; age: number }\n",
                        "const user: User = { name: \"Ada\", age: 37 };\n",
                        "user.na\n",
                    ),
                ),
            ])
            .expect("replace project");
        let completion = service
            .completion(
                document_id(2),
                Utf16Position {
                    line: 2,
                    character: 7,
                },
            )
            .expect("completion request");
        let name = completion
            .items
            .iter()
            .find(|item| item.label == "name")
            .expect("name completion");
        assert_eq!(
            name.primary_edit.as_ref().map(|edit| edit.range),
            Some(Utf16Range {
                start: Utf16Position {
                    line: 2,
                    character: 5,
                },
                end: Utf16Position {
                    line: 2,
                    character: 7,
                },
            })
        );

        service
            .replace_project(vec![file(
                2,
                "/main.ts",
                Language::TypeScript,
                "const alpha = 1;\nal\n",
            )])
            .expect("replace completion-prefix project");
        let completion = service
            .completion(
                document_id(2),
                Utf16Position {
                    line: 1,
                    character: 2,
                },
            )
            .expect("prefix-filtered completion request");
        let alpha = completion
            .items
            .iter()
            .take(8)
            .find(|item| item.label == "alpha")
            .expect("typed prefix keeps alpha in the visible completion budget");
        assert_eq!(
            alpha.primary_edit.as_ref().map(|edit| edit.range),
            Some(Utf16Range {
                start: Utf16Position {
                    line: 1,
                    character: 0,
                },
                end: Utf16Position {
                    line: 1,
                    character: 2,
                },
            })
        );

        let formatting = service
            .formatting(
                document_id(2),
                FormattingOptions {
                    tab_size: 2,
                    insert_spaces: true,
                },
            )
            .expect("formatting request");
        assert!(!formatting.edits.is_empty());
    }

    #[test]
    fn embedded_service_loads_rooted_immutable_declaration_libraries() {
        let mut service = EmbeddedLanguageService::new_with_libraries(vec![
            LanguageServiceLibrary {
                file_name: "/types/payload.d.ts".to_owned(),
                text: Cow::Borrowed("interface LibraryPayload { name: string; age: number }\n"),
                is_root: false,
            },
            LanguageServiceLibrary {
                file_name: "/types/smudgy-test.d.ts".to_owned(),
                text: Cow::Borrowed(
                    "/// <reference path=\"./payload.d.ts\" />\n\
                     declare module \"smudgy:test\" {\n\
                       export function make(): LibraryPayload;\n\
                     }\n",
                ),
                is_root: true,
            },
        ])
        .expect("boot language service with immutable declarations");
        service
            .replace_project(vec![file(
                1,
                "/main.ts",
                Language::TypeScript,
                "import { make } from \"smudgy:test\";\n\
                 const item = make();\n\
                 item.na\n\
                 const wrong: number = item.name;\n",
            )])
            .expect("open library consumer");

        let diagnostics = service
            .diagnostics(document_id(1))
            .expect("library-backed diagnostics");
        assert!(
            !diagnostics
                .items
                .iter()
                .any(|item| item.code == Some(DiagnosticCode::Number(2307))),
            "the ambient module root must resolve"
        );
        assert!(
            diagnostics
                .items
                .iter()
                .any(|item| item.code == Some(DiagnosticCode::Number(2322))),
            "the referenced non-root declaration must supply LibraryPayload"
        );

        let completion = service
            .completion(
                document_id(1),
                Utf16Position {
                    line: 2,
                    character: 7,
                },
            )
            .expect("library-backed completion");
        assert!(completion.items.iter().any(|item| item.label == "name"));
    }

    #[test]
    fn esnext_library_closure_parses_reference_format_variants_and_requires_dependencies() {
        const COMPLETE: &[(&str, &str)] = &[
            (
                "lib.esnext.d.ts",
                "///\t<reference other='ignored' LIB = 'ES2023'/>\n",
            ),
            (
                "lib.es2023.d.ts",
                "/// <reference no-default-lib=\"true\" />\n",
            ),
        ];
        let libraries = esnext_standard_libraries(COMPLETE).expect("resolve fixture closure");
        assert_eq!(
            libraries.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["/lib.d.ts", "/lib.es2023.d.ts", "/lib.esnext.d.ts"]
        );

        const MISSING: &[(&str, &str)] = &[(
            "lib.esnext.d.ts",
            "/// <reference lib=\"es2099.missing\" />\n",
        )];
        let error = esnext_standard_libraries(MISSING)
            .expect_err("a missing transitive standard library must fail initialization");
        assert!(error.to_string().contains("lib.es2099.missing.d.ts"));
    }

    #[test]
    fn esnext_library_closure_never_exposes_host_environment_libraries() {
        let libraries = esnext_standard_libraries(LIBS).expect("resolve vendored ESNext closure");
        assert!(libraries.contains_key("/lib.d.ts"));
        assert!(libraries.contains_key("/lib.esnext.d.ts"));
        assert!(libraries.contains_key("/lib.es5.d.ts"));
        for unavailable in [
            "/lib.dom.d.ts",
            "/lib.dom.iterable.d.ts",
            "/lib.esnext.full.d.ts",
            "/lib.scripthost.d.ts",
            "/lib.webworker.d.ts",
            "/lib.webworker.importscripts.d.ts",
        ] {
            assert!(
                !libraries.contains_key(unavailable),
                "host library {unavailable} must not be resolver-visible"
            );
        }
    }

    #[test]
    fn authored_dom_lib_reference_cannot_enable_web_audio() {
        let mut service = EmbeddedLanguageService::new().expect("boot language service");
        service
            .replace_project(vec![file(
                1,
                "/audio.ts",
                Language::TypeScript,
                "/// <reference lib=\"dom\" />\nconst context = new AudioContext();\n",
            )])
            .expect("open audio consumer");

        let diagnostics = service
            .diagnostics(document_id(1))
            .expect("audio diagnostics");
        assert!(diagnostics.items.iter().any(|item| {
            item.code == Some(DiagnosticCode::Number(2304)) && item.message.contains("AudioContext")
        }));
    }

    #[test]
    fn authored_webworker_lib_reference_cannot_widen_smudgys_worker_contract() {
        let mut service =
            EmbeddedLanguageService::new_with_libraries(vec![LanguageServiceLibrary {
                file_name: "/smudgy/narrow-worker.d.ts".to_owned(),
                text: Cow::Borrowed(
                    "interface Worker {}\n\
                     interface WorkerOptions { type: \"module\"; name?: string; }\n\
                     declare var Worker: {\n\
                       readonly prototype: Worker;\n\
                       new(specifier: string, options: WorkerOptions): Worker;\n\
                     };\n",
                ),
                is_root: true,
            }])
            .expect("boot language service with narrowed Worker declaration");
        service
            .replace_project(vec![file(
                1,
                "/worker.ts",
                Language::TypeScript,
                "/// <reference lib=\"webworker\" />\n\
                 new Worker(\"worker.ts\");\n\
                 new Worker(\"worker.ts\", { type: \"classic\" });\n",
            )])
            .expect("open worker consumer");

        let diagnostics = service
            .diagnostics(document_id(1))
            .expect("worker diagnostics");
        assert!(
            diagnostics
                .items
                .iter()
                .any(|item| item.code == Some(DiagnosticCode::Number(2554))),
            "the Smudgy Worker constructor must continue to require options"
        );
        assert!(
            diagnostics.items.iter().any(|item| {
                item.code == Some(DiagnosticCode::Number(2322)) && item.message.contains("classic")
            }),
            "the Smudgy Worker constructor must continue to require module workers"
        );
    }

    #[test]
    fn exact_materialized_relative_imports_cover_static_dynamic_and_module_extensions() {
        let mut service = EmbeddedLanguageService::new().expect("boot language service");
        service
            .replace_project(vec![
                file(
                    10,
                    "/projects/7/8/modules/static.mts",
                    Language::TypeScript,
                    "export const esmValue = 1;\n",
                ),
                file(
                    11,
                    "/projects/7/8/modules/static.cts",
                    Language::TypeScript,
                    "export const commonValue = 2;\n",
                ),
                file(
                    12,
                    "/projects/7/8/modules/types.d.mts",
                    Language::TypeScript,
                    "export interface EsmShape { value: number }\n",
                ),
                file(
                    13,
                    "/projects/7/8/modules/types.d.cts",
                    Language::TypeScript,
                    "export interface CommonShape { value: number }\n",
                ),
                file(
                    14,
                    "/projects/7/8/modules/dynamic.mjs",
                    Language::JavaScript,
                    "export const dynamicEsm = 3;\n",
                ),
                file(
                    15,
                    "/projects/7/8/modules/dynamic.cjs",
                    Language::JavaScript,
                    "export const dynamicCommon = 4;\n",
                ),
                file(
                    16,
                    "/projects/7/8/modules/main.ts",
                    Language::TypeScript,
                    concat!(
                        "import { esmValue } from \"./static.mts\";\n",
                        "import { commonValue } from \"./static.cts\";\n",
                        "import type { EsmShape } from \"./types.d.mts\";\n",
                        "import type { CommonShape } from \"./types.d.cts\";\n",
                        "void import(\"./dynamic.mjs\");\n",
                        "void import(\"./dynamic.cjs\");\n",
                        "const values: [number, number, EsmShape, CommonShape] = [\n",
                        "  esmValue, commonValue, { value: 1 }, { value: 2 },\n",
                        "];\n",
                    ),
                ),
            ])
            .expect("open exact-import project");

        let diagnostics = service
            .diagnostics(document_id(16))
            .expect("exact relative-import diagnostics");
        assert!(
            !diagnostics
                .items
                .iter()
                .any(|item| item.code == Some(DiagnosticCode::Number(2307))),
            "every explicitly materialized static and dynamic target must resolve"
        );
    }

    #[test]
    fn extensionless_relative_misses_and_actionable_auto_imports_stay_hidden() {
        let mut service = EmbeddedLanguageService::new().expect("boot language service");
        service
            .replace_project(vec![
                file(
                    20,
                    "/projects/9/10/modules/dep.ts",
                    Language::TypeScript,
                    "export const exportedValue = 1;\n",
                ),
                file(
                    21,
                    "/projects/9/10/modules/hidden.ts",
                    Language::TypeScript,
                    "export const hiddenAutoImport = 2;\n",
                ),
                file(
                    22,
                    "/projects/9/10/modules/miss.ts",
                    Language::TypeScript,
                    "import { exportedValue } from \"./dep\";\nvoid exportedValue;\n",
                ),
                file(
                    23,
                    "/projects/9/10/modules/completion.ts",
                    Language::TypeScript,
                    concat!(
                        "import { exp } from \"./dep.ts\";\n",
                        "const local = { memberValue: 1 };\n",
                        "local.mem\n",
                        "hiddenAu\n",
                    ),
                ),
            ])
            .expect("open exact-resolution completion project");

        let diagnostics = service
            .diagnostics(document_id(22))
            .expect("extensionless relative-import diagnostics");
        assert!(
            diagnostics
                .items
                .iter()
                .any(|item| item.code == Some(DiagnosticCode::Number(2307))),
            "an extensionless specifier must not guess the materialized .ts target"
        );

        let explicit_import = service
            .completion(
                document_id(23),
                Utf16Position {
                    line: 0,
                    character: 12,
                },
            )
            .expect("explicit import-clause completion");
        assert!(
            explicit_import
                .items
                .iter()
                .any(|item| item.label == "exportedValue"),
            "explicit import-clause completions remain directly insertable"
        );

        let member = service
            .completion(
                document_id(23),
                Utf16Position {
                    line: 2,
                    character: 9,
                },
            )
            .expect("member completion");
        assert!(
            member.items.iter().any(|item| item.label == "memberValue"),
            "member completions remain available"
        );

        let auto_import = service
            .completion(
                document_id(23),
                Utf16Position {
                    line: 3,
                    character: 8,
                },
            )
            .expect("auto-import completion exclusion");
        assert!(
            !auto_import
                .items
                .iter()
                .any(|item| item.label == "hiddenAutoImport"),
            "entries that require an unrepresentable import action must stay hidden"
        );
    }

    #[test]
    fn embedded_service_uses_lsp_lines_and_utf16_columns() {
        let mut service = EmbeddedLanguageService::new().expect("boot language service");
        service
            .replace_project(vec![file(
                1,
                "/line-endings.ts",
                Language::TypeScript,
                concat!(
                    "const target = 1;\r",
                    "const wrong: number = \"😀\";\u{2028}",
                    "target;\u{2029}",
                ),
            )])
            .expect("open mixed-line-ending project");

        let diagnostics = service
            .diagnostics(document_id(1))
            .expect("mixed-line-ending diagnostics");
        let mismatch = diagnostics
            .items
            .iter()
            .find(|diagnostic| diagnostic.code == Some(DiagnosticCode::Number(2322)))
            .expect("type mismatch diagnostic");
        assert_eq!(mismatch.range.start.line, 1);

        let definitions = service
            .definition(
                document_id(1),
                Utf16Position {
                    line: 2,
                    character: 1,
                },
            )
            .expect("definition after Unicode line separator");
        assert_eq!(definitions.targets.len(), 1);
        assert_eq!(definitions.targets[0].document_id, document_id(1));
        assert_eq!(definitions.targets[0].target_range.start.line, 0);
    }

    #[test]
    fn truncated_completion_result_is_marked_incomplete() {
        let mut service = EmbeddedLanguageService::new().expect("boot language service");
        let mut text = String::new();
        for index in 0..650 {
            use std::fmt::Write as _;
            writeln!(text, "const item{index} = {index};").expect("write fixture source");
        }
        text.push_str("ite");
        service
            .replace_project(vec![file(
                1,
                "/completion-cap.ts",
                Language::TypeScript,
                &text,
            )])
            .expect("open completion project");

        let result = service
            .completion(
                document_id(1),
                Utf16Position {
                    line: 650,
                    character: 3,
                },
            )
            .expect("bounded completion result");
        assert_eq!(result.items.len(), 500);
        assert!(result.is_incomplete);
    }

    #[test]
    fn project_snapshot_rejects_duplicate_authority() {
        let mut service = EmbeddedLanguageService::new().expect("boot language service");
        let error = service
            .replace_project(vec![
                file(1, "/one.ts", Language::TypeScript, "export {};"),
                file(1, "/two.ts", Language::TypeScript, "export {};"),
            ])
            .expect_err("duplicate document ID must fail");
        assert!(
            error
                .to_string()
                .contains("duplicate language-service document")
        );
    }
}
