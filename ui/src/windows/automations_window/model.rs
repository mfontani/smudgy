//! Data model for the Automations window: the script tree, the status model,
//! and the (client-side) package dependency graph derivations described in
//! `docs/new-automations-window.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use smudgy_core::models::shared_packages::LockedPackage;
use smudgy_core::models::{self, aliases, hotkeys, packages, triggers};
use smudgy_core::session::runtime::{
    AutomationBody, AutomationDelta, AutomationKind, AutomationSummary, Origin,
};

use super::{AutomationsWindow, Message};

/// Live script-created automations for the open session, kept in sync from the per-session
/// automation broadcast and keyed by creator [`Origin`]. The sidebar nests each creator's
/// aliases/triggers under its module/package node. Disk-authored automations are not here
/// (they come from the on-disk model); only `Module`/`Package`-origin ones are streamed.
#[derive(Default)]
pub struct LiveAutomations {
    by_origin: HashMap<Origin, CreatorAutomations>,
}

/// One creator's script-created aliases/triggers (name → live detail).
#[derive(Default)]
pub struct CreatorAutomations {
    pub aliases: BTreeMap<String, LiveAutomation>,
    pub triggers: BTreeMap<String, LiveAutomation>,
}

/// A single script-created automation's live state, mirrored from the runtime's introspection
/// stream — its on/off flag plus the read-only pattern/body shown in the detail pane.
#[derive(Clone)]
pub struct LiveAutomation {
    pub enabled: bool,
    /// Match pattern(s), joined for display. Empty when it has none.
    pub pattern: Arc<str>,
    /// What it does (command text, script source, or none). Display-only.
    pub body: AutomationBody,
}

impl LiveAutomations {
    /// Replace all state from a fresh snapshot (sent when the window subscribes, and after a
    /// session reload).
    pub fn reset(&mut self, summaries: &[AutomationSummary]) {
        self.by_origin.clear();
        for summary in summaries {
            self.upsert(summary);
        }
    }

    /// Apply a batch of incremental changes on top of the current state.
    pub fn apply(&mut self, deltas: &[AutomationDelta]) {
        for delta in deltas {
            match delta {
                AutomationDelta::Upserted(summary) => self.upsert(summary),
                AutomationDelta::EnabledChanged {
                    kind,
                    origin,
                    name,
                    enabled,
                } => {
                    if let Some(creator) = self.by_origin.get_mut(origin)
                        && let Some(slot) = creator.map_mut(*kind).get_mut(name)
                    {
                        slot.enabled = *enabled;
                    }
                }
                AutomationDelta::Removed { kind, origin, name } => {
                    if let Some(creator) = self.by_origin.get_mut(origin) {
                        creator.map_mut(*kind).remove(name);
                    }
                }
            }
        }
    }

    fn upsert(&mut self, summary: &AutomationSummary) {
        let creator = self.by_origin.entry(summary.origin.clone()).or_default();
        creator.map_mut(summary.kind).insert(
            summary.name.clone(),
            LiveAutomation {
                enabled: summary.enabled,
                pattern: summary.pattern.clone(),
                body: summary.body.clone(),
            },
        );
    }

    /// A local module's automations, keyed by its `modules/`-relative subpath.
    pub fn module(&self, subpath: &str) -> Option<&CreatorAutomations> {
        self.by_origin.get(&Origin::Module {
            subpath: subpath.to_string(),
        })
    }

    /// An installed package's automations, matched by owner/name across any resolved version.
    pub fn package(&self, owner: &str, name: &str) -> Option<&CreatorAutomations> {
        self.by_origin
            .iter()
            .find_map(|(origin, creator)| match origin {
                Origin::Package {
                    owner: o, name: n, ..
                } if o == owner && n == name => Some(creator),
                _ => None,
            })
    }
}

impl CreatorAutomations {
    fn map_mut(&mut self, kind: AutomationKind) -> &mut BTreeMap<String, LiveAutomation> {
        match kind {
            AutomationKind::Alias => &mut self.aliases,
            AutomationKind::Trigger => &mut self.triggers,
            // Hotkeys are not streamed as automation deltas (they live in the runtime's own
            // `HotkeyId` map, not the trigger introspection mirror), so this is never reached.
            AutomationKind::Hotkey => unreachable!("hotkeys are not tracked as automation deltas"),
        }
    }
}

/// A leaf automation or a folder, mirroring the on-disk model. Folders hold a
/// nested map of children (the tree is built from each script's `package` path).
#[derive(Debug, Clone)]
pub enum Script {
    Alias(models::aliases::AliasDefinition),
    Hotkey(models::hotkeys::HotkeyDefinition),
    Trigger(models::triggers::TriggerDefinition),
    /// A folder of nested children. Folder enable state lives in the package
    /// tree (`packages.json`), not here; the placeholder `bool` mirrors the
    /// loader's shape and is intentionally never read.
    Folder(#[allow(dead_code)] bool, BTreeMap<String, Script>),
}

impl Script {
    /// The folder (package path) this script lives under, if any.
    pub fn folder_name(&self) -> Option<&str> {
        match self {
            Script::Alias(a) => a.package.as_deref(),
            Script::Hotkey(h) => h.package.as_deref(),
            Script::Trigger(t) => t.package.as_deref(),
            Script::Folder(_, _) => None,
        }
    }

    /// This node's own `enabled` flag (folders report `true`).
    pub fn own_enabled(&self) -> bool {
        match self {
            Script::Alias(a) => a.enabled,
            Script::Hotkey(h) => h.enabled,
            Script::Trigger(t) => t.enabled,
            Script::Folder(_, _) => true,
        }
    }
}

/// Identifies a script by its (folder, name) pair for tree selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptKey {
    pub folder_name: Option<String>,
    pub script_name: String,
}

/// A node's at-a-glance health. Drives the colored status dot everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// Enabled & healthy (green).
    Ok,
    /// Enabled but broken — e.g. a pattern won't compile (red).
    Error,
    /// Needs attention but not broken (orange/amber) — e.g. an installed package whose newest
    /// version is held back because it demands more permissions than were granted (the update is
    /// blocked until the user reviews + grants it, `PACKAGE-ISOLATES-CONSENT-TRUST.md`).
    Warning,
    /// Turned off; won't run (grey).
    Disabled,
}

/// The per-row matcher role in the unified trigger row list (`MatcherRole` in
/// the persisted sidecar; `Anti` renders as "Exceptions" in the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// Must match the line for the trigger to fire.
    Match,
    /// A veto: any match prevents the trigger from firing.
    Anti,
    /// Matched against the raw line, color codes included (regex only).
    Raw,
}

impl PatternKind {
    fn role(self) -> matchers::MatcherRole {
        match self {
            PatternKind::Match => matchers::MatcherRole::Match,
            PatternKind::Anti => matchers::MatcherRole::Anti,
            PatternKind::Raw => matchers::MatcherRole::Raw,
        }
    }

    fn from_role(role: matchers::MatcherRole) -> Self {
        match role {
            matchers::MatcherRole::Match => PatternKind::Match,
            matchers::MatcherRole::Anti => PatternKind::Anti,
            matchers::MatcherRole::Raw => PatternKind::Raw,
        }
    }
}

use smudgy_core::models::matchers::{
    self, AliasMatcherSource, ArgKind, ArgSpec, CmdMode, MatcherSyntax, ParseMode,
    TriggerMatcherSource,
};

/// A `MatcherSyntax` wrapper carrying the pick-list `Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxChoice(pub MatcherSyntax);

impl SyntaxChoice {
    pub const ALL: [SyntaxChoice; 2] = [
        SyntaxChoice(MatcherSyntax::Pattern),
        SyntaxChoice(MatcherSyntax::Regex),
    ];
}

impl std::fmt::Display for SyntaxChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            MatcherSyntax::Pattern => crate::i18n::ts!("editor-syntax-pattern"),
            MatcherSyntax::Regex => crate::i18n::ts!("editor-syntax-regex"),
        })
    }
}

/// An `ArgKind` wrapper carrying the pick-list `Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgKindChoice(pub ArgKind);

impl std::fmt::Display for ArgKindChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            ArgKind::Required => crate::i18n::ts!("editor-arg-required"),
            ArgKind::Optional => crate::i18n::ts!("editor-arg-optional"),
            ArgKind::Rest => crate::i18n::ts!("editor-arg-rest"),
        })
    }
}

/// A `ParseMode` wrapper carrying the pick-list `Display` (label only; the
/// prototype's two-line example rows are the deferred overlay-picker design).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseModeChoice(pub ParseMode);

impl ParseModeChoice {
    pub const ALL: [ParseModeChoice; 5] = [
        ParseModeChoice(ParseMode::Spaces),
        ParseModeChoice(ParseMode::Quotes),
        ParseModeChoice(ParseMode::Braces),
        ParseModeChoice(ParseMode::All),
        ParseModeChoice(ParseMode::Raw),
    ];
}

impl std::fmt::Display for ParseModeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            ParseMode::Spaces => crate::i18n::ts!("editor-parse-spaces"),
            ParseMode::Quotes => crate::i18n::ts!("editor-parse-quotes"),
            ParseMode::Braces => crate::i18n::ts!("editor-parse-braces"),
            ParseMode::All => crate::i18n::ts!("editor-parse-all"),
            ParseMode::Raw => crate::i18n::ts!("editor-parse-raw"),
        })
    }
}

/// The alias editor's matcher kind (the three type cards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasKind {
    Command,
    Pattern,
    Regex,
}

/// The trigger pane's three matcher cards (README §4): teaching cards while
/// no matcher exists, a kind+role selector while exactly one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerCard {
    Pattern,
    Regex,
    Raw,
}

impl TriggerCard {
    /// The `(syntax, role)` a card stands for.
    pub fn shape(self) -> (MatcherSyntax, PatternKind) {
        match self {
            TriggerCard::Pattern => (MatcherSyntax::Pattern, PatternKind::Match),
            TriggerCard::Regex => (MatcherSyntax::Regex, PatternKind::Match),
            TriggerCard::Raw => (MatcherSyntax::Regex, PatternKind::Raw),
        }
    }

    /// The card describing an existing matcher row, for the selector state.
    pub fn of_row(row: &TriggerRow) -> Self {
        if row.role == PatternKind::Raw {
            TriggerCard::Raw
        } else if row.syntax == MatcherSyntax::Pattern {
            TriggerCard::Pattern
        } else {
            TriggerCard::Regex
        }
    }
}

/// The alias editor's matcher draft: the selected kind plus every kind's
/// buffers, so switching type cards never destroys work. Lives on the window
/// (like the hotkey capture state) and is seeded on open/create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasMatcherDraft {
    pub kind: AliasKind,
    /// The pinned command word, when the author chose one that differs from
    /// the alias's name. `None` — the ordinary case — inherits the name, and
    /// the Command row stays out of the deck entirely. `Some("")` is the row
    /// revealed but not yet filled, which still inherits.
    pub command_override: Option<String>,
    pub args: Vec<ArgSpec>,
    pub parse: ParseMode,
    pub cmd_mode: CmdMode,
    pub pattern_source: String,
    pub anchor_start: bool,
    pub anchor_end: bool,
    pub regex_source: String,
    /// A stale sidecar degraded this matcher to Regex on load — the stored
    /// pattern was edited by hand, and the hand edit wins.
    pub degraded: bool,
}

impl Default for AliasMatcherDraft {
    fn default() -> Self {
        Self {
            // Command is the default for NEW aliases; opening an existing one
            // reseeds via `from_definition`.
            kind: AliasKind::Command,
            command_override: None,
            args: Vec::new(),
            parse: ParseMode::All,
            cmd_mode: CmdMode::Simple,
            pattern_source: String::new(),
            anchor_start: true,
            anchor_end: true,
            regex_source: String::new(),
            degraded: false,
        }
    }
}

impl AliasMatcherDraft {
    /// Seed the draft from a stored definition. The sidecar drives the editor
    /// only while it still compiles to the stored pattern; on a mismatch (a
    /// hand-edited `pattern`, or a lying package sidecar) the matcher degrades
    /// to Regex showing the stored pattern verbatim — the hand edit wins.
    pub fn from_definition(alias: &aliases::AliasDefinition, alias_name: &str) -> Self {
        let mut draft = Self {
            kind: AliasKind::Regex,
            regex_source: alias.pattern.clone(),
            ..Self::default()
        };
        let Some(source) = &alias.matcher else {
            return draft;
        };
        let fresh = matchers::alias_pattern(source, alias_name)
            .is_ok_and(|derived| derived == alias.pattern);
        if !fresh {
            draft.degraded = true;
            return draft;
        }
        match source {
            AliasMatcherSource::Command {
                name,
                args,
                parse,
                mode,
            } => {
                draft.kind = AliasKind::Command;
                draft.command_override.clone_from(name);
                draft.args.clone_from(args);
                draft.parse = *parse;
                draft.cmd_mode = *mode;
            }
            AliasMatcherSource::Pattern {
                source,
                anchor_start,
                anchor_end,
            } => {
                draft.kind = AliasKind::Pattern;
                draft.pattern_source.clone_from(source);
                draft.anchor_start = *anchor_start;
                draft.anchor_end = *anchor_end;
            }
        }
        draft
    }

    /// The sidecar this draft saves (`None` for the Regex kind — an absent
    /// sidecar IS the Regex kind).
    pub fn to_matcher(&self) -> Option<AliasMatcherSource> {
        match self.kind {
            AliasKind::Regex => None,
            AliasKind::Command => Some(AliasMatcherSource::Command {
                // A revealed-but-blank override is no override: clearing the
                // field collapses back to inheriting the name.
                name: self
                    .command_override
                    .as_deref()
                    .map(str::trim)
                    .filter(|word| !word.is_empty())
                    .map(str::to_string),
                args: self.args.clone(),
                parse: self.parse,
                mode: self.cmd_mode,
            }),
            AliasKind::Pattern => Some(AliasMatcherSource::Pattern {
                source: self.pattern_source.clone(),
                anchor_start: self.anchor_start,
                anchor_end: self.anchor_end,
            }),
        }
    }

    /// The word a Command draft matches on: its override, or the alias's own
    /// name. Both are trimmed, and a blank override inherits.
    pub fn command_word<'a>(&'a self, alias_name: &'a str) -> &'a str {
        self.command_override
            .as_deref()
            .map(str::trim)
            .filter(|word| !word.is_empty())
            .unwrap_or_else(|| alias_name.trim())
    }

    /// Whether the Command row belongs in the deck: once an override is
    /// pinned it stays visible, and it reveals itself unasked when the name
    /// cannot serve as a command word — otherwise the alias could never fire
    /// and nothing would say why.
    pub fn shows_command_override(&self, alias_name: &str) -> bool {
        self.command_override.is_some() || alias_name.trim().contains(char::is_whitespace)
    }

    /// The stored pattern this draft saves, or a display-ready error.
    pub fn to_pattern(&self, alias_name: &str) -> Result<String, String> {
        if self.kind == AliasKind::Command {
            let word = self.command_word(alias_name);
            if word.is_empty() {
                return Err(crate::i18n::t!("editor-command-name-empty"));
            }
            // The parser compares the first whitespace-delimited token, so a
            // word with a space in it can never match anything.
            if word.contains(char::is_whitespace) {
                return Err(crate::i18n::t!("editor-command-name-spaces"));
            }
        }
        match self.to_matcher() {
            None => Ok(self.regex_source.clone()),
            Some(matcher) => matchers::alias_pattern(&matcher, alias_name)
                .map_err(|errors| pattern_error_text(&errors[0])),
        }
    }

    /// README §2 invariants on the argument rows, applied after every args
    /// mutation: `Rest` only in the last position (earlier ones demote to
    /// `Optional`), and Simple mode forces every argument `Optional` with the
    /// last one `Rest`.
    pub fn normalize_args(&mut self) {
        let len = self.args.len();
        for (i, arg) in self.args.iter_mut().enumerate() {
            if arg.kind == ArgKind::Rest && i + 1 != len {
                arg.kind = ArgKind::Optional;
            }
        }
        if self.cmd_mode == CmdMode::Simple {
            for (i, arg) in self.args.iter_mut().enumerate() {
                arg.kind = if i + 1 == len {
                    ArgKind::Rest
                } else {
                    ArgKind::Optional
                };
            }
        }
    }
}

/// A display string for one pattern-compilation error.
pub fn pattern_error_text(error: &matchers::PatternError) -> String {
    match error {
        matchers::PatternError::NumberedHole { body } => {
            crate::i18n::t!("editor-numbered-hole", "body" => body.clone())
        }
        matchers::PatternError::UnknownHoleType { body } => {
            crate::i18n::t!("editor-unknown-hole-type", "body" => body.clone())
        }
        matchers::PatternError::Engine { message } => {
            crate::i18n::t!("editor-invalid-regex", "error" => message.clone())
        }
    }
}

/// One trigger matcher row in the editor: role, syntax, source, and the
/// Pattern-syntax anchor checkboxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerRow {
    pub role: PatternKind,
    pub syntax: MatcherSyntax,
    pub source: String,
    pub anchor_start: bool,
    pub anchor_end: bool,
}

impl TriggerRow {
    pub fn new(role: PatternKind) -> Self {
        Self {
            role,
            syntax: MatcherSyntax::Regex,
            source: String::new(),
            anchor_start: true,
            anchor_end: true,
        }
    }

    /// The stored regex this row derives to, or a display-ready error.
    pub fn compiled(&self) -> Result<String, String> {
        match self.syntax {
            MatcherSyntax::Pattern => {
                let compiled =
                    matchers::compile_pattern(&self.source, self.anchor_start, self.anchor_end);
                match compiled.errors.first() {
                    None => Ok(compiled.source),
                    Some(error) => Err(pattern_error_text(error)),
                }
            }
            MatcherSyntax::Regex => {
                let source = if self.role == PatternKind::Raw {
                    matchers::translate_esc(&self.source)
                } else {
                    self.source.clone()
                };
                regex::Regex::new(&source).map_err(
                    |e| crate::i18n::t!("editor-invalid-regex", "error" => e.to_string()),
                )?;
                Ok(source)
            }
        }
    }
}

/// Builds the editor's row list from a stored trigger. A fresh sidecar drives
/// the rows; an absent or stale one (the stored vectors were hand-edited)
/// degrades every row to Regex syntax showing the stored regexes verbatim.
pub fn trigger_rows(trigger: &triggers::TriggerDefinition) -> Vec<TriggerRow> {
    if let Some(sidecar) = &trigger.matchers {
        let fresh = matchers::trigger_patterns(sidecar).is_ok_and(|derived| {
            let stored = |v: &Option<Vec<String>>| v.clone().unwrap_or_default();
            derived.patterns == stored(&trigger.patterns)
                && derived.anti_patterns == stored(&trigger.anti_patterns)
                && derived.raw_patterns == stored(&trigger.raw_patterns)
        });
        if fresh {
            return sidecar
                .iter()
                .map(|m| TriggerRow {
                    role: PatternKind::from_role(m.role),
                    syntax: m.syntax,
                    source: m.source.clone(),
                    anchor_start: m.anchor_start,
                    anchor_end: m.anchor_end,
                })
                .collect();
        }
    }
    let mut rows = Vec::new();
    if let Some(patterns) = &trigger.patterns {
        rows.extend(patterns.iter().map(|p| TriggerRow {
            source: p.clone(),
            ..TriggerRow::new(PatternKind::Match)
        }));
    }
    if let Some(anti) = &trigger.anti_patterns {
        rows.extend(anti.iter().map(|p| TriggerRow {
            source: p.clone(),
            ..TriggerRow::new(PatternKind::Anti)
        }));
    }
    if let Some(raw) = &trigger.raw_patterns {
        rows.extend(raw.iter().map(|p| TriggerRow {
            source: p.clone(),
            ..TriggerRow::new(PatternKind::Raw)
        }));
    }
    // A trigger with no matchers opens at the teaching-cards state (README §4)
    // rather than with a blank row pre-created.
    rows
}

/// Rebuilds a trigger's three pattern vectors (and its sidecar) from the row
/// list — the save path. The sidecar is written only when it carries real
/// authoring intent: a Pattern-syntax row, or a Raw row whose stored form
/// differs from what was typed (`\e` translation); pure hand-written-regex
/// triggers stay sidecar-free, exactly like files from older clients.
///
/// # Errors
///
/// Returns the first failing row's `(index, display error)`.
pub fn rows_into_trigger(
    rows: &[TriggerRow],
    trigger: &mut triggers::TriggerDefinition,
) -> Result<(), (usize, String)> {
    let mut patterns = Vec::new();
    let mut anti_patterns = Vec::new();
    let mut raw_patterns = Vec::new();
    let mut wants_sidecar = false;

    for (i, row) in rows.iter().enumerate() {
        if row.source.trim().is_empty() {
            continue;
        }
        let compiled = row.compiled().map_err(|e| (i, e))?;
        wants_sidecar |= row.syntax == MatcherSyntax::Pattern || compiled != row.source;
        match row.role {
            PatternKind::Match => patterns.push(compiled),
            PatternKind::Anti => anti_patterns.push(compiled),
            PatternKind::Raw => raw_patterns.push(compiled),
        }
    }

    let some = |v: Vec<String>| if v.is_empty() { None } else { Some(v) };
    trigger.patterns = some(patterns);
    trigger.anti_patterns = some(anti_patterns);
    trigger.raw_patterns = some(raw_patterns);
    trigger.matchers = wants_sidecar.then(|| {
        rows.iter()
            .filter(|row| !row.source.trim().is_empty())
            .map(|row| TriggerMatcherSource {
                role: row.role.role(),
                syntax: row.syntax,
                source: row.source.clone(),
                anchor_start: row.anchor_start,
                anchor_end: row.anchor_end,
            })
            .collect()
    });
    Ok(())
}

/// The client-side package dependency graph, derived from the lockfile plus
/// each installed package's resolved `dependencies`. Specifiers are the keys.
#[derive(Debug, Clone, Default)]
pub struct PackageGraph {
    /// `specifier -> (range, dep specifiers)` — a package's declared requires.
    pub requires: HashMap<String, Vec<DepEdge>>,
    /// Direct-install intent: the user installed it at top level.
    pub direct: HashSet<String>,
    /// Owned (authored) packages — always directly enabled (their own source).
    pub owned: HashSet<String>,
    /// The user's direct enable intent for controllable packages.
    pub intent: HashMap<String, bool>,
    /// Resolved version per specifier (best-effort, from the last resolve).
    pub resolved: HashMap<String, String>,
}

/// One edge in the requires graph: the dependency specifier + its declared range.
#[derive(Debug, Clone)]
pub struct DepEdge {
    pub specifier: String,
    pub range: String,
}

impl PackageGraph {
    /// Packages whose `requires` include `id`.
    pub fn required_by(&self, id: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .requires
            .iter()
            .filter(|(_, edges)| edges.iter().any(|e| e.specifier == id))
            .map(|(parent, _)| parent.clone())
            .collect();
        out.sort();
        out
    }

    /// A dependency-only package: not directly installed, not owned, but required
    /// by something — its on/off follows its dependents.
    pub fn is_dep_only(&self, id: &str) -> bool {
        !self.direct.contains(id) && !self.owned.contains(id) && !self.required_by(id).is_empty()
    }

    /// Effective-enabled: the user turned it on (or owns it), or some
    /// effectively-enabled dependent needs it. Guards against cycles.
    pub fn effectively_enabled(&self, id: &str) -> bool {
        let mut visited = HashSet::new();
        self.eff(id, &mut visited)
    }

    fn eff(&self, id: &str, visited: &mut HashSet<String>) -> bool {
        if !visited.insert(id.to_string()) {
            return false;
        }
        if self.owned.contains(id) || self.intent.get(id).copied().unwrap_or(false) {
            return true;
        }
        self.required_by(id)
            .iter()
            .any(|parent| self.eff(parent, visited))
    }

    /// The switch is interactive only when nothing else forces it on:
    /// not dep-only, and no *enabled* package currently requires it.
    pub fn controllable(&self, id: &str) -> bool {
        if self.is_dep_only(id) {
            return false;
        }
        !self
            .required_by(id)
            .iter()
            .any(|parent| self.effectively_enabled(parent))
    }

    /// Enabled dependents currently forcing `id` on (for the "Required by …" note).
    pub fn enabled_dependents(&self, id: &str) -> Vec<String> {
        self.required_by(id)
            .into_iter()
            .filter(|parent| self.effectively_enabled(parent))
            .collect()
    }

    /// Whether a dependency row shown UNDER `parent` should read as live. Such a row
    /// exists because `parent` pulls `child` in, so it follows the parent's context:
    /// it greys once `parent` is no longer effectively enabled, rather than reporting
    /// `child`'s global state (which stays on for a separately-installed `child` that
    /// runs on its own — that belongs to `child`'s own row, not this edge). The
    /// operative term is the parent; the `child` term is a guard, since `parent`
    /// requiring `child` already implies [`effectively_enabled`](Self::effectively_enabled)
    /// of `child` whenever the parent is enabled — kept so the predicate is
    /// self-contained for any edge it's asked about.
    pub fn dep_edge_active(&self, parent: &str, child: &str) -> bool {
        self.effectively_enabled(parent) && self.effectively_enabled(child)
    }
}

impl AutomationsWindow {
    /// Loads aliases/triggers/hotkeys into the nested script tree.
    pub(super) fn load_scripts_message(&self) -> Message {
        let mut errors = Vec::new();

        let aliases = aliases::load_aliases(&self.server_name)
            .map_err(|e| errors.push(e.to_string()))
            .unwrap_or_default()
            .into_iter()
            .map(|(name, alias)| (name, Script::Alias(alias)));
        let hotkeys = hotkeys::load_hotkeys(&self.server_name)
            .map_err(|e| errors.push(e.to_string()))
            .unwrap_or_default()
            .into_iter()
            .map(|(name, hotkey)| (name, Script::Hotkey(hotkey)));
        let triggers = triggers::load_triggers(&self.server_name)
            .map_err(|e| errors.push(e.to_string()))
            .unwrap_or_default()
            .into_iter()
            .map(|(name, trigger)| (name, Script::Trigger(trigger)));

        let combined: Vec<(String, Script)> =
            aliases.into_iter().chain(hotkeys).chain(triggers).collect();

        let mut scripts = BTreeMap::new();
        for (name, script) in combined {
            match upsert_script_folder(&mut scripts, script.folder_name()) {
                Ok(folder) => {
                    folder.insert(name, script);
                }
                Err(e) => errors.push(e),
            }
        }
        Message::ScriptsLoaded(scripts, Arc::new(errors))
    }

    /// Adds the folder-tree's folders (incl. empty ones) into the script map so
    /// they render with no scripts inside. Idempotent.
    pub(super) fn merge_folders(&mut self) {
        for path in packages::collect_folder_paths(&self.packages) {
            let _ = upsert_script_folder(&mut self.scripts, Some(&path));
        }
    }

    pub(super) fn serialize_scripts(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut aliases_map = std::collections::HashMap::new();
        let mut hotkeys_map = std::collections::HashMap::new();
        let mut triggers_map = std::collections::HashMap::new();
        collect_scripts(
            &self.scripts,
            &mut aliases_map,
            &mut hotkeys_map,
            &mut triggers_map,
        );
        aliases::save_aliases(&self.server_name, &aliases_map).map_err(
            |e| crate::i18n::t!("automation-save-aliases-failed", "error" => e.to_string()),
        )?;
        hotkeys::save_hotkeys(&self.server_name, &hotkeys_map).map_err(
            |e| crate::i18n::t!("automation-save-hotkeys-failed", "error" => e.to_string()),
        )?;
        triggers::save_triggers(&self.server_name, &triggers_map).map_err(
            |e| crate::i18n::t!("automation-save-triggers-failed", "error" => e.to_string()),
        )?;
        Ok(())
    }

    pub(super) fn script_exists(&self, name: &str) -> bool {
        fn rec(scripts: &BTreeMap<String, Script>, name: &str) -> bool {
            for (script_name, script) in scripts {
                // Case-insensitive: these names become files on disk, and
                // Windows/macOS filesystems treat `Combat` and `combat` as one.
                if models::naming::names_conflict(script_name, name) {
                    return true;
                }
                if let Script::Folder(_, children) = script
                    && rec(children, name)
                {
                    return true;
                }
            }
            false
        }
        rec(&self.scripts, name)
    }

    pub(super) fn remove_script_by_name(&mut self, name: &str) {
        fn rec(scripts: &mut BTreeMap<String, Script>, name: &str) -> bool {
            if scripts.remove(name).is_some() {
                return true;
            }
            for script in scripts.values_mut() {
                if let Script::Folder(_, children) = script
                    && rec(children, name)
                {
                    return true;
                }
            }
            false
        }
        rec(&mut self.scripts, name);
    }

    /// Looks up a leaf script by its (folder, name) key.
    pub(super) fn find_script(&self, key: &ScriptKey) -> Option<Script> {
        fn rec(scripts: &BTreeMap<String, Script>, name: &str) -> Option<Script> {
            for (script_name, script) in scripts {
                if script_name == name && !matches!(script, Script::Folder(_, _)) {
                    return Some(script.clone());
                }
                if let Script::Folder(_, children) = script
                    && let Some(found) = rec(children, name)
                {
                    return Some(found);
                }
            }
            None
        }
        rec(&self.scripts, &key.script_name)
    }

    /// Every folder path in the tree (each `Script::Folder`, nested as a
    /// `/`-joined path), sorted with parents before children. This is the set of
    /// destinations the "move to folder" affordances offer — drawn from the live
    /// script tree (not `packages.json`) so it includes folders that exist only
    /// because a script's `package` field points at them.
    pub(super) fn all_folder_paths(&self) -> Vec<String> {
        fn rec(scripts: &BTreeMap<String, Script>, prefix: &str, out: &mut Vec<String>) {
            for (name, script) in scripts {
                if let Script::Folder(_, children) = script {
                    let path = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{prefix}/{name}")
                    };
                    out.push(path.clone());
                    rec(children, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        rec(&self.scripts, "", &mut out);
        out.sort();
        out
    }

    /// The effective status of a leaf script (its own enable + folder enable + a
    /// compile error). Folders/modules report Ok unless disabled.
    pub(super) fn script_status(&self, script: &Script) -> NodeStatus {
        let folder_enabled = script
            .folder_name()
            .is_none_or(|path| packages::is_package_effectively_enabled(path, &self.packages));
        if !script.own_enabled() || !folder_enabled {
            return NodeStatus::Disabled;
        }
        if script_has_error(script) {
            return NodeStatus::Error;
        }
        NodeStatus::Ok
    }
}

/// Whether a script carries an obvious compile error (a regex that won't build).
pub fn script_has_error(script: &Script) -> bool {
    match script {
        Script::Alias(a) if a.language != models::ScriptLang::Plaintext => false,
        Script::Alias(a) => regex::Regex::new(&a.pattern).is_err() && !a.pattern.is_empty(),
        Script::Trigger(t) => {
            let bad = |v: &Option<Vec<String>>| {
                v.as_ref().is_some_and(|patterns| {
                    patterns
                        .iter()
                        .any(|p| !p.is_empty() && regex::Regex::new(p).is_err())
                })
            };
            bad(&t.patterns) || bad(&t.anti_patterns) || bad(&t.raw_patterns)
        }
        Script::Hotkey(_) | Script::Folder(_, _) => false,
    }
}

/// Walks the tree collecting leaves of each type into flat maps for serialization.
fn collect_scripts(
    scripts: &BTreeMap<String, Script>,
    aliases: &mut std::collections::HashMap<String, models::aliases::AliasDefinition>,
    hotkeys: &mut std::collections::HashMap<String, models::hotkeys::HotkeyDefinition>,
    triggers: &mut std::collections::HashMap<String, models::triggers::TriggerDefinition>,
) {
    for (name, script) in scripts {
        match script {
            Script::Alias(a) => {
                aliases.insert(name.clone(), a.clone());
            }
            Script::Hotkey(h) => {
                hotkeys.insert(name.clone(), h.clone());
            }
            Script::Trigger(t) => {
                triggers.insert(name.clone(), t.clone());
            }
            Script::Folder(_, children) => collect_scripts(children, aliases, hotkeys, triggers),
        }
    }
}

/// Ensures the folder chain `folder_name` exists in `scripts`, returning the
/// innermost folder's child map.
pub fn upsert_script_folder<'a>(
    scripts: &'a mut BTreeMap<String, Script>,
    folder_name: Option<&str>,
) -> Result<&'a mut BTreeMap<String, Script>, String> {
    let mut current = scripts;
    if let Some(folder_name) = folder_name {
        for (i, folder) in folder_name.split('/').enumerate() {
            match current.get(folder) {
                Some(Script::Folder(_, _)) => {}
                Some(_) => {
                    return Err(crate::i18n::t!(
                        "automation-script-folder-invalid",
                        "path" => folder_name.split('/').take(i).collect::<Vec<_>>().join("/")
                    ));
                }
                None => {
                    current.insert(folder.to_string(), Script::Folder(false, BTreeMap::new()));
                }
            }
            current = match current.get_mut(folder) {
                Some(Script::Folder(_, children)) => children,
                _ => return Err(crate::i18n::t!("automation-script-folder-create-failed")),
            };
        }
    }
    Ok(current)
}

/// Parses a `smudgy://owner/name` specifier into `(owner, name)`.
pub fn parse_specifier(specifier: &str) -> Option<(String, String)> {
    let rest = specifier.strip_prefix("smudgy://")?;
    let (owner, name) = rest.rsplit_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}

/// A short display label for an installed-package specifier (the trailing name).
pub fn package_display_name(specifier: &str) -> &str {
    specifier.rsplit('/').next().unwrap_or(specifier)
}

/// The specifier a `LockedPackage` would carry for a given owner/name.
pub fn specifier_for(owner: &str, name: &str) -> String {
    format!("smudgy://{owner}/{name}")
}

/// Whether `owner/name` is present in the lockfile list.
pub fn is_installed(installed: &[LockedPackage], owner: &str, name: &str) -> bool {
    let specifier = specifier_for(owner, name);
    installed.iter().any(|p| p.specifier == specifier)
}

#[cfg(test)]
mod tests {
    use super::{DepEdge, PackageGraph};

    mod matcher_drafts {
        use super::super::*;

        fn alias(pattern: &str, matcher: Option<AliasMatcherSource>) -> aliases::AliasDefinition {
            aliases::AliasDefinition {
                pattern: pattern.to_string(),
                script: None,
                package: None,
                enabled: true,
                priority: 0,
                fallthrough: true,
                language: smudgy_core::models::ScriptLang::Plaintext,
                matcher,
            }
        }

        #[test]
        fn fresh_pattern_sidecar_drives_the_draft_and_round_trips() {
            let source = AliasMatcherSource::Pattern {
                source: "greet {person}".to_string(),
                anchor_start: true,
                anchor_end: true,
            };
            let stored = matchers::alias_pattern(&source, "greet").unwrap();
            let draft =
                AliasMatcherDraft::from_definition(&alias(&stored, Some(source.clone())), "greet");
            assert_eq!(draft.kind, AliasKind::Pattern);
            assert_eq!(draft.pattern_source, "greet {person}");
            assert!(!draft.degraded);
            // save(compile(sidecar)) == stored, and the sidecar re-emerges.
            assert_eq!(draft.to_pattern("greet").unwrap(), stored);
            assert_eq!(draft.to_matcher(), Some(source));
        }

        #[test]
        fn stale_sidecar_degrades_to_regex_showing_the_hand_edit() {
            let source = AliasMatcherSource::Pattern {
                source: "greet {person}".to_string(),
                anchor_start: true,
                anchor_end: true,
            };
            let draft = AliasMatcherDraft::from_definition(
                &alias(r"^greetz\s+(.*)$", Some(source)),
                "greet",
            );
            assert_eq!(draft.kind, AliasKind::Regex);
            assert!(draft.degraded);
            assert_eq!(draft.regex_source, r"^greetz\s+(.*)$");
            // Saving keeps the hand edit and drops the lying sidecar.
            assert_eq!(draft.to_pattern("greet").unwrap(), r"^greetz\s+(.*)$");
            assert_eq!(draft.to_matcher(), None);
        }

        #[test]
        fn the_alias_name_is_the_command_until_an_override_is_pinned() {
            let draft = AliasMatcherDraft {
                kind: AliasKind::Command,
                ..AliasMatcherDraft::default()
            };
            // Renaming the alias renames the command, prefilter and all.
            assert_eq!(draft.command_word("obe"), "obe");
            assert_eq!(draft.to_pattern("obe").unwrap(), r"^obe(?:\s|$)");
            assert_eq!(draft.to_pattern("bash").unwrap(), r"^bash(?:\s|$)");
            assert!(matches!(
                draft.to_matcher(),
                Some(AliasMatcherSource::Command { name: None, .. })
            ));
            assert!(!draft.shows_command_override("obe"));

            // A pinned override wins and keeps its row on screen.
            let pinned = AliasMatcherDraft {
                command_override: Some("*".to_string()),
                ..draft.clone()
            };
            assert_eq!(pinned.command_word("star-emote"), "*");
            assert_eq!(pinned.to_pattern("star-emote").unwrap(), r"^\*(?:\s|$)");
            assert!(pinned.shows_command_override("star-emote"));

            // Revealed but blank is no override: it still inherits, and it
            // still saves as an absent field.
            let blank = AliasMatcherDraft {
                command_override: Some("  ".to_string()),
                ..draft.clone()
            };
            assert_eq!(blank.command_word("obe"), "obe");
            assert!(matches!(
                blank.to_matcher(),
                Some(AliasMatcherSource::Command { name: None, .. })
            ));
            assert!(blank.shows_command_override("obe"));
        }

        #[test]
        fn a_name_that_cannot_be_a_command_word_blocks_the_save() {
            let draft = AliasMatcherDraft {
                kind: AliasKind::Command,
                ..AliasMatcherDraft::default()
            };
            // Names may contain spaces; command words are one token, so the
            // override row reveals itself and the save refuses until it is
            // filled in.
            assert!(draft.to_pattern("guild tell").is_err());
            assert!(draft.shows_command_override("guild tell"));
            assert!(draft.to_pattern("   ").is_err());

            let rescued = AliasMatcherDraft {
                command_override: Some("gt".to_string()),
                ..draft
            };
            assert_eq!(rescued.to_pattern("guild tell").unwrap(), r"^gt(?:\s|$)");
        }

        #[test]
        fn simple_mode_normalizes_args_to_optional_then_rest() {
            let mut draft = AliasMatcherDraft {
                kind: AliasKind::Command,
                command_override: None,
                args: vec![
                    ArgSpec {
                        name: "a".to_string(),
                        kind: ArgKind::Required,
                    },
                    ArgSpec {
                        name: "b".to_string(),
                        kind: ArgKind::Required,
                    },
                ],
                ..AliasMatcherDraft::default()
            };
            draft.normalize_args();
            assert_eq!(draft.args[0].kind, ArgKind::Optional);
            assert_eq!(draft.args[1].kind, ArgKind::Rest);

            // Advanced mode: only the rest-only-last invariant applies.
            draft.cmd_mode = CmdMode::Advanced;
            draft.args[0].kind = ArgKind::Rest;
            draft.normalize_args();
            assert_eq!(draft.args[0].kind, ArgKind::Optional);
            assert_eq!(draft.args[1].kind, ArgKind::Rest);
        }

        #[test]
        fn trigger_rows_round_trip_through_the_sidecar() {
            let rows = vec![
                TriggerRow {
                    role: PatternKind::Match,
                    syntax: MatcherSyntax::Pattern,
                    source: "You are {state}.".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                },
                TriggerRow {
                    role: PatternKind::Raw,
                    syntax: MatcherSyntax::Regex,
                    source: r"\e\[31m".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                },
            ];
            let mut trigger = triggers::TriggerDefinition::default();
            rows_into_trigger(&rows, &mut trigger).unwrap();
            assert_eq!(
                trigger.patterns.as_deref(),
                Some(&[r"^You\s+are\s+(?<state>.*?)\.$".to_string()][..])
            );
            // The raw row stores the translated form; the sidecar keeps `\e`.
            assert_eq!(
                trigger.raw_patterns.as_deref(),
                Some(&[r"\x1b\[31m".to_string()][..])
            );
            assert!(trigger.matchers.is_some());
            assert_eq!(trigger_rows(&trigger), rows);
        }

        #[test]
        fn pure_regex_rows_stay_sidecar_free() {
            let rows = vec![TriggerRow {
                source: "^ready$".to_string(),
                ..TriggerRow::new(PatternKind::Match)
            }];
            let mut trigger = triggers::TriggerDefinition::default();
            rows_into_trigger(&rows, &mut trigger).unwrap();
            assert!(
                trigger.matchers.is_none(),
                "no authoring intent, no sidecar"
            );
            assert_eq!(
                trigger.patterns.as_deref(),
                Some(&["^ready$".to_string()][..])
            );
        }

        #[test]
        fn hand_edited_vectors_degrade_sidecar_rows_to_regex() {
            let rows = vec![TriggerRow {
                role: PatternKind::Match,
                syntax: MatcherSyntax::Pattern,
                source: "You are {state}.".to_string(),
                anchor_start: true,
                anchor_end: true,
            }];
            let mut trigger = triggers::TriggerDefinition::default();
            rows_into_trigger(&rows, &mut trigger).unwrap();
            // A hand edit to the stored vector makes the sidecar stale.
            trigger.patterns = Some(vec!["^edited$".to_string()]);
            let degraded = trigger_rows(&trigger);
            assert_eq!(degraded.len(), 1);
            assert_eq!(degraded[0].syntax, MatcherSyntax::Regex);
            assert_eq!(degraded[0].source, "^edited$");
        }

        #[test]
        fn invalid_rows_report_their_index() {
            let rows = vec![
                TriggerRow {
                    source: "fine".to_string(),
                    ..TriggerRow::new(PatternKind::Match)
                },
                TriggerRow {
                    source: "[unclosed".to_string(),
                    ..TriggerRow::new(PatternKind::Match)
                },
            ];
            let mut trigger = triggers::TriggerDefinition::default();
            let (index, _) = rows_into_trigger(&rows, &mut trigger).unwrap_err();
            assert_eq!(index, 1);
        }
    }

    /// A directly-installed package: present in the lockfile (so in `direct`) with its own
    /// enable intent — exactly what `rebuild_graph` seeds for each installed entry.
    fn install(graph: &mut PackageGraph, spec: &str, enabled: bool) {
        graph.direct.insert(spec.to_string());
        graph.intent.insert(spec.to_string(), enabled);
    }

    /// Record that `parent` pulls `child` in (a `requires`-graph edge).
    fn imports(graph: &mut PackageGraph, parent: &str, child: &str) {
        graph
            .requires
            .entry(parent.to_string())
            .or_default()
            .push(DepEdge {
                specifier: child.to_string(),
                range: String::new(),
            });
    }

    #[test]
    fn dep_edge_row_greys_when_parent_disabled_but_dep_keeps_its_own_status() {
        // P imports D, and D is also separately installed (its own enabled lockfile entry).
        let mut graph = PackageGraph::default();
        install(&mut graph, "p", true);
        install(&mut graph, "d", true);
        imports(&mut graph, "p", "d");

        assert!(
            graph.dep_edge_active("p", "d"),
            "both on: D's row under P is live"
        );

        // Disable P. D keeps its own enabled entry, so it still runs on its own...
        graph.intent.insert("p".to_string(), false);
        assert!(
            graph.effectively_enabled("d"),
            "D still runs via its own install — its own row stays green",
        );
        // ...but its row UNDER P greys: the import via P is no longer active. This is the bug fix.
        assert!(!graph.dep_edge_active("p", "d"));
    }

    #[test]
    fn dep_edge_row_stays_live_via_another_enabled_requirer() {
        // P and Q both import D (D separately installed too).
        let mut graph = PackageGraph::default();
        install(&mut graph, "p", true);
        install(&mut graph, "q", true);
        install(&mut graph, "d", true);
        imports(&mut graph, "p", "d");
        imports(&mut graph, "q", "d");

        // Disable only P: D's row under P greys, but under still-enabled Q it stays live.
        graph.intent.insert("p".to_string(), false);
        assert!(
            !graph.dep_edge_active("p", "d"),
            "the import via the disabled P is dead"
        );
        assert!(graph.dep_edge_active("q", "d"), "Q still pulls D in");
        assert!(graph.effectively_enabled("d"));
    }

    #[test]
    fn dep_edge_row_follows_parent_for_a_pure_import_dep() {
        // P imports L, which has NO lockfile entry of its own (a pure transitive import).
        let mut graph = PackageGraph::default();
        install(&mut graph, "p", true);
        imports(&mut graph, "p", "l");

        assert!(graph.is_dep_only("l"));
        assert!(
            graph.dep_edge_active("p", "l"),
            "L is live while P pulls it in"
        );

        // Disable P: nothing else needs L, so both its global state and its row go inactive.
        graph.intent.insert("p".to_string(), false);
        assert!(!graph.effectively_enabled("l"));
        assert!(!graph.dep_edge_active("p", "l"));
    }
}
