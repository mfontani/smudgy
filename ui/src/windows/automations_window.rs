//! The **Automations** window — a separate desktop window where a player
//! manages everything that reacts to or augments their MUD session: aliases,
//! triggers, hotkeys, folders, modules, and packages.
//!
//! Structure: a fixed left **sidebar** (New menu + search + filter chips +
//! status-dotted tree + footer) and a flexible **main** column (a top action
//! bar over one content pane at a time). A Ctrl/⌘+P command palette overlays both.
//!
//! Uses the on-disk model (`aliases.json` / `triggers.json` / `hotkeys.json` /
//! `packages.json`, `modules/`, `packages/`, `smudgy.lock.json`) and the cloud clients.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::event::{Event as IcedEvent, Status};
use iced::keyboard::{self, key::Named};
use iced::widget::{markdown, operation, text_editor};
use iced::{Subscription, Task, window};
use smudgy_cloud::cloud_api::FriendView;
use smudgy_cloud::package_api::{
    CommentView, PackageDetail, PackageGrantView, PackageSearchResult, ResolvedPackageWire,
    VersionListItem,
};
use smudgy_cloud::{CloudError, Uuid};
use smudgy_core::models::local_packages::{LocalPackage, PublishSummary};
use smudgy_core::models::modules::ModuleFile;
use smudgy_core::models::packages::{self as core_packages, PackageTree};
use smudgy_core::models::server;
use smudgy_core::models::shared_packages::{LockedPackage, PackagePermissions, UpdateMode};
use smudgy_core::models::{ScriptLang, aliases, hotkeys};
use smudgy_core::session::SessionId;
use smudgy_core::session::runtime::catalogue::{CatalogueEvent, CatalogueSnapshot};
use smudgy_core::session::runtime::{AutomationEvent, AutomationKind};

use crate::cloud_account::CloudHandles;
use crate::keymap::MaybePhysicalKey;
use crate::theme::Element as ThemedElement;
use crate::update::Update;

pub(crate) mod common;
// Host-owned writable-code controller and `iced-code-editor` adapter. The current
// read-only previews intentionally keep their existing text widgets.
#[allow(dead_code)]
mod code_editor;
mod dashboard;
mod editors;
mod highlight;
mod keyboard_control;
mod manifest;
pub(crate) mod model;
mod packages;
mod palette;
mod param_values;
mod sidebar;
mod store_inspector;
mod topbar;

use manifest::{ManifestDraft, ManifestEdit, ManifestTab};
use model::{LiveAutomations, PackageGraph, PatternKind, Script, ScriptKey};
use packages::{
    ConsentPrompt, DetailSeq, FilePreview, ForkActivation, InstallResolution, InstallSeq,
    InstalledFileTab, ParamConfig, ParamPrompt, PublishOutput, StaleInstallCheck, UpdateDelta,
};

/// Returns the traversal direction for an unconsumed, unmodified Tab press.
/// `true` means backwards (Shift+Tab). Shortcut-modified Tabs remain
/// available to the window manager/application.
fn tab_traversal(modifiers: keyboard::Modifiers, status: Status) -> Option<bool> {
    (status == Status::Ignored && !modifiers.control() && !modifiers.alt() && !modifiers.logo())
        .then_some(modifiers.shift())
}

fn code_completion_shortcut(
    key: &keyboard::Key,
    modifiers: keyboard::Modifiers,
    status: Status,
) -> bool {
    status == Status::Ignored
        && modifiers.command()
        && matches!(key.as_ref(), keyboard::Key::Character(" "))
}

fn matcher_truecolor_range(
    color: smudgy_core::models::matchers::MatcherColor,
) -> Option<smudgy_core::models::matchers::MatcherHsvRange> {
    use smudgy_core::models::matchers::{MatcherColor, MatcherHsvRange};
    let MatcherColor::Truecolor { r, g, b, range } = color else {
        return None;
    };
    let point = smudgy_core::models::matchers::MatcherHsv::from_rgb(r, g, b);
    let range = range
        .unwrap_or_else(|| MatcherHsvRange::from_to(point, point))
        .rgb_canonicalized();
    let (from, to) = range.directed_endpoints();
    Some(MatcherHsvRange::from_to(from, to))
}

fn matcher_hsv_to_picker(
    hsv: smudgy_core::models::matchers::MatcherHsv,
) -> crate::components::color_picker::Hsv {
    crate::components::color_picker::Hsv {
        hue: f32::from(hsv.hue % 360),
        saturation: f32::from(hsv.saturation) / 255.0,
        value: f32::from(hsv.value) / 255.0,
    }
}

fn picker_hsv_to_matcher(
    hsv: crate::components::color_picker::Hsv,
) -> smudgy_core::models::matchers::MatcherHsv {
    let hsv = hsv.normalized();
    let quantized = smudgy_core::models::matchers::MatcherHsv {
        hue: (hsv.hue.round() as u16) % 360,
        saturation: (hsv.saturation * 255.0).round() as u8,
        value: (hsv.value * 255.0).round() as u8,
    };
    // Store the HSV value that the displayed 8-bit RGB swatch represents.
    // Without this canonical value, HSV-to-RGB-to-HSV quantization can make a
    // single-color or narrow range reject its selected color. Keep the
    // specified hue as a range boundary for an achromatic endpoint. Matching
    // does not compare hue when the input color is achromatic.
    quantized.rgb_canonicalized()
}

fn matcher_truecolor_from_range(
    range: smudgy_core::models::matchers::MatcherHsvRange,
) -> smudgy_core::models::matchers::MatcherColor {
    let from = range.first.rgb_canonicalized();
    let to = range.second.rgb_canonicalized();
    let range = smudgy_core::models::matchers::MatcherHsvRange::from_to(from, to);
    let (r, g, b) = range.first.to_rgb();
    smudgy_core::models::matchers::MatcherColor::Truecolor {
        r,
        g,
        b,
        range: Some(range),
    }
}

fn matcher_hsv_hex(hsv: smudgy_core::models::matchers::MatcherHsv) -> String {
    let (r, g, b) = hsv.to_rgb();
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Convenience alias for this window's themed elements.
pub(crate) type Elem<'a> = ThemedElement<'a, Message>;

/// Events bubbled up to the daemon when persisted runtime inputs change.
#[derive(Debug, Clone)]
pub enum Event {
    /// Aliases, triggers, hotkeys, or their legacy folder enablement changed. Live sessions
    /// reconcile only the changed user-owned registrations.
    UserAutomationsChanged { server_name: String },
    /// Modules or script packages changed and still require the existing full engine reload.
    ScriptsChanged { server_name: String },
}

/// Create vs. edit, shared by the script and folder editors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
    Create,
    Edit,
}

/// The single-select filter chips above the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    All,
    Aliases,
    Triggers,
    Hotkeys,
    Folders,
    Modules,
    Packages,
}

/// The Discover scope radios — a host-aware view over the wire `(host, SearchCategory)` pair
/// (translated in [`AutomationsWindow::discover_search`]). The host is this profile's MUD host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiscoverScope {
    /// Aligned to this profile's MUD host *plus* universal packages — the useful default
    /// (`host` + `category=both`). With no profile host, this is equivalent to [`Self::All`].
    #[default]
    Relevant,
    /// Only packages aligned to this profile's MUD host (`host` + `category=mud`).
    HostOnly,
    /// Only host-agnostic (universal) packages (`category=universal`).
    Universal,
    /// Every public package, regardless of MUD alignment (no host + `category=both`).
    All,
}

/// The body of a script editor — the per-kind editable fields. Writable JS/TS
/// bodies live in [`AutomationsWindow::code_editor`].
#[derive(Debug, Clone)]
pub enum EditNode {
    Alias(aliases::AliasDefinition),
    Hotkey(hotkeys::HotkeyDefinition),
    Trigger {
        enabled: bool,
        language: ScriptLang,
        prompt: bool,
        priority: i32,
        fallthrough: bool,
        package: Option<String>,
        /// The unified, ordered matcher row list (role + syntax per row).
        rows: Vec<model::TriggerRow>,
    },
}

/// State for the open script editor pane.
#[derive(Debug, Clone)]
pub struct EditorState {
    pub mode: EditorMode,
    pub original_name: Option<String>,
    pub name: String,
    pub node: EditNode,
    pub error: Option<String>,
}

/// State for the folder editor pane.
#[derive(Debug, Clone)]
pub struct FolderState {
    pub mode: EditorMode,
    pub original_path: Option<String>,
    pub path: String,
    pub enabled: bool,
    pub error: Option<String>,
}

/// View vs. create, for the module pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleMode {
    View,
    Create,
}

/// State for the module pane (a local, non-shareable helper file).
#[derive(Debug, Clone)]
pub struct ModuleState {
    pub mode: ModuleMode,
    pub subpath: String,
    pub path: Option<PathBuf>,
    pub name: String,
    pub error: Option<String>,
}

/// Exactly one content pane shows at a time.
#[derive(Default, Debug, Clone)]
pub enum Pane {
    #[default]
    Dashboard,
    Error(Arc<Vec<String>>),
    Editor(EditorState),
    Folder(FolderState),
    Module(ModuleState),
    /// The author view of a package you own (source + dependents + versions +
    /// sharing). Data lives in `self.local_package` / share-state fields.
    OwnedPackage,
    /// The create-a-package form.
    NewPackage {
        name: String,
        error: Option<String>,
    },
    /// The consumer view of an installed package (deps + README + actions).
    InstalledPackage,
    /// The read-only detail of a script-created automation (pattern + body). Data is read live
    /// from `self.live` keyed by these fields, so the pane just carries the lookup key.
    CreatorAutomation {
        creator_id: String,
        kind: AutomationKind,
        name: String,
    },
    Discover,
    Shared,
    /// The live session-store inspector (`docs/interop.md` §10): the store tree
    /// per producer plus the interop catalogue (declared/observed handles with recent
    /// samples and inferred shapes). Data streams in via [`Message::CatalogueEvent`] while
    /// this pane is open.
    StoreInspector,
}

/// Which tree node is currently selected (drives highlighting + breadcrumb).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    None,
    Script(ScriptKey),
    Folder(String),
    Module(String),
    OwnedPackage(String),
    InstalledPackage(String),
    /// A dependency *reference* row nested under `parent` (an installed/local package). Distinct
    /// from [`Selection::InstalledPackage`] so that selecting the reference highlights only the
    /// clicked row — not the same package's own top-level row, when it has one.
    Dependency {
        parent: String,
        spec: String,
    },
    /// A script-created (package/module) automation leaf, keyed by its creator tree node
    /// (`module:<subpath>` / `package:<spec>`), kind, and name. Drives the read-only detail pane.
    CreatorAutomation {
        creator_id: String,
        kind: AutomationKind,
        name: String,
    },
    Discover,
    Shared,
    Dashboard,
    StoreInspector,
}

#[derive(Debug, Clone)]
pub enum Message {
    // ---- loading -----------------------------------------------------------
    ScriptsLoaded(BTreeMap<String, Script>, Arc<Vec<String>>),
    LoadFolders,
    LoadModules,
    LoadLocalPackages,
    LoadInstalledPackages,

    // ---- navigation / selection -------------------------------------------
    ShowDashboard,
    SelectScript(ScriptKey),
    SelectFolder(String),
    SelectModule(String),
    SelectOwnedPackage(String),
    SelectInstalledPackage(String),
    /// Open an installed package via a nested dependency-reference row (keeps the clicked row,
    /// not the package's top-level row, as the highlighted selection).
    SelectDependency {
        parent: String,
        spec: String,
    },
    /// Open the read-only detail pane for a script-created automation.
    SelectCreatorAutomation {
        creator_id: String,
        kind: AutomationKind,
        name: String,
    },
    ToggleFolderExpanded(String),

    // ---- sidebar controls --------------------------------------------------
    ToggleNewMenu,
    SearchChanged(String),
    ClearSearch,
    SelectChip(Chip),

    // ---- create ------------------------------------------------------------
    NewAlias,
    NewTrigger,
    NewHotkey,
    NewFolder,
    NewModule,
    NewPackage,

    // ---- editor fields -----------------------------------------------------
    SetName(String),
    /// An edit in the alias Regex field's one-line editor.
    AliasRegexAction(text_editor::Action),
    // alias matcher draft
    SetAliasKind(model::AliasKind),
    SetArgName(usize, String),
    SetArgKind(usize, smudgy_core::models::matchers::ArgKind),
    AddArg,
    RemoveArg(usize),
    SetCmdMode(smudgy_core::models::matchers::CmdMode),
    SetParseMode(smudgy_core::models::matchers::ParseMode),
    /// Open/close the Parsing picker's floating list.
    OpenParsingPicker,
    CloseParsingPicker,
    /// Move the Parsing picker's keyboard cursor by a delta.
    MoveParsingCursor(i32),
    /// An edit in the alias Simple-pattern field's one-line editor.
    AliasPatternAction(text_editor::Action),
    ToggleAnchorStart,
    ToggleAnchorEnd,
    TogglePrompt,
    RevealOrder,
    HideOrder,
    /// Insert a capture reference at the caret in the action body.
    InsertReference(String),
    /// Move the open script to a folder (`None` = top level). Also dispatched by
    /// the palette's "Move to…" group for the selected script.
    SetScriptFolder(Option<String>),
    SetBehavior(ScriptLang),
    AdjustPriority(i32),
    ToggleFallthrough,
    /// Flip the open alias's "sent text may match itself" opt-in.
    ToggleAllowSelfMatch,
    /// An event emitted by the active writable JS/TS editor.
    CodeEditorAction(code_editor::BoundEditorMessage),
    /// Applies a language-service completion from the visible candidate list.
    ApplyCodeCompletion(code_editor::CompletionSelection),
    /// Requests completions at the active code-editor caret (Ctrl/Command+Space or button).
    TriggerCodeCompletion,
    /// Closes host-owned completion and hover overlays without changing source.
    DismissCodeOverlays,
    /// Opens the first current-project target from an accepted definition response.
    NavigateCodeDefinition(code_editor::DefinitionNavigation),
    /// An edit in a plaintext hotkey body (JS/TS hotkeys use `CodeEditorAction`).
    HotkeyTextAction(text_editor::Action),
    /// An edit in the send-text action draft.
    SendTextAction(text_editor::Action),
    /// Expand/collapse the Try-it accordion (collapsed by default).
    ToggleTryIt,
    SetTestInput(String),
    ToggleEnabled,
    MarkHotkeyState(Vec<MaybePhysicalKey>),
    // trigger patterns
    AddPattern,
    /// Add an exception row (Pattern syntax by default).
    AddExceptionRow,
    /// Add a raw row (always Regex syntax).
    AddRawRow,
    /// A click on one of the trigger pane's matcher cards: creates the first
    /// matcher row at the zero-matcher state, or re-shapes the single existing
    /// matcher at the selector state (README §4).
    SetTriggerCard(model::TriggerCard),
    RemovePattern(usize),
    /// Move a row up/down within its role group (the phase order is fixed).
    MoveRowUp(usize),
    MoveRowDown(usize),
    /// An edit in a trigger row's one-line source editor.
    RowSourceAction(usize, text_editor::Action),
    SetRowSyntax(usize, smudgy_core::models::matchers::MatcherSyntax),
    ToggleRowAnchorStart(usize),
    ToggleRowAnchorEnd(usize),
    /// Adds or removes the color filter for a normal or exception matcher row.
    ToggleRowColor(usize, bool),
    SelectRowColorChannel(usize, smudgy_core::models::matchers::MatcherColorChannel),
    SelectRowColorKind(usize, model::MatcherColorKind),
    SetRowAnsiColor(usize, u8),
    SetRowXtermColor(usize, u8),
    SetRowColorRange(
        usize,
        model::ColorRangeEndpoint,
        crate::components::color_picker::Message,
    ),
    SetRowColorRangeHex(usize, model::ColorRangeEndpoint, String),
    SetRowExactTruecolorHex(usize, String),
    SetRowExactTruecolorRgb(usize, model::TruecolorComponent, String),
    ToggleRowColorAttribute(
        usize,
        smudgy_core::models::matchers::MatcherTextAttribute,
        bool,
    ),

    // ---- save bar ----------------------------------------------------------
    Save,
    Discard,
    Delete,
    ConfirmDiscardNav,
    CancelDiscardNav,

    // ---- folder ------------------------------------------------------------
    SetFolderPath(String),
    SaveFolder,
    RequestDeleteFolder,
    CancelDeleteFolder,
    ConfirmDeleteFolder(bool),

    // ---- module ------------------------------------------------------------
    SaveModule,
    SetNewModuleName(String),
    CreateModule,

    // ---- owned (local) package --------------------------------------------
    SelectOwnedFile(String),
    SaveOwnedFile,
    /// A field-level edit to the open package's manifest draft (the rich manifest editor for
    /// the package's `smudgy.package.json`).
    EditManifest(ManifestEdit),
    SelectManifestTab(ManifestTab),
    ManifestBeginEdit,
    SaveManifest,
    RevertManifest,
    PublishOwned,
    PublishFinished {
        name: String,
        result: Result<PublishSummary, String>,
    },
    RequestDeleteOwned,
    CancelDeleteOwned,
    DeleteOwned,
    SetNewPackageName(String),
    CreatePackage,
    // owned sharing / versions
    SetVisibility(bool),
    VisibilityUpdated(Result<bool, CloudError>),
    YankVersion {
        version: String,
        yanked: bool,
    },
    DeleteVersion(String),
    VersionsUpdated(Result<Vec<VersionListItem>, CloudError>),
    ShareWithFriend(Uuid),
    GrantsUpdated(Result<Vec<PackageGrantView>, CloudError>),
    #[allow(clippy::type_complexity)]
    OwnedShareLoaded {
        name: String,
        result: Result<
            (
                Uuid,
                bool,
                Vec<FriendView>,
                Vec<PackageGrantView>,
                Vec<VersionListItem>,
            ),
            CloudError,
        >,
    },

    // ---- installed package -------------------------------------------------
    /// The [`DetailSeq`] is the manage-pane detail generation captured when the load started; a
    /// stale result (the open package changed, navigation, uninstall, or a re-resolve) is discarded.
    InstalledDetailLoaded(
        DetailSeq,
        Box<Result<packages::InstalledDetail, CloudError>>,
    ),
    InstalledResolvedForGraph(
        String,
        Result<(ResolvedPackageWire, PackagePermissions), CloudError>,
    ),
    SetInstalledUpdateMode(UpdateMode),
    TogglePackageEnabled(String),
    /// Make `target_spec` the active member of a same-name group (enable it, disabling siblings).
    SetActiveMember {
        target_spec: String,
        siblings: Vec<String>,
    },
    /// Enable/disable a lone (non-colliding) local package from the tree.
    ToggleLocalEnabled(String),
    SelectInstalledFile(String),
    /// Switch the installed-package "README & source" area between its README and Source tabs.
    SelectInstalledFileTab(InstalledFileTab),
    /// A source-browser module body finished fetching for the open installed package, keyed by its
    /// `content_hash`. Content-addressed, so a late result just fills the cache and is matched to
    /// the selected file by hash — no staleness token needed.
    InstalledSourceLoaded {
        hash: String,
        result: Result<FilePreview, CloudError>,
    },
    RequestUninstall,
    /// The apt-style removal plan finished for the requested uninstall: `breaks` are the installed
    /// packages that `require` the open one (removed with it, forced); `orphans` are the
    /// auto-installed required roots nothing else would need once it's gone (offered).
    UninstallPlanComputed {
        breaks: Vec<String>,
        orphans: Vec<String>,
    },
    /// "Keep them": keep the offered orphans (clears only the orphan set; forced breaks still go).
    UninstallKeepOrphans,
    CancelUninstall,
    ConfirmUninstall,
    ForkPackage,
    ForkFinished(Result<(String, ForkActivation), String>),
    /// An async cloud check of account-owned installs finished (`delete_owned`'s post-delete
    /// check, or the installed-list sweep): stale entries were pruned, a parked entry was
    /// restored, or nothing changed.
    StaleAccountInstallsChecked(StaleInstallCheck),
    RevealPackageFolder,
    StartRenameOwned,
    RenameOwnedChanged(String),
    CommitRenameOwned,
    CancelRenameOwned,
    // trust toggle
    RequestTrust,
    CancelTrust,
    SetTrusted(bool),
    // owned (local) package: jump into the manifest's Capabilities tab; develop-unsandboxed toggle
    EditOwnedCapabilities,
    SetLocalUnsandboxed(bool),
    // update re-prompt
    GrantUpdate,
    DismissUpdate,
    // rating (a cloud package the user has installed): set the caller's 1–5 star rating, and the
    // fresh `PackageDetail` (rating average/count) the server returns for it.
    RateInstalledPackage(i16),
    InstalledRatingUpdated(Result<PackageDetail, CloudError>),

    // ---- discover ----------------------------------------------------------
    OpenDiscover,
    /// Loads the dashboard "Discover" teaser (a default-scope empty-query search).
    LoadFeaturedDiscover,
    FeaturedDiscoverLoaded(Result<Vec<PackageSearchResult>, CloudError>),
    DiscoverQueryChanged(String),
    DiscoverSearch,
    DiscoverScopeChanged(DiscoverScope),
    DiscoverResultsLoaded(Result<Vec<PackageSearchResult>, CloudError>),
    DiscoverSelect {
        package_id: Uuid,
        owner: String,
    },
    /// Install a search result directly (the result-card "Install" / dashboard teaser): routes to
    /// the Discover pane (so the consent window shows) and begins the install for `owner/name`.
    DiscoverInstallResult {
        owner: String,
        name: String,
    },
    DiscoverDetailLoaded(Result<PackageDetail, CloudError>),
    DiscoverCommentsLoaded(Result<Vec<CommentView>, CloudError>),
    DiscoverBack,
    RatePackage(i16),
    RatingUpdated(Result<PackageDetail, CloudError>),
    CommentInputChanged(String),
    AddComment,
    CommentAdded(Result<CommentView, CloudError>),
    OpenReadmeLink(markdown::Uri),
    DiscoverInstall,
    /// The [`InstallSeq`] is the install generation captured at `begin_install`; a stale result
    /// (the user navigated away / clicked Back / started another install) is discarded.
    InstallResolved(InstallSeq, Result<InstallResolution, CloudError>),
    // install-time consent confirmation; `enable` = "Install & enable" vs "Install, don't
    // enable" (both record the same consent — they differ only in turning the package on now).
    ConsentGrant {
        enable: bool,
    },
    ConsentCancel,
    // One edit to a parameter's value, routed by `ParamTarget` to the install-time prompt or the
    // in-pane config editor. The `String` is the parameter key; `ParamValueEdit` is the addressed
    // change (a scalar edit, or a list/table row op). Shared by both value-entry surfaces.
    ParamValueEdit(
        param_values::ParamTarget,
        String,
        param_values::ParamValueEdit,
    ),
    ParamPromptSubmit,
    ParamPromptCancel,
    // in-pane param-value editor (installed & owned package panes): save all, or clear a stored
    // secret. Distinct from the install-time `ParamPrompt*` gate above.
    ParamConfigSave,
    ParamConfigClearSecret(String),

    // ---- private & shared --------------------------------------------------
    OpenShared,
    SharedLoaded(Result<Vec<PackageDetail>, CloudError>),
    /// The caller's own cloud packages (`GET /packages/mine`), shown alongside the
    /// shared-with-me list in the "Private & Shared" pane — including private ones with
    /// no local copy on this machine, which appear in no other surface.
    MyCloudLoaded(Result<Vec<PackageDetail>, CloudError>),
    InstallShared {
        owner: String,
        name: String,
    },

    // ---- top action bar ----------------------------------------------------
    Reload,
    Inspect,

    // ---- command palette ---------------------------------------------------
    OpenPalette,
    ClosePalette,
    PaletteInput(String),
    PaletteMove(i32),
    PaletteRun,
    PaletteRunItem(usize),

    // ---- keyboard focus traversal -----------------------------------------
    /// Focus one feature-local composite color control after a pointer press.
    FocusColorControl(iced::widget::Id),
    FocusNext(window::Id),
    FocusPrevious(window::Id),

    // ---- toast -------------------------------------------------------------
    DismissToast(u64),

    /// Drain ready embedded language-service events without blocking the UI.
    PollLanguageService,

    // ---- live (script-created) automations --------------------------------
    AutomationEvent(AutomationEvent),
    ToggleCreator(String),
    ToggleCreatorShowAll(String),

    // ---- session-store inspector -------------------------------------------
    OpenStoreInspector,
    CatalogueEvent(CatalogueEvent),
    /// Flip one store-tree node between expanded and collapsed (keyed by producer + path).
    ToggleStoreNode(String),
}

/// The Automations window. One per (server, session) the user opens it for.
pub struct AutomationsWindow {
    window_id: window::Id,
    pub(super) server_name: String,
    pub(super) cloud: CloudHandles,
    pub(super) session_id: SessionId,
    pub(super) mud_host: Option<String>,
    /// Whether advanced scripting features are unlocked (settings `advanced_scripting_features`):
    /// the "Remove sandbox" package action and the script inspector. Read at construction and
    /// refreshed on Reload — toggling it in Settings takes effect on the next reload/reopen.
    pub(super) advanced_features: bool,

    // ---- script tree -------------------------------------------------------
    pub(super) scripts: BTreeMap<String, Script>,
    pub(super) packages: PackageTree,
    pub(super) modules: Vec<ModuleFile>,
    pub(super) local_packages: Vec<String>,
    pub(super) installed_packages: Vec<LockedPackage>,

    // ---- live (script-created) automations --------------------------------
    /// Streamed from this session's automation broadcast; rendered nested under each
    /// creating module/package node in the tree.
    pub(super) live: LiveAutomations,
    /// Creators whose nested automations are expanded (collapsed by default — a bulk package
    /// can create tens of thousands).
    pub(super) expanded_creators: HashSet<String>,
    /// Creators showing all their automations rather than the first `CREATOR_SHOW_LIMIT`.
    pub(super) show_all_creators: HashSet<String>,

    // ---- session-store inspector -------------------------------------------
    /// The latest catalogue snapshot, streamed from this session's catalogue broadcast while
    /// the store pane is open (the subscription exists only then, so a closed pane costs the
    /// runtime nothing). `None` before the first snapshot.
    pub(super) catalogue: Option<Arc<CatalogueSnapshot>>,
    /// Store-tree nodes whose expansion the user flipped (keyed producer + NUL + path). The
    /// default is expanded near the root and collapsed deeper; membership here inverts it.
    pub(super) store_toggled: HashSet<String>,

    pub(super) selection: Selection,
    pub(super) collapsed_folders: HashSet<String>,
    pub(super) pane: Pane,

    // ---- sidebar -----------------------------------------------------------
    pub(super) search: String,
    pub(super) chip: Chip,
    pub(super) new_menu_open: bool,

    // ---- shared editor buffers --------------------------------------------
    /// The active writable JS/TS/module/package editor. This field precedes
    /// `language_service`, so its Drop queues CloseDocument before host shutdown.
    code_editor: Option<code_editor::ActiveCodeEditor>,
    /// Lazily spawned and retained for this Automations-window lifetime.
    pub(super) language_service:
        Option<smudgy_script::language_service_worker::LanguageServiceHost>,
    /// Saved-source graph currently installed beneath the active editor overlay.
    language_project_context: Option<code_editor::LanguageProjectContext>,
    /// Context selected by the newest editor binding, which may still be awaiting refresh.
    language_project_target_context: Option<code_editor::LanguageProjectContext>,
    /// Exact in-flight graph refresh. Only its acknowledgement commits the installed context.
    pending_language_project_refresh: Option<code_editor::PendingLanguageProjectRefresh>,
    /// Stable identities for saved module/package sources during this window lifetime.
    language_source_ids:
        HashMap<code_editor::LanguageSourceKey, smudgy_script::language_service::DocumentId>,
    /// Local editor-mount fence for delayed upstream tasks such as clipboard reads.
    code_editor_mount_generation: u64,
    next_language_graph_generation: u64,
    pub(super) next_language_request_id: u64,
    pub(super) next_code_disk_revision: u64,
    /// Legacy plaintext body used only while a hotkey's behavior is Send Text.
    pub(super) hotkey_text_content: text_editor::Content,
    /// The send-text action draft, held separately from the script draft so
    /// switching action tabs never destroys work. Save writes whichever tab
    /// is active.
    pub(super) send_text_content: text_editor::Content,
    /// Whether the send-text draft has been edited (or came from disk). An
    /// unpinned draft is regenerated from the live matcher on every edit.
    pub(super) action_text_pinned: bool,
    /// As [`Self::action_text_pinned`], for the script draft.
    pub(super) action_script_pinned: bool,
    /// The language the Run JavaScript tab writes: `JS`, or `TS` when the
    /// automation opened as TypeScript (a TS alias stays TS on save).
    pub(super) action_script_lang: ScriptLang,
    /// The alias Simple-pattern field's buffer; `alias_draft.pattern_source`
    /// mirrors it after every edit (the draft stays the compile input).
    pub(super) alias_pattern_content: text_editor::Content,
    /// The alias Regex field's buffer, mirrored into `alias_draft.regex_source`.
    pub(super) alias_regex_content: text_editor::Content,
    /// One buffer per trigger matcher row, kept index-aligned with the open
    /// trigger's `rows` through every add/remove/reorder.
    pub(super) trigger_row_contents: Vec<text_editor::Content>,
    pub(super) hotkey_state: Vec<MaybePhysicalKey>,
    /// The alias editor's matcher draft (kind + every kind's buffers), seeded
    /// on open/create like `hotkey_state` and consumed at save.
    pub(super) alias_draft: model::AliasMatcherDraft,
    /// Whether the "When it runs" module is disclosed by the user's click.
    /// Non-default values force it open regardless (and it cannot re-hide
    /// while they hold); reset when an editor opens.
    pub(super) order_revealed: bool,
    /// Whether the Try-it accordion is expanded; collapsed when an editor opens.
    pub(super) try_it_open: bool,
    /// Whether the Parsing picker's floating list is open.
    pub(super) parsing_open: bool,
    /// The Parsing picker's keyboard cursor (an index into
    /// `ParseModeChoice::ALL`).
    pub(super) parsing_cursor: usize,
    pub(super) test_input: String,
    pub(super) dirty: bool,
    pub(super) pending_nav: Option<Box<Message>>,
    pub(super) confirm_folder_delete: bool,

    // ---- package dependency graph ------------------------------------------
    pub(super) graph: PackageGraph,
    /// Installed-package specifiers whose newest resolvable version's closure permission union
    /// exceeds the consented grant — the engine holds them at an older fitting version (or won't
    /// load them), so the tree flags them orange and the manage pane shows "update blocked"
    /// (`PACKAGE-ISOLATES-CONSENT-TRUST.md`). Populated by the background graph resolve.
    pub(super) blocked_updates: HashSet<String>,

    // ---- owned (local) package state --------------------------------------
    pub(super) local_package: Option<Box<LocalPackage>>,
    pub(super) local_readme: Option<markdown::Content>,
    pub(super) owned_selected_file: Option<String>,
    /// Inline rename buffer for the open local package (the folder name is its identity). `Some`
    /// while the rename field is showing; `None` otherwise.
    pub(super) rename_buffer: Option<String>,
    /// The editable manifest form for the open owned package (the rich editor for its
    /// `smudgy.package.json`). Seeded on open + after a Save; `None` off-pane.
    pub(super) manifest_draft: Option<ManifestDraft>,
    /// Whether the manifest draft has unsaved edits (independent of the script-editor `dirty`
    /// flag, which guards a different pane).
    pub(super) manifest_dirty: bool,
    /// Whether the manifest section is in the structured editor (vs the default read-only summary).
    pub(super) manifest_editing: bool,
    /// Which manifest-editor tab is showing (view-only; reset to `Settings` when a package opens).
    pub(super) manifest_tab: ManifestTab,
    pub(super) authoring_busy: bool,
    pub(super) authoring_feedback: Option<String>,
    /// The latest publish command/output, scoped to the package that produced it. Kept separate
    /// from general authoring feedback so it can render beside Publish in a bounded console.
    publish_output: Option<PublishOutput>,
    pub(super) confirm_delete_local: bool,
    pub(super) share_package_id: Option<Uuid>,
    pub(super) share_is_public: bool,
    pub(super) share_friends: Vec<FriendView>,
    pub(super) share_grants: Vec<PackageGrantView>,
    pub(super) share_versions: Vec<VersionListItem>,
    pub(super) share_busy: bool,
    pub(super) share_feedback: Option<String>,

    // ---- installed package state ------------------------------------------
    pub(super) installed_open: Option<Box<LockedPackage>>,
    pub(super) installed_detail: Option<Box<ResolvedPackageWire>>,
    /// The cloud package metadata (rating, install count) for the open installed package, fetched
    /// best-effort alongside the detail resolve. `None` for a local/owned package, while loading, or
    /// when the fetch failed — gating the rating UI on `Some` keeps it to real cloud packages.
    /// Replaced by the fresh `PackageDetail` the server returns when the user rates.
    pub(super) installed_rating: Option<Box<PackageDetail>>,
    pub(super) installed_versions: Vec<String>,
    pub(super) installed_selected_file: Option<String>,
    /// Which tab of the installed-package "README & source" area is showing (README vs Source).
    pub(super) installed_file_tab: InstalledFileTab,
    /// On-demand source for the installed-package source browser, keyed by module `content_hash`
    /// (content-addressed, so identical blobs share an entry and a late fetch is self-validating).
    /// Populated lazily when a file is selected; cleared when a different installed package opens.
    pub(super) installed_source: HashMap<String, FilePreview>,
    pub(super) manage_busy: bool,
    pub(super) manage_feedback: Option<String>,
    pub(super) confirm_uninstall: bool,
    /// The auto-installed required roots that would become **orphans** if the open package were
    /// uninstalled — apt-style, surfaced in the uninstall confirmation so the user can remove them
    /// too (`script/REQUIRED-PACKAGES.md`). Computed asynchronously when uninstall is requested
    /// (resolving the installed packages' `requires`); empty when nothing would be orphaned.
    pub(super) uninstall_orphans: Vec<String>,
    /// The installed packages that **`require`** the open package and would break if it were removed
    /// — they are removed alongside it (forced, never kept). Computed with `uninstall_orphans` from
    /// `SharedPackageLock::plan_removal` when uninstall is requested (`script/REQUIRED-PACKAGES.md`).
    pub(super) uninstall_breaks: Vec<String>,
    /// Two-step confirm gate for the heavy Trust action.
    pub(super) confirm_trust: bool,
    /// A pending update re-prompt for the open installed package: the new version's added
    /// permission asks beyond the consented baseline. `None` when there's nothing new to grant.
    pub(super) update_delta: Option<UpdateDelta>,

    // ---- discover state ----------------------------------------------------
    pub(super) discover_query: String,
    pub(super) discover_scope: DiscoverScope,
    pub(super) discover_results: Vec<PackageSearchResult>,
    /// The dashboard "Discover" teaser: the top results of a default ([`DiscoverScope::Relevant`])
    /// empty-query search, loaded on window init. Kept separate from `discover_results` so it stays
    /// stable regardless of how the user later searches/filters inside the Discover pane.
    pub(super) featured_packages: Vec<PackageSearchResult>,
    pub(super) discover_owner: Option<String>,
    pub(super) discover_detail: Option<Box<PackageDetail>>,
    pub(super) discover_readme: Option<markdown::Content>,
    pub(super) discover_comments: Vec<CommentView>,
    pub(super) discover_comment_input: String,
    pub(super) discover_busy: bool,
    pub(super) discover_error: Option<String>,
    /// The always-shown install confirmation; shown before any lock entry is written.
    pub(super) consent_prompt: Option<ConsentPrompt>,
    /// Monotonic generation for the in-flight install resolve; bumped on `begin_install` and on any
    /// action that abandons a pending install, so a late async result that no longer matches is
    /// discarded instead of popping a stale consent window.
    pub(super) install_seq: InstallSeq,
    /// Monotonic generation for the in-flight manage-pane detail load; bumped when the open package
    /// changes (`clear_selection`), is re-resolved (update-mode change), or is uninstalled, so a late
    /// async result that no longer matches is discarded instead of repainting a superseded package.
    pub(super) detail_seq: DetailSeq,
    pub(super) param_prompt: Option<ParamPrompt>,
    /// The remaining install-time required-params prompts to show after the current one, in order:
    /// a required install configures the chosen package and each co-installed required root in turn,
    /// so this holds the not-yet-shown prompts (`script/REQUIRED-PACKAGES.md`). Empty when the
    /// current prompt (if any) is the last. Drained by `advance_param_prompt_queue`.
    pub(super) param_prompt_queue: Vec<ParamPrompt>,
    /// The inline param-value editor for the open package pane (installed or owned). Seeded when a
    /// package that declares params opens; `None` otherwise. Independent of `param_prompt`, which is
    /// the install-time required-params gate.
    pub(super) param_config: Option<ParamConfig>,

    // ---- private & shared --------------------------------------------------
    pub(super) shared_with_me: Option<Vec<PackageDetail>>,
    /// The caller's own cloud packages (`GET /packages/mine`), public and private. `None`
    /// until the "Private & Shared" pane loads them. Surfaces packages the owner has no
    /// other way to see — e.g. a private package published from another machine.
    pub(super) my_cloud_packages: Option<Vec<PackageDetail>>,

    // ---- command palette ---------------------------------------------------
    pub(super) palette_open: bool,
    pub(super) palette_query: String,
    pub(super) palette_cursor: usize,

    // ---- toast -------------------------------------------------------------
    pub(super) toast: Option<String>,
    pub(super) toast_gen: u64,
}

/// A subscription stream of this session's script-created automation updates: waits for the
/// session runtime to exist, subscribes to its automation broadcast, and yields events
/// (skipping lag, ending when the session shuts down).
fn automation_stream(session_id: SessionId) -> impl iced::futures::Stream<Item = AutomationEvent> {
    use tokio::sync::broadcast::error::RecvError;

    enum State {
        Connecting,
        Streaming(tokio::sync::broadcast::Receiver<AutomationEvent>),
    }

    iced::futures::stream::unfold(State::Connecting, move |state| async move {
        let mut rx = match state {
            State::Streaming(rx) => rx,
            State::Connecting => loop {
                if let Some(runtime) = smudgy_core::session::registry::get_runtime(session_id) {
                    break runtime.subscribe_automations();
                }
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            },
        };
        loop {
            match rx.recv().await {
                Ok(event) => return Some((event, State::Streaming(rx))),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    })
}

/// A subscription stream of this session's runtime-catalogue snapshots (the store-inspector
/// pane's data): waits for the session runtime, subscribes to its catalogue broadcast, and
/// yields snapshots. On lag it just continues — every message is a full snapshot, so the
/// latest one is all that matters.
fn catalogue_stream(session_id: SessionId) -> impl iced::futures::Stream<Item = CatalogueEvent> {
    use tokio::sync::broadcast::error::RecvError;

    enum State {
        Connecting,
        Streaming(tokio::sync::broadcast::Receiver<CatalogueEvent>),
    }

    iced::futures::stream::unfold(State::Connecting, move |state| async move {
        let mut rx = match state {
            State::Streaming(rx) => rx,
            State::Connecting => loop {
                if let Some(runtime) = smudgy_core::session::registry::get_runtime(session_id) {
                    break runtime.subscribe_catalogue();
                }
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            },
        };
        loop {
            match rx.recv().await {
                Ok(event) => return Some((event, State::Streaming(rx))),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    })
}

impl AutomationsWindow {
    pub fn new(
        window_id: window::Id,
        server_name: String,
        cloud: CloudHandles,
        session_id: SessionId,
    ) -> Self {
        let mud_host = server::load_server(&server_name)
            .ok()
            .map(|server| server.config.host);
        let advanced_features =
            smudgy_core::models::settings::load_settings().advanced_scripting_features;
        Self {
            window_id,
            server_name,
            cloud,
            session_id,
            mud_host,
            advanced_features,
            scripts: BTreeMap::new(),
            packages: PackageTree::new(),
            modules: Vec::new(),
            local_packages: Vec::new(),
            installed_packages: Vec::new(),
            live: LiveAutomations::default(),
            expanded_creators: HashSet::new(),
            show_all_creators: HashSet::new(),
            catalogue: None,
            store_toggled: HashSet::new(),
            selection: Selection::Dashboard,
            collapsed_folders: HashSet::new(),
            pane: Pane::Dashboard,
            search: String::new(),
            chip: Chip::All,
            new_menu_open: false,
            code_editor: None,
            language_service: None,
            language_project_context: None,
            language_project_target_context: None,
            pending_language_project_refresh: None,
            language_source_ids: HashMap::new(),
            code_editor_mount_generation: 0,
            next_language_graph_generation: 2,
            next_language_request_id: 1,
            next_code_disk_revision: 1,
            hotkey_text_content: text_editor::Content::new(),
            send_text_content: text_editor::Content::new(),
            action_text_pinned: false,
            action_script_pinned: false,
            action_script_lang: ScriptLang::JS,
            alias_pattern_content: text_editor::Content::new(),
            alias_regex_content: text_editor::Content::new(),
            trigger_row_contents: Vec::new(),
            hotkey_state: Vec::new(),
            alias_draft: model::AliasMatcherDraft::default(),
            order_revealed: false,
            try_it_open: false,
            parsing_open: false,
            parsing_cursor: 0,
            test_input: String::new(),
            dirty: false,
            pending_nav: None,
            confirm_folder_delete: false,
            graph: PackageGraph::default(),
            blocked_updates: HashSet::new(),
            local_package: None,
            local_readme: None,
            owned_selected_file: None,
            rename_buffer: None,
            manifest_draft: None,
            manifest_dirty: false,
            manifest_editing: false,
            manifest_tab: ManifestTab::default(),
            authoring_busy: false,
            authoring_feedback: None,
            publish_output: None,
            confirm_delete_local: false,
            share_package_id: None,
            share_is_public: false,
            share_friends: Vec::new(),
            share_grants: Vec::new(),
            share_versions: Vec::new(),
            share_busy: false,
            share_feedback: None,
            installed_open: None,
            installed_detail: None,
            installed_rating: None,
            installed_versions: Vec::new(),
            installed_selected_file: None,
            installed_file_tab: InstalledFileTab::default(),
            installed_source: HashMap::new(),
            manage_busy: false,
            manage_feedback: None,
            confirm_uninstall: false,
            uninstall_orphans: Vec::new(),
            uninstall_breaks: Vec::new(),
            confirm_trust: false,
            update_delta: None,
            discover_query: String::new(),
            discover_scope: DiscoverScope::default(),
            discover_results: Vec::new(),
            featured_packages: Vec::new(),
            discover_owner: None,
            discover_detail: None,
            discover_readme: None,
            discover_comments: Vec::new(),
            discover_comment_input: String::new(),
            discover_busy: false,
            discover_error: None,
            consent_prompt: None,
            install_seq: InstallSeq::default(),
            detail_seq: DetailSeq::default(),
            param_prompt: None,
            param_prompt_queue: Vec::new(),
            param_config: None,
            shared_with_me: None,
            my_cloud_packages: None,
            palette_open: false,
            palette_query: String::new(),
            palette_cursor: 0,
            toast: None,
            toast_gen: 0,
        }
    }

    pub fn init(&self) -> Task<Message> {
        Task::batch([
            Task::done(self.load_scripts_message()),
            Task::done(Message::LoadFolders),
            Task::done(Message::LoadModules),
            Task::done(Message::LoadLocalPackages),
            Task::done(Message::LoadInstalledPackages),
            Task::done(Message::LoadFeaturedDiscover),
        ])
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Ctrl/⌘+P opens the palette; arrows/enter/escape drive it while open.
    /// Navigation keys only act on events no focused widget captured, so they
    /// don't fight text inputs elsewhere.
    pub fn subscription(&self) -> Subscription<Message> {
        let keyboard = iced::event::listen_with(|event, status, event_window| {
            let IcedEvent::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event
            else {
                return None;
            };
            match (key.as_ref(), status) {
                _ if code_completion_shortcut(&key, modifiers, status) => {
                    Some(Message::TriggerCodeCompletion)
                }
                (keyboard::Key::Character("p"), _) if modifiers.command() => {
                    Some(Message::OpenPalette)
                }
                (keyboard::Key::Named(Named::Escape), Status::Ignored) => {
                    Some(Message::ClosePalette)
                }
                (keyboard::Key::Named(Named::ArrowDown), Status::Ignored) => {
                    Some(Message::PaletteMove(1))
                }
                (keyboard::Key::Named(Named::ArrowUp), Status::Ignored) => {
                    Some(Message::PaletteMove(-1))
                }
                (keyboard::Key::Named(Named::Enter), Status::Ignored) => Some(Message::PaletteRun),
                (keyboard::Key::Named(Named::Tab), status) => {
                    tab_traversal(modifiers, status).map(|backwards| {
                        if backwards {
                            Message::FocusPrevious(event_window)
                        } else {
                            Message::FocusNext(event_window)
                        }
                    })
                }
                _ => None,
            }
        });
        // Stream this session's script-created automation updates, keyed by session id so
        // iced keeps a single broadcast subscription (one runtime receiver) across renders.
        let automations =
            Subscription::run_with(self.session_id, |session_id| automation_stream(*session_id))
                .map(Message::AutomationEvent);
        let mut subscriptions = vec![keyboard, automations];
        // The catalogue broadcast is subscribed only while the store pane is showing: the
        // runtime builds snapshots only while receivers exist, so a closed pane costs it
        // nothing, and re-opening gets a fresh snapshot (the new-subscriber resync).
        if matches!(self.pane, Pane::StoreInspector) {
            subscriptions.push(
                Subscription::run_with(self.session_id, |session_id| catalogue_stream(*session_id))
                    .map(Message::CatalogueEvent),
            );
        }
        if self.language_service.is_some() {
            subscriptions.push(
                iced::time::every(Duration::from_millis(50)).map(|_| Message::PollLanguageService),
            );
        }
        Subscription::batch(subscriptions)
    }

    /// Pops a toast and schedules its auto-dismiss (~2.2s).
    pub(super) fn show_toast(&mut self, message: impl Into<String>) -> Task<Message> {
        self.toast_gen += 1;
        let toast_id = self.toast_gen;
        self.toast = Some(message.into());
        Task::perform(
            async move { tokio::time::sleep(Duration::from_millis(2200)).await },
            move |()| Message::DismissToast(toast_id),
        )
    }

    pub fn update(&mut self, message: Message) -> Update<Message, Event> {
        // Message tracing for GUI debugging: run with
        // `SMUDGY_LOG=smudgy_ui::windows::automations_window=trace` to watch
        // every message this window handles.
        log::trace!("{message:?}");
        // Unsaved-changes guard: defer navigation away from a dirty editor or an edited but
        // unsaved manifest draft (the rich manifest editor tracks its own dirty flag).
        if (self.dirty || self.manifest_dirty) && Self::is_guarded_navigation(&message) {
            self.pending_nav = Some(Box::new(message));
            return Update::none();
        }
        if Self::is_edit_message(&message) {
            self.dirty = true;
        }
        let refresh_generated = Self::affects_captures(&message);
        let mut update = match message {
            // -------- loading ----------------------------------------------
            Message::ScriptsLoaded(scripts, errors) => {
                self.scripts = scripts;
                self.merge_folders();
                if errors.is_empty() {
                    Update::none()
                } else {
                    self.pane = Pane::Error(errors);
                    Update::none()
                }
            }
            Message::LoadFolders => {
                self.packages =
                    core_packages::load_packages(&self.server_name).unwrap_or_else(|e| {
                        log::warn!("Failed to load folders for {}: {e}", self.server_name);
                        PackageTree::new()
                    });
                self.merge_folders();
                Update::none()
            }
            Message::LoadModules => {
                self.modules = smudgy_core::models::modules::list_modules(&self.server_name)
                    .unwrap_or_else(|e| {
                        log::warn!("Failed to list modules for {}: {e}", self.server_name);
                        Vec::new()
                    });
                Update::with_task(self.reconcile_module_language_project_reload())
            }
            Message::LoadLocalPackages => {
                self.local_packages =
                    smudgy_core::models::local_packages::list_local_packages(&self.server_name)
                        .unwrap_or_else(|e| {
                            log::warn!("Failed to list local packages: {e}");
                            Vec::new()
                        });
                self.rebuild_graph();
                Update::with_task(self.reconcile_owned_package_language_project_reload())
            }
            Message::LoadInstalledPackages => {
                // Self-heal before reading: a reserved-`local`-owner install whose folder is gone
                // can never resolve again and would render as a phantom installed package (and
                // fail to load every session) — lockfiles written by app versions whose package
                // delete left install entries behind carry such strays. One with a folder is
                // migrated to the account's nickname form once a nickname exists.
                let nickname = self.cloud.snapshot.get().nickname_text();
                match smudgy_core::models::shared_packages::reconcile_local_installs(
                    &self.server_name,
                    nickname.as_deref(),
                ) {
                    Ok(changed) if !changed.is_empty() => {
                        log::info!("Reconciled local package installs: {}", changed.join(", "));
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("Failed to reconcile local installs: {e}"),
                }
                self.installed_packages =
                    smudgy_core::models::shared_packages::load_lock(&self.server_name)
                        .map(|lock| lock.packages)
                        .unwrap_or_else(|e| {
                            log::warn!("Failed to load lockfile: {e}");
                            Vec::new()
                        });
                self.rebuild_graph();
                let mut task = self.resolve_graph_deps();
                if let Some(sweep) = self.sweep_stale_account_installs() {
                    task = Task::batch([task, sweep]);
                }
                Update::with_task(task)
            }
            // -------- live (script-created) automations --------------------
            Message::AutomationEvent(event) => {
                match event {
                    AutomationEvent::Reset(summaries) => self.live.reset(&summaries),
                    AutomationEvent::Changed(deltas) => self.live.apply(&deltas),
                }
                Update::none()
            }
            Message::ToggleCreator(id) => {
                if !self.expanded_creators.remove(&id) {
                    self.expanded_creators.insert(id);
                }
                Update::none()
            }
            Message::ToggleCreatorShowAll(id) => {
                if !self.show_all_creators.remove(&id) {
                    self.show_all_creators.insert(id);
                }
                Update::none()
            }

            // -------- session-store inspector --------------------------------
            Message::OpenStoreInspector => {
                self.clear_selection();
                self.selection = Selection::StoreInspector;
                self.pane = Pane::StoreInspector;
                Update::none()
            }
            Message::CatalogueEvent(CatalogueEvent::Snapshot(snapshot)) => {
                self.catalogue = Some(snapshot);
                Update::none()
            }
            Message::ToggleStoreNode(key) => {
                if !self.store_toggled.remove(&key) {
                    self.store_toggled.insert(key);
                }
                Update::none()
            }

            // -------- keyboard focus traversal ----------------------------
            Message::FocusColorControl(id) => Update::with_task(operation::focus(id)),
            Message::FocusNext(event_window) if event_window == self.window_id => {
                Update::with_task(operation::focus_next())
            }
            Message::FocusPrevious(event_window) if event_window == self.window_id => {
                Update::with_task(operation::focus_previous())
            }
            Message::FocusNext(_) | Message::FocusPrevious(_) => Update::none(),

            // -------- navigation -------------------------------------------
            Message::ShowDashboard => {
                self.clear_selection();
                self.selection = Selection::Dashboard;
                self.pane = Pane::Dashboard;
                Update::none()
            }
            Message::SelectScript(key) => self.open_script(key),
            Message::SelectFolder(path) => self.open_folder(path),
            Message::SelectModule(subpath) => self.open_module(subpath),
            Message::SelectOwnedPackage(name) => self.open_owned_package(name),
            Message::SelectInstalledPackage(spec) => self.open_installed_package(spec),
            Message::SelectDependency { parent, spec } => self.open_dependency(parent, spec),
            Message::SelectCreatorAutomation {
                creator_id,
                kind,
                name,
            } => self.open_creator_automation(creator_id, kind, name),
            Message::ToggleFolderExpanded(path) => {
                if !self.collapsed_folders.remove(&path) {
                    self.collapsed_folders.insert(path);
                }
                Update::none()
            }

            // -------- sidebar ----------------------------------------------
            Message::ToggleNewMenu => {
                self.new_menu_open = !self.new_menu_open;
                Update::none()
            }
            Message::SearchChanged(q) => {
                self.search = q;
                Update::none()
            }
            Message::ClearSearch => {
                self.search.clear();
                Update::none()
            }
            Message::SelectChip(chip) => {
                self.chip = chip;
                Update::none()
            }

            // -------- create -----------------------------------------------
            Message::NewAlias => self.new_alias(),
            Message::NewTrigger => self.new_trigger(),
            Message::NewHotkey => self.new_hotkey(),
            Message::NewFolder => self.new_folder(),
            Message::NewModule => self.new_module(),
            Message::NewPackage => self.new_package(),

            // -------- editor fields ----------------------------------------
            Message::SetName(name) => {
                if let Pane::Editor(state) = &mut self.pane {
                    state.name = name;
                    // The name IS the command: editing it drops any stored
                    // command-word override a legacy save carried, so the
                    // command follows the name from here on.
                    if matches!(state.node, EditNode::Alias(_)) {
                        self.alias_draft.command_override = None;
                    }
                }
                Update::none()
            }
            Message::AliasRegexAction(action) => {
                // The Regex kind's source buffer; `pattern` on the definition is
                // written from the draft at save time.
                editors::perform_single_line(&mut self.alias_regex_content, action);
                self.alias_draft.regex_source =
                    editors::single_line_text(&self.alias_regex_content);
                Update::none()
            }
            Message::SetAliasKind(kind) => {
                self.alias_draft.kind = kind;
                Update::none()
            }
            Message::SetArgName(i, name) => {
                if let Some(arg) = self.alias_draft.args.get_mut(i) {
                    arg.name = name;
                }
                Update::none()
            }
            Message::SetArgKind(i, kind) => {
                if let Some(arg) = self.alias_draft.args.get_mut(i) {
                    arg.kind = kind;
                    self.alias_draft.normalize_args();
                }
                Update::none()
            }
            Message::AddArg => {
                self.alias_draft
                    .args
                    .push(smudgy_core::models::matchers::ArgSpec {
                        name: format!("arg{}", self.alias_draft.args.len() + 1),
                        kind: smudgy_core::models::matchers::ArgKind::Required,
                    });
                self.alias_draft.normalize_args();
                Update::none()
            }
            Message::RemoveArg(i) => {
                if i < self.alias_draft.args.len() {
                    self.alias_draft.args.remove(i);
                    self.alias_draft.normalize_args();
                }
                Update::none()
            }
            Message::SetCmdMode(mode) => {
                self.alias_draft.cmd_mode = mode;
                self.alias_draft.normalize_args();
                Update::none()
            }
            Message::SetParseMode(parse) => {
                self.alias_draft.parse = parse;
                self.parsing_open = false;
                Update::none()
            }
            Message::OpenParsingPicker => {
                self.parsing_open = true;
                // The cursor starts on the current choice.
                self.parsing_cursor = model::ParseModeChoice::ALL
                    .iter()
                    .position(|choice| choice.0 == self.alias_draft.parse)
                    .unwrap_or(0);
                Update::none()
            }
            Message::CloseParsingPicker => {
                self.parsing_open = false;
                Update::none()
            }
            Message::MoveParsingCursor(delta) => {
                let len = model::ParseModeChoice::ALL.len() as i32;
                let cursor = self.parsing_cursor as i32 + delta;
                self.parsing_cursor = cursor.rem_euclid(len) as usize;
                Update::none()
            }
            Message::AliasPatternAction(action) => {
                editors::perform_single_line(&mut self.alias_pattern_content, action);
                self.alias_draft.pattern_source =
                    editors::single_line_text(&self.alias_pattern_content);
                Update::none()
            }
            Message::ToggleAnchorStart => {
                self.alias_draft.anchor_start = !self.alias_draft.anchor_start;
                Update::none()
            }
            Message::ToggleAnchorEnd => {
                self.alias_draft.anchor_end = !self.alias_draft.anchor_end;
                Update::none()
            }
            Message::TogglePrompt => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { prompt, .. },
                    ..
                }) = &mut self.pane
                {
                    *prompt = !*prompt;
                }
                Update::none()
            }
            Message::RevealOrder => {
                self.order_revealed = true;
                Update::none()
            }
            Message::HideOrder => {
                self.order_revealed = false;
                Update::none()
            }
            Message::InsertReference(reference) => {
                // The badge inserts into whichever action tab is active.
                if self.open_action_language() == Some(ScriptLang::Plaintext) {
                    self.action_text_pinned = true;
                    self.send_text_content.perform(text_editor::Action::Edit(
                        text_editor::Edit::Paste(Arc::new(reference)),
                    ));
                    Update::none()
                } else {
                    self.action_script_pinned = true;
                    let Some(message) = self
                        .bind_code_editor_message(code_editor::IcedEditorMessage::Paste(reference))
                    else {
                        return Update::none();
                    };
                    let (task, _) = self.update_code_editor(&message);
                    Update::with_task(task)
                }
            }
            Message::SetScriptFolder(folder) => self.set_script_folder(folder),
            Message::SetBehavior(language) => {
                let previous = match &self.pane {
                    Pane::Editor(EditorState { node, .. }) => match node {
                        EditNode::Alias(alias) => {
                            Some((alias.language, code_editor::CodeDocument::Alias))
                        }
                        EditNode::Hotkey(hotkey) => {
                            Some((hotkey.language, code_editor::CodeDocument::Hotkey))
                        }
                        EditNode::Trigger { language, .. } => {
                            Some((*language, code_editor::CodeDocument::Trigger))
                        }
                    },
                    _ => None,
                };
                if let Pane::Editor(state) = &mut self.pane {
                    match &mut state.node {
                        EditNode::Alias(a) => a.language = language,
                        EditNode::Hotkey(h) => h.language = language,
                        EditNode::Trigger { language: l, .. } => *l = language,
                    }
                }
                let Some((previous, kind)) = previous else {
                    return Update::none();
                };
                if previous == language {
                    Update::none()
                } else if kind == code_editor::CodeDocument::Hotkey {
                    if previous == ScriptLang::Plaintext {
                        let text = self.hotkey_text_content.text();
                        Update::with_task(self.bind_code_editor(
                            &text,
                            code_editor::script_language(language),
                            kind,
                        ))
                    } else if language == ScriptLang::Plaintext {
                        // A hotkey has one body regardless of execution language.
                        // Transfer the authoritative code buffer back before the
                        // plaintext editor becomes visible.
                        let text = self.code_editor_text();
                        self.hotkey_text_content = text_editor::Content::with_text(&text);
                        self.clear_code_editor();
                        Update::none()
                    } else {
                        let text = self.code_editor_text();
                        Update::with_task(self.bind_code_editor(
                            &text,
                            code_editor::script_language(language),
                            kind,
                        ))
                    }
                } else if language != ScriptLang::Plaintext {
                    let text = self.code_editor_text();
                    self.action_script_lang = language;
                    Update::with_task(self.bind_code_editor(
                        &text,
                        code_editor::script_language(language),
                        kind,
                    ))
                } else {
                    Update::none()
                }
            }
            Message::AdjustPriority(delta) => {
                if let Pane::Editor(state) = &mut self.pane {
                    match &mut state.node {
                        EditNode::Alias(alias) => {
                            alias.priority = alias.priority.saturating_add(delta);
                        }
                        EditNode::Trigger { priority, .. } => {
                            *priority = priority.saturating_add(delta);
                        }
                        EditNode::Hotkey(_) => {}
                    }
                }
                Update::none()
            }
            Message::ToggleFallthrough => {
                if let Pane::Editor(state) = &mut self.pane {
                    match &mut state.node {
                        EditNode::Alias(alias) => alias.fallthrough = !alias.fallthrough,
                        EditNode::Trigger { fallthrough, .. } => {
                            *fallthrough = !*fallthrough;
                        }
                        EditNode::Hotkey(_) => {}
                    }
                }
                Update::none()
            }
            Message::ToggleAllowSelfMatch => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Alias(alias),
                    ..
                }) = &mut self.pane
                {
                    alias.allow_self_match = !alias.allow_self_match;
                }
                Update::none()
            }
            Message::CodeEditorAction(action)
                if matches!(
                    action.message,
                    code_editor::IcedEditorMessage::WriteRequested
                ) =>
            {
                if !self.code_editor_message_is_current(&action) {
                    return Update::none();
                }
                match &self.pane {
                    Pane::Editor(_) => self.save_open(),
                    Pane::Module(_) => self.save_module(),
                    Pane::OwnedPackage => self.save_owned_file(),
                    _ => Update::none(),
                }
            }
            Message::CodeEditorAction(action) => {
                let (task, changed) = self.update_code_editor(&action);
                if changed {
                    self.dirty = true;
                    if matches!(self.pane, Pane::Editor(_)) {
                        self.action_script_pinned = true;
                    }
                }
                Update::with_task(task)
            }
            Message::ApplyCodeCompletion(selection) => {
                let (task, changed) = self.apply_code_completion(selection);
                if changed {
                    self.dirty = true;
                    if matches!(self.pane, Pane::Editor(_)) {
                        self.action_script_pinned = true;
                    }
                }
                Update::with_task(task)
            }
            Message::TriggerCodeCompletion => {
                let Some(message) = self.bind_code_editor_message(
                    code_editor::IcedCodeEditorSurface::explicit_completion_message(),
                ) else {
                    return Update::none();
                };
                let (task, _) = self.update_code_editor(&message);
                Update::with_task(task)
            }
            Message::DismissCodeOverlays => {
                if let Some(editor) = &mut self.code_editor {
                    editor.dismiss_overlays();
                }
                Update::none()
            }
            Message::NavigateCodeDefinition(navigation) => {
                self.navigate_code_definition(navigation)
            }
            Message::HotkeyTextAction(action) => {
                self.hotkey_text_content.perform(action);
                Update::none()
            }
            Message::SendTextAction(action) => {
                if action.is_edit() {
                    self.action_text_pinned = true;
                }
                self.send_text_content.perform(action);
                Update::none()
            }
            Message::ToggleTryIt => {
                self.try_it_open = !self.try_it_open;
                Update::none()
            }
            Message::SetTestInput(value) => {
                self.test_input = value;
                Update::none()
            }
            Message::ToggleEnabled => self.toggle_open_enabled(),
            Message::MarkHotkeyState(keys) => {
                self.hotkey_state = keys;
                Update::none()
            }
            Message::AddPattern => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                {
                    // "Another" means another of what you have: the new row
                    // copies the last Match row's syntax, defaulting to the
                    // Simple pattern for the first one.
                    let syntax = rows
                        .iter()
                        .rev()
                        .find(|row| row.role == PatternKind::Match)
                        .map_or(
                            smudgy_core::models::matchers::MatcherSyntax::Pattern,
                            |row| row.syntax,
                        );
                    rows.push(model::TriggerRow {
                        syntax,
                        ..model::TriggerRow::new(PatternKind::Match)
                    });
                    self.trigger_row_contents.push(text_editor::Content::new());
                }
                Update::none()
            }
            Message::AddExceptionRow => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                {
                    rows.push(model::TriggerRow {
                        syntax: smudgy_core::models::matchers::MatcherSyntax::Pattern,
                        ..model::TriggerRow::new(PatternKind::Anti)
                    });
                    self.trigger_row_contents.push(text_editor::Content::new());
                }
                Update::none()
            }
            Message::AddRawRow => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                {
                    rows.push(model::TriggerRow::new(PatternKind::Raw));
                    self.trigger_row_contents.push(text_editor::Content::new());
                }
                Update::none()
            }
            Message::SetTriggerCard(card) => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                {
                    let (syntax, role) = card.shape();
                    let matcher_indexes: Vec<usize> = rows
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| row.role != PatternKind::Anti)
                        .map(|(i, _)| i)
                        .collect();
                    match matcher_indexes[..] {
                        [] => {
                            rows.push(model::TriggerRow {
                                syntax,
                                ..model::TriggerRow::new(role)
                            });
                            self.trigger_row_contents.push(text_editor::Content::new());
                        }
                        [index] => {
                            if let Some(row) = rows.get_mut(index) {
                                row.syntax = syntax;
                                row.role = role;
                                if role == PatternKind::Raw {
                                    row.color = None;
                                }
                            }
                        }
                        // The cards are not shown at two or more matchers.
                        _ => {}
                    }
                }
                Update::none()
            }
            Message::MoveRowUp(i) => {
                self.move_trigger_row(i, false);
                Update::none()
            }
            Message::MoveRowDown(i) => {
                self.move_trigger_row(i, true);
                Update::none()
            }
            Message::RemovePattern(i) => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && i < rows.len()
                {
                    rows.remove(i);
                    if i < self.trigger_row_contents.len() {
                        self.trigger_row_contents.remove(i);
                    }
                }
                Update::none()
            }
            Message::RowSourceAction(i, action) => {
                if let Some(content) = self.trigger_row_contents.get_mut(i) {
                    editors::perform_single_line(content, action);
                    let source = editors::single_line_text(content);
                    if let Pane::Editor(EditorState {
                        node: EditNode::Trigger { rows, .. },
                        ..
                    }) = &mut self.pane
                        && let Some(row) = rows.get_mut(i)
                    {
                        row.source = source;
                    }
                }
                Update::none()
            }
            Message::SetRowSyntax(i, syntax) => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    row.syntax = syntax;
                    // Raw implies Regex: choosing Pattern demotes the role.
                    if syntax == smudgy_core::models::matchers::MatcherSyntax::Pattern
                        && row.role == PatternKind::Raw
                    {
                        row.role = PatternKind::Match;
                    }
                }
                Update::none()
            }
            Message::ToggleRowAnchorStart(i) => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    row.anchor_start = !row.anchor_start;
                }
                Update::none()
            }
            Message::ToggleRowAnchorEnd(i) => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    row.anchor_end = !row.anchor_end;
                }
                Update::none()
            }
            Message::ToggleRowColor(i, enabled) => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                    && row.role != PatternKind::Raw
                {
                    row.set_color_enabled(enabled);
                }
                Update::none()
            }
            Message::SelectRowColorChannel(i, channel) => {
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    row.color_channel = channel;
                }
                Update::none()
            }
            Message::SelectRowColorKind(i, kind) => {
                use smudgy_core::models::matchers::{MatcherColor, MatcherColorChannel};
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    let channel = row.color_channel;
                    let draft = row.color_draft(channel);
                    let [r, g, b] = draft.exact_truecolor.last_valid;
                    let range = draft.color_range_last_valid;
                    if let Some(filter) = row.color.as_mut() {
                        let slot = match channel {
                            MatcherColorChannel::Foreground => &mut filter.foreground,
                            MatcherColorChannel::Background => &mut filter.background,
                        };
                        if model::MatcherColorKind::of(*slot) != kind {
                            *slot = match kind {
                                model::MatcherColorKind::Any => None,
                                model::MatcherColorKind::Ansi => {
                                    Some(MatcherColor::Ansi { index: 7 })
                                }
                                model::MatcherColorKind::Xterm => {
                                    Some(MatcherColor::Xterm { index: 7 })
                                }
                                model::MatcherColorKind::Truecolor => {
                                    Some(MatcherColor::Truecolor {
                                        r,
                                        g,
                                        b,
                                        range: None,
                                    })
                                }
                                model::MatcherColorKind::ColorRange => {
                                    Some(matcher_truecolor_from_range(range))
                                }
                            };
                        }
                    }
                }
                Update::none()
            }
            Message::SetRowAnsiColor(i, index) => {
                self.set_row_matcher_color(
                    i,
                    smudgy_core::models::matchers::MatcherColor::Ansi {
                        index: index.min(15),
                    },
                );
                Update::none()
            }
            Message::SetRowXtermColor(i, index) => {
                self.set_row_matcher_color(
                    i,
                    smudgy_core::models::matchers::MatcherColor::Xterm { index },
                );
                Update::none()
            }
            Message::SetRowColorRange(i, endpoint, message) => {
                let mut range = self
                    .trigger_row(i)
                    .map(|row| row.color_draft(row.color_channel).color_range_last_valid)
                    .unwrap_or_else(|| {
                        let point =
                            smudgy_core::models::matchers::MatcherHsv::from_rgb(255, 255, 255);
                        smudgy_core::models::matchers::MatcherHsvRange::from_to(point, point)
                    });
                let (mut from, mut to) = range.directed_endpoints();
                let initial = match endpoint {
                    model::ColorRangeEndpoint::First => from,
                    model::ColorRangeEndpoint::Second => to,
                };
                let mut picker = crate::components::color_picker::ColorPicker::from_hsv(
                    matcher_hsv_to_picker(initial),
                );
                let _ = picker.update(message);
                let hsv = picker_hsv_to_matcher(picker.hsv());
                match endpoint {
                    model::ColorRangeEndpoint::First => from = hsv,
                    model::ColorRangeEndpoint::Second => to = hsv,
                }
                range = smudgy_core::models::matchers::MatcherHsvRange::from_to(from, to);
                let color = matcher_truecolor_from_range(range);
                range = matcher_truecolor_range(color).unwrap_or(range);
                self.set_row_matcher_color(i, color);
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    let channel = row.color_channel;
                    let draft = row.color_draft_mut(channel);
                    draft.color_range_last_valid = range;
                    let (from, to) = range.directed_endpoints();
                    draft.color_range_hex[endpoint.index()] = matcher_hsv_hex(match endpoint {
                        model::ColorRangeEndpoint::First => from,
                        model::ColorRangeEndpoint::Second => to,
                    });
                }
                Update::none()
            }
            Message::SetRowColorRangeHex(i, endpoint, value) => {
                let parsed = model::parse_matcher_hex(&value);
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    let channel = row.color_channel;
                    row.color_draft_mut(channel).color_range_hex[endpoint.index()] = value;
                }
                if let Some((r, g, b)) = parsed {
                    let mut range = self
                        .trigger_row(i)
                        .map(|row| row.color_draft(row.color_channel).color_range_last_valid)
                        .unwrap_or_else(|| {
                            let point =
                                smudgy_core::models::matchers::MatcherHsv::from_rgb(255, 255, 255);
                            smudgy_core::models::matchers::MatcherHsvRange::from_to(point, point)
                        });
                    let (mut from, mut to) = range.directed_endpoints();
                    let old_hue = match endpoint {
                        model::ColorRangeEndpoint::First => from.hue,
                        model::ColorRangeEndpoint::Second => to.hue,
                    };
                    let mut hsv = smudgy_core::models::matchers::MatcherHsv::from_rgb(r, g, b);
                    if hsv.saturation == 0 {
                        hsv.hue = old_hue;
                    }
                    match endpoint {
                        model::ColorRangeEndpoint::First => from = hsv,
                        model::ColorRangeEndpoint::Second => to = hsv,
                    }
                    range = smudgy_core::models::matchers::MatcherHsvRange::from_to(from, to);
                    let color = matcher_truecolor_from_range(range);
                    range = matcher_truecolor_range(color).unwrap_or(range);
                    self.set_row_matcher_color(i, color);
                    if let Pane::Editor(EditorState {
                        node: EditNode::Trigger { rows, .. },
                        ..
                    }) = &mut self.pane
                        && let Some(row) = rows.get_mut(i)
                    {
                        let channel = row.color_channel;
                        row.color_draft_mut(channel).color_range_last_valid = range;
                    }
                }
                Update::none()
            }
            Message::SetRowExactTruecolorHex(i, value) => {
                let parsed = model::parse_matcher_hex(&value);
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    let channel = row.color_channel;
                    let draft = &mut row.color_draft_mut(channel).exact_truecolor;
                    draft.hex = value;
                    if let Some((r, g, b)) = parsed {
                        draft.rgb = [r.to_string(), g.to_string(), b.to_string()];
                        draft.last_valid = [r, g, b];
                    }
                }
                if let Some((r, g, b)) = parsed {
                    self.set_row_matcher_color(
                        i,
                        smudgy_core::models::matchers::MatcherColor::Truecolor {
                            r,
                            g,
                            b,
                            range: None,
                        },
                    );
                }
                Update::none()
            }
            Message::SetRowExactTruecolorRgb(i, component, value) => {
                let parsed = if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(row) = rows.get_mut(i)
                {
                    let channel = row.color_channel;
                    let draft = &mut row.color_draft_mut(channel).exact_truecolor;
                    draft.rgb[component.index()] = value;
                    let [red, green, blue] = &draft.rgb;
                    let parsed = red
                        .parse::<u8>()
                        .ok()
                        .zip(green.parse::<u8>().ok())
                        .zip(blue.parse::<u8>().ok())
                        .map(|((r, g), b)| (r, g, b));
                    if let Some((r, g, b)) = parsed {
                        draft.hex = format!("#{r:02x}{g:02x}{b:02x}");
                        draft.last_valid = [r, g, b];
                    }
                    parsed
                } else {
                    None
                };
                if let Some((r, g, b)) = parsed {
                    self.set_row_matcher_color(
                        i,
                        smudgy_core::models::matchers::MatcherColor::Truecolor {
                            r,
                            g,
                            b,
                            range: None,
                        },
                    );
                }
                Update::none()
            }
            Message::ToggleRowColorAttribute(i, attribute, selected) => {
                use smudgy_core::models::matchers::MatcherTextAttribute;
                if let Pane::Editor(EditorState {
                    node: EditNode::Trigger { rows, .. },
                    ..
                }) = &mut self.pane
                    && let Some(filter) = rows.get_mut(i).and_then(|row| row.color.as_mut())
                {
                    filter.attributes.retain(|current| *current != attribute);
                    if selected {
                        let incompatible = match attribute {
                            MatcherTextAttribute::Bold => Some(MatcherTextAttribute::Faint),
                            MatcherTextAttribute::Faint => Some(MatcherTextAttribute::Bold),
                            MatcherTextAttribute::Underline => {
                                Some(MatcherTextAttribute::DoubleUnderline)
                            }
                            MatcherTextAttribute::DoubleUnderline => {
                                Some(MatcherTextAttribute::Underline)
                            }
                            MatcherTextAttribute::SlowBlink => {
                                Some(MatcherTextAttribute::FastBlink)
                            }
                            MatcherTextAttribute::FastBlink => {
                                Some(MatcherTextAttribute::SlowBlink)
                            }
                            _ => None,
                        };
                        if let Some(incompatible) = incompatible {
                            filter.attributes.retain(|current| *current != incompatible);
                        }
                        filter.attributes.push(attribute);
                    }
                }
                Update::none()
            }

            // -------- save bar ---------------------------------------------
            Message::Save => self.save_open(),
            Message::Discard => {
                self.dirty = false;
                self.pending_nav = None;
                self.clear_selection();
                self.selection = Selection::Dashboard;
                self.pane = Pane::Dashboard;
                Update::none()
            }
            Message::Delete => self.delete_open(),
            Message::ConfirmDiscardNav => {
                match self.pending_nav.take() {
                    Some(msg) => match *msg {
                        // Definition results are state-fenced and can become stale while this
                        // confirmation is open. Execute synchronously and clear dirty state only
                        // if the jump really leaves the origin; otherwise the mounted draft must
                        // remain protected by the next navigation guard.
                        Message::NavigateCodeDefinition(navigation) => {
                            let (update, left_origin) =
                                self.navigate_code_definition_checked(navigation);
                            if left_origin {
                                self.accept_discarded_navigation();
                            }
                            update
                        }
                        message => {
                            // Other guarded navigation is replayed through the normal update path.
                            // Re-seed the manifest immediately so Discard cannot leave a dormant
                            // edited draft behind if the destination remains in this package.
                            self.accept_discarded_navigation();
                            Update::with_task(Task::done(message))
                        }
                    },
                    None => Update::none(),
                }
            }
            Message::CancelDiscardNav => {
                self.pending_nav = None;
                Update::none()
            }

            // -------- folder -----------------------------------------------
            Message::SetFolderPath(value) => {
                if let Pane::Folder(state) = &mut self.pane {
                    state.path = value;
                }
                Update::none()
            }
            Message::SaveFolder => self.save_folder(),
            Message::RequestDeleteFolder => {
                self.confirm_folder_delete = true;
                Update::none()
            }
            Message::CancelDeleteFolder => {
                self.confirm_folder_delete = false;
                Update::none()
            }
            Message::ConfirmDeleteFolder(delete_scripts) => self.delete_folder(delete_scripts),

            // -------- module -----------------------------------------------
            Message::SaveModule => self.save_module(),
            Message::SetNewModuleName(value) => {
                let is_module = if let Pane::Module(state) = &mut self.pane {
                    state.name.clone_from(&value);
                    true
                } else {
                    false
                };
                if !is_module {
                    return Update::none();
                }
                let language = code_editor::path_language(&value);
                if self
                    .code_editor
                    .as_ref()
                    .is_some_and(|editor| editor.document().language == language)
                {
                    Update::none()
                } else {
                    let text = self.code_editor_text();
                    Update::with_task(self.bind_code_editor(
                        &text,
                        language,
                        code_editor::CodeDocument::StandaloneModule,
                    ))
                }
            }
            Message::CreateModule => self.create_module(),

            // -------- owned package ----------------------------------------
            Message::SelectOwnedFile(subpath) => self.select_owned_file(subpath),
            Message::SaveOwnedFile => self.save_owned_file(),
            Message::EditManifest(edit) => self.apply_manifest_edit(edit),
            Message::SelectManifestTab(tab) => {
                self.manifest_tab = tab;
                Update::none()
            }
            Message::ManifestBeginEdit => self.begin_manifest_edit(),
            Message::SaveManifest => self.save_manifest(),
            Message::RevertManifest => self.revert_manifest(),
            Message::PublishOwned => self.publish_owned(),
            Message::PublishFinished { name, result } => {
                self.authoring_busy = false;
                match result {
                    Ok(summary) => {
                        let is_open_package = matches!(
                            &self.selection,
                            Selection::OwnedPackage(open) if open == &name
                        );
                        if is_open_package {
                            self.share_package_id = Some(summary.package_id);
                            self.share_is_public = summary.is_public;
                            if !self
                                .share_versions
                                .iter()
                                .any(|version| version.version == summary.version)
                            {
                                self.share_versions.insert(
                                    0,
                                    VersionListItem {
                                        version: summary.version.clone(),
                                        yanked: false,
                                        deleted: false,
                                        published_at: summary.published_at,
                                    },
                                );
                            }
                        }
                        let mut feedback = format!(
                            "smudgy> publish {name}\n{}",
                            crate::i18n::t!(
                                "automation-published",
                                "version" => &summary.version
                            )
                        );
                        if summary.typings_generated > 0 {
                            let typings_generated =
                                i64::try_from(summary.typings_generated).unwrap_or(i64::MAX);
                            feedback.push('\n');
                            feedback.push_str(&crate::i18n::t!(
                                "automation-published-typings",
                                "count" => typings_generated
                            ));
                        }
                        // Surface tsc warnings to the author — typings are best-effort, so a
                        // warning here never means the publish failed.
                        if !summary.typings_warnings.is_empty() {
                            feedback.push('\n');
                            feedback.push_str(&crate::i18n::t!(
                                "automation-typings-warning",
                                "warnings" => summary.typings_warnings.join("\n")
                            ));
                        }
                        // Show exactly what each dependency froze to — a publish pins the whole tree,
                        // so a stale range silently locking an old version is otherwise invisible.
                        if !summary.locked_dependencies.is_empty() {
                            let locked: Vec<String> = summary
                                .locked_dependencies
                                .iter()
                                .map(|(spec, ver)| {
                                    format!("{}@{ver}", spec.trim_start_matches("smudgy://"))
                                })
                                .collect();
                            feedback.push('\n');
                            feedback.push_str(&crate::i18n::t!(
                                "automation-locked-dependencies",
                                "dependencies" => locked.join(", ")
                            ));
                        }
                        // A range that excludes a newer published version (the 0.0.x caret footgun):
                        // non-fatal, but the author almost certainly wanted the newer one.
                        if !summary.dependency_warnings.is_empty() {
                            feedback.push_str(&format!(
                                "\n\u{26a0} {}",
                                summary.dependency_warnings.join("\n\u{26a0} ")
                            ));
                        }
                        // Interop-declaration warnings (duplicate/aliased handle exports, a
                        // handle the previous version published that this one drops): a handle
                        // name is the identity consumers import, so these deserve eyes even
                        // though the publish succeeded.
                        if !summary.interop_warnings.is_empty() {
                            feedback.push('\n');
                            feedback.push_str(&crate::i18n::t!(
                                "automation-interop-warning",
                                "warnings" => summary.interop_warnings.join("\n")
                            ));
                        }
                        self.publish_output = Some(PublishOutput {
                            package: name.clone(),
                            text: feedback,
                        });
                        let refresh = if is_open_package {
                            self.load_owned_share(name)
                        } else {
                            Task::none()
                        };
                        Update::with_task(Task::batch([
                            refresh,
                            self.show_toast(format!("Published v{}", summary.version)),
                        ]))
                    }
                    Err(e) => {
                        self.publish_output = Some(PublishOutput {
                            package: name.clone(),
                            text: format!(
                                "smudgy> publish {name}\n{}",
                                crate::i18n::t!(
                                    "automation-publish-failed",
                                    "error" => e.to_string()
                                )
                            ),
                        });
                        Update::none()
                    }
                }
            }
            Message::RequestDeleteOwned => {
                self.confirm_delete_local = true;
                Update::none()
            }
            Message::CancelDeleteOwned => {
                self.confirm_delete_local = false;
                Update::none()
            }
            Message::DeleteOwned => self.delete_owned(),
            Message::SetNewPackageName(value) => {
                if let Pane::NewPackage { name, .. } = &mut self.pane {
                    *name = value;
                }
                Update::none()
            }
            Message::CreatePackage => self.create_package(),
            Message::SetVisibility(public) => self.set_visibility(public),
            Message::VisibilityUpdated(result) => self.visibility_updated(result),
            Message::YankVersion { version, yanked } => self.yank_version(version, yanked),
            Message::DeleteVersion(version) => self.delete_version(version),
            Message::VersionsUpdated(result) => self.versions_updated(result),
            Message::ShareWithFriend(grantee) => self.share_with_friend(grantee),
            Message::GrantsUpdated(result) => self.grants_updated(result),
            Message::OwnedShareLoaded { name, result } => self.owned_share_loaded(&name, result),

            // -------- installed package ------------------------------------
            Message::InstalledDetailLoaded(seq, result) => {
                self.installed_detail_loaded(seq, *result)
            }
            Message::InstalledResolvedForGraph(spec, result) => {
                self.installed_resolved_for_graph(&spec, result)
            }
            Message::SetInstalledUpdateMode(mode) => self.set_installed_update_mode(mode),
            Message::TogglePackageEnabled(spec) => self.toggle_package_enabled(spec),
            Message::SetActiveMember {
                target_spec,
                siblings,
            } => self.set_active_member(target_spec, siblings),
            Message::ToggleLocalEnabled(name) => self.toggle_local_enabled(name),
            Message::SelectInstalledFile(subpath) => self.select_installed_file(subpath),
            Message::SelectInstalledFileTab(tab) => {
                self.installed_file_tab = tab;
                // Entering the Source tab with a file already selected: make sure its source is
                // loading/loaded (idempotent — no-ops when nothing is selected or it's cached).
                match tab {
                    InstalledFileTab::Source => self.ensure_selected_source(),
                    InstalledFileTab::Readme => Update::none(),
                }
            }
            Message::InstalledSourceLoaded { hash, result } => {
                self.installed_source_loaded(hash, result)
            }
            Message::RequestUninstall => self.request_uninstall(),
            Message::UninstallPlanComputed { breaks, orphans } => {
                // Only adopt the result if the user is still in the uninstall confirmation (it
                // wasn't cancelled while the resolve was in flight).
                if self.confirm_uninstall {
                    self.uninstall_breaks = breaks;
                    self.uninstall_orphans = orphans;
                }
                Update::none()
            }
            Message::UninstallKeepOrphans => {
                // Keep the offered orphans; the forced breaks still go.
                self.uninstall_orphans.clear();
                Update::none()
            }
            Message::CancelUninstall => {
                self.confirm_uninstall = false;
                self.uninstall_orphans.clear();
                self.uninstall_breaks.clear();
                Update::none()
            }
            Message::ConfirmUninstall => self.uninstall_installed(),
            Message::ForkPackage => self.fork_installed(),
            Message::ForkFinished(result) => self.fork_finished(result),
            Message::StaleAccountInstallsChecked(outcome) => {
                self.stale_account_installs_checked(outcome)
            }
            Message::RevealPackageFolder => self.reveal_package_folder(),
            Message::StartRenameOwned => self.start_rename_owned(),
            Message::RenameOwnedChanged(value) => {
                self.rename_buffer = Some(value);
                Update::none()
            }
            Message::CommitRenameOwned => self.commit_rename_owned(),
            Message::CancelRenameOwned => {
                self.rename_buffer = None;
                Update::none()
            }
            Message::RequestTrust => self.request_trust(),
            Message::CancelTrust => self.cancel_trust(),
            Message::SetTrusted(trusted) => self.set_trusted(trusted),
            Message::EditOwnedCapabilities => self.edit_owned_capabilities(),
            Message::SetLocalUnsandboxed(unsandboxed) => self.set_local_unsandboxed(unsandboxed),
            Message::GrantUpdate => self.grant_update(),
            Message::DismissUpdate => self.dismiss_update(),
            Message::RateInstalledPackage(stars) => self.rate_installed_package(stars),
            Message::InstalledRatingUpdated(result) => self.installed_rating_updated(result),

            // -------- discover ---------------------------------------------
            Message::OpenDiscover => self.open_discover(),
            Message::LoadFeaturedDiscover => self.load_featured_discover(),
            Message::FeaturedDiscoverLoaded(result) => {
                if let Ok(results) = result {
                    self.featured_packages = results;
                }
                Update::none()
            }
            Message::DiscoverQueryChanged(q) => {
                self.discover_query = q;
                Update::none()
            }
            Message::DiscoverSearch => self.discover_search(),
            Message::DiscoverScopeChanged(scope) => {
                // Scope is a radio; changing it re-runs the search immediately (no separate Search press).
                self.discover_scope = scope;
                self.discover_search()
            }
            Message::DiscoverResultsLoaded(result) => self.discover_results_loaded(result),
            Message::DiscoverSelect { package_id, owner } => {
                self.discover_select(package_id, owner)
            }
            Message::DiscoverInstallResult { owner, name } => {
                self.discover_install_result(owner, name)
            }
            Message::DiscoverDetailLoaded(result) => self.discover_detail_loaded(result),
            Message::DiscoverCommentsLoaded(result) => self.discover_comments_loaded(result),
            Message::DiscoverBack => self.discover_back(),
            Message::RatePackage(stars) => self.rate_package(stars),
            Message::RatingUpdated(result) => self.rating_updated(result),
            Message::CommentInputChanged(value) => {
                self.discover_comment_input = value;
                Update::none()
            }
            Message::AddComment => self.add_comment(),
            Message::CommentAdded(result) => self.comment_added(result),
            Message::OpenReadmeLink(uri) => {
                let _ = open::that(uri.as_str());
                Update::none()
            }
            Message::DiscoverInstall => self.discover_install(),
            Message::InstallResolved(seq, result) => self.install_resolved(seq, result),
            Message::ConsentGrant { enable } => self.consent_grant(enable),
            Message::ConsentCancel => self.consent_cancel(),
            Message::ParamValueEdit(target, key, edit) => self.param_value_edit(target, key, edit),
            Message::ParamPromptSubmit => self.param_prompt_submit(),
            Message::ParamPromptCancel => self.param_prompt_cancel(),
            Message::ParamConfigSave => self.param_config_save(),
            Message::ParamConfigClearSecret(key) => self.param_config_clear_secret(key),

            // -------- private & shared -------------------------------------
            Message::OpenShared => self.open_shared(),
            Message::SharedLoaded(result) => self.shared_loaded(result),
            Message::MyCloudLoaded(result) => self.my_cloud_loaded(result),
            Message::InstallShared { owner, name } => self.begin_install(owner, name),

            // -------- top action bar ---------------------------------------
            Message::Reload => {
                // Pick up a Settings change to the advanced-features gate without reopening.
                self.advanced_features =
                    smudgy_core::models::settings::load_settings().advanced_scripting_features;
                let toast = self.show_toast(format!("Reloaded scripts for {}.", self.server_name));
                Update::new(
                    Task::batch([
                        Task::done(self.load_scripts_message()),
                        Task::done(Message::LoadFolders),
                        Task::done(Message::LoadModules),
                        Task::done(Message::LoadLocalPackages),
                        Task::done(Message::LoadInstalledPackages),
                        Task::done(Message::LoadFeaturedDiscover),
                        toast,
                    ]),
                    Some(Event::ScriptsChanged {
                        server_name: self.server_name.clone(),
                    }),
                )
            }
            Message::Inspect => {
                match smudgy_core::session::registry::get_inspector_address(self.session_id) {
                    Some(addr) => {
                        crate::windows::smudgy_window::spawn_inspector(addr);
                        Update::none()
                    }
                    // The inspector port is opened at session-connect time, so a session
                    // that connected before advanced features were turned on has none yet.
                    // Surface it (a log line is invisible in a windowed build) and point at
                    // the fix: reconnect. The button itself is already gated on advanced
                    // features being on, so we don't repeat that here.
                    None => {
                        log::warn!(
                            "No script inspector for session {}: it is created at connect \
                             time; reconnect this session to start it.",
                            self.server_name
                        );
                        Update::with_task(
                            self.show_toast(crate::i18n::t!("automation-inspector-unavailable")),
                        )
                    }
                }
            }

            // -------- palette ----------------------------------------------
            Message::OpenPalette => {
                self.palette_open = true;
                self.palette_query.clear();
                self.palette_cursor = 0;
                self.new_menu_open = false;
                Update::with_task(self.focus_palette())
            }
            Message::ClosePalette => {
                self.palette_open = false;
                // Unconsumed Escape routes here even with no palette open, so
                // it also provides conventional, source-preserving dismissal
                // for the host-owned completion and hover overlays.
                if let Some(editor) = &mut self.code_editor {
                    editor.dismiss_overlays();
                }
                Update::none()
            }
            Message::PaletteInput(value) => {
                self.palette_query = value;
                self.palette_cursor = 0;
                Update::none()
            }
            Message::PaletteMove(delta) => self.palette_move(delta),
            Message::PaletteRun => self.palette_run_active(),
            Message::PaletteRunItem(index) => {
                self.palette_cursor = index;
                self.palette_run_active()
            }

            // -------- toast ------------------------------------------------
            Message::DismissToast(toast_id) => {
                if toast_id == self.toast_gen {
                    self.toast = None;
                }
                Update::none()
            }
            Message::PollLanguageService => {
                let (task, changed) = self.poll_language_service();
                if changed {
                    self.dirty = true;
                    if matches!(self.pane, Pane::Editor(_)) {
                        self.action_script_pinned = true;
                    }
                }
                Update::with_task(task)
            }
        };
        // An unpinned action draft follows the live matcher: any edit that can
        // change what is captured regenerates the example bodies.
        if refresh_generated {
            update.task = Task::batch([update.task, self.refresh_generated_actions()]);
        }
        update
    }

    // ---- guards ------------------------------------------------------------

    fn is_edit_message(message: &Message) -> bool {
        match message {
            Message::HotkeyTextAction(action)
            | Message::SendTextAction(action)
            | Message::AliasPatternAction(action)
            | Message::AliasRegexAction(action)
            | Message::RowSourceAction(_, action) => {
                matches!(action, text_editor::Action::Edit(_))
            }
            Message::SetName(_)
            | Message::SetNewModuleName(_)
            | Message::SetAliasKind(_)
            | Message::SetArgName(_, _)
            | Message::SetArgKind(_, _)
            | Message::AddArg
            | Message::RemoveArg(_)
            | Message::SetCmdMode(_)
            | Message::SetParseMode(_)
            | Message::ToggleAnchorStart
            | Message::ToggleAnchorEnd
            | Message::TogglePrompt
            | Message::SetBehavior(_)
            | Message::AdjustPriority(_)
            | Message::ToggleFallthrough
            | Message::ToggleAllowSelfMatch
            | Message::AddPattern
            | Message::AddExceptionRow
            | Message::AddRawRow
            | Message::SetTriggerCard(_)
            | Message::RemovePattern(_)
            | Message::MoveRowUp(_)
            | Message::MoveRowDown(_)
            | Message::SetRowSyntax(_, _)
            | Message::ToggleRowAnchorStart(_)
            | Message::ToggleRowAnchorEnd(_)
            | Message::ToggleRowColor(_, _)
            | Message::SelectRowColorKind(_, _)
            | Message::SetRowAnsiColor(_, _)
            | Message::SetRowXtermColor(_, _)
            | Message::SetRowColorRange(_, _, _)
            | Message::SetRowColorRangeHex(_, _, _)
            | Message::SetRowExactTruecolorHex(_, _)
            | Message::SetRowExactTruecolorRgb(_, _, _)
            | Message::ToggleRowColorAttribute(_, _, _)
            | Message::InsertReference(_)
            | Message::MarkHotkeyState(_) => true,
            _ => false,
        }
    }

    /// Whether a message can change what the open matcher captures — the
    /// signal to regenerate any unpinned action draft.
    fn affects_captures(message: &Message) -> bool {
        match message {
            Message::AliasPatternAction(action)
            | Message::AliasRegexAction(action)
            | Message::RowSourceAction(_, action) => {
                matches!(action, text_editor::Action::Edit(_))
            }
            Message::SetAliasKind(_)
            | Message::SetArgName(_, _)
            | Message::SetArgKind(_, _)
            | Message::AddArg
            | Message::RemoveArg(_)
            | Message::SetCmdMode(_)
            | Message::AddPattern
            | Message::AddExceptionRow
            | Message::AddRawRow
            | Message::SetTriggerCard(_)
            | Message::RemovePattern(_)
            | Message::MoveRowUp(_)
            | Message::MoveRowDown(_)
            | Message::SetRowSyntax(_, _) => true,
            _ => false,
        }
    }

    /// The action language of the open alias/trigger editor, if one is open.
    fn open_action_language(&self) -> Option<ScriptLang> {
        match &self.pane {
            Pane::Editor(EditorState { node, .. }) => match node {
                EditNode::Alias(alias) => Some(alias.language),
                EditNode::Trigger { language, .. } => Some(*language),
                EditNode::Hotkey(_) => None,
            },
            _ => None,
        }
    }

    fn is_guarded_navigation(message: &Message) -> bool {
        matches!(
            message,
            Message::SelectScript(_)
                | Message::SelectFolder(_)
                | Message::SelectModule(_)
                | Message::SelectOwnedPackage(_)
                | Message::SelectOwnedFile(_)
                | Message::NavigateCodeDefinition(_)
                | Message::SelectInstalledPackage(_)
                | Message::SelectDependency { .. }
                | Message::SelectCreatorAutomation { .. }
                | Message::ShowDashboard
                | Message::OpenDiscover
                | Message::OpenShared
                | Message::OpenStoreInspector
                | Message::NewAlias
                | Message::NewTrigger
                | Message::NewHotkey
                | Message::NewFolder
                | Message::NewModule
                | Message::NewPackage
        )
    }

    /// Commits the user's Discard choice before navigation consumes the current pane.
    /// Re-seeding the manifest matters for same-package definition jumps, which replace only
    /// the source editor and otherwise retain the structured manifest form in memory.
    fn accept_discarded_navigation(&mut self) {
        self.dirty = false;
        if self.manifest_dirty {
            let _ = self.revert_manifest();
        }
    }

    /// Swaps a trigger row with its neighbor **within its role group** — the
    /// phase order (exceptions, raw, matches) is fixed, so reordering never
    /// crosses roles. The row buffers move with the rows.
    fn move_trigger_row(&mut self, i: usize, down: bool) {
        if let Pane::Editor(EditorState {
            node: EditNode::Trigger { rows, .. },
            ..
        }) = &mut self.pane
            && i < rows.len()
        {
            let role = rows[i].role;
            let neighbor = if down {
                rows[i + 1..]
                    .iter()
                    .position(|row| row.role == role)
                    .map(|offset| i + 1 + offset)
            } else {
                rows[..i].iter().rposition(|row| row.role == role)
            };
            if let Some(j) = neighbor {
                rows.swap(i, j);
                if i < self.trigger_row_contents.len() && j < self.trigger_row_contents.len() {
                    self.trigger_row_contents.swap(i, j);
                }
            }
        }
    }

    fn trigger_row(&self, index: usize) -> Option<&model::TriggerRow> {
        let Pane::Editor(EditorState {
            node: EditNode::Trigger { rows, .. },
            ..
        }) = &self.pane
        else {
            return None;
        };
        rows.get(index)
    }

    fn set_row_matcher_color(
        &mut self,
        index: usize,
        color: smudgy_core::models::matchers::MatcherColor,
    ) {
        use smudgy_core::models::matchers::MatcherColorChannel;
        let Pane::Editor(EditorState {
            node: EditNode::Trigger { rows, .. },
            ..
        }) = &mut self.pane
        else {
            return;
        };
        let Some(row) = rows.get_mut(index) else {
            return;
        };
        let Some(filter) = row.color.as_mut() else {
            return;
        };
        match row.color_channel {
            MatcherColorChannel::Foreground => filter.foreground = Some(color),
            MatcherColorChannel::Background => filter.background = Some(color),
        }
    }

    /// Resets per-pane selection scaffolding before opening a new pane.
    pub(super) fn clear_selection(&mut self) {
        self.clear_code_editor();
        self.new_menu_open = false;
        self.confirm_folder_delete = false;
        self.confirm_delete_local = false;
        self.confirm_uninstall = false;
        self.confirm_trust = false;
        // Drop any open manifest draft + its unsaved/editing flags — leaving the owned-package pane
        // abandons the edit (re-seeded fresh from disk when an owned package is next opened). Also
        // keeps the unsaved-changes guard from later firing for a package that's no longer open.
        self.manifest_draft = None;
        self.manifest_dirty = false;
        self.manifest_editing = false;
        // Drop the inline param-value editor; the next package pane re-seeds it from its own params.
        self.param_config = None;
        // Abandon any in-flight install confirmation / update re-prompt on navigation — neither
        // has written anything yet (the consent window writes only on Grant). Bumping the
        // generation also discards a still-pending resolve so it can't pop a stale window later.
        self.consent_prompt = None;
        self.update_delta = None;
        self.install_seq.bump();
        // Drop any not-yet-shown required-params prompts queued after a multi-package install; their
        // packages are already installed (just left unconfigured), so navigating away is safe.
        self.param_prompt_queue.clear();
        // Opening any pane abandons the manage pane's in-flight detail load too — invalidate it so a
        // late result can't repaint or record consent against the package that was open before.
        self.detail_seq.bump();
    }

    /// The cloud package client (constructed per use).
    pub(super) fn package_client(&self) -> smudgy_cloud::package_api::PackageApiClient {
        smudgy_cloud::package_api::PackageApiClient::new(
            self.cloud.base_url.as_str(),
            self.cloud.credentials.clone(),
        )
    }

    pub(super) fn signed_in(&self) -> bool {
        self.cloud.snapshot.get().signed_in
    }
}

// ---- top-level view --------------------------------------------------------

use iced::widget::{column, container, row, scrollable, stack};
use iced::{Length, Padding};

impl AutomationsWindow {
    pub fn view(&self) -> Elem<'_> {
        let main = column![
            self.view_topbar(),
            self.view_nav_banner(),
            container(scrollable(self.view_pane()).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill),
        ]
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill);

        let base = container(
            row![self.view_sidebar(), main]
                .spacing(0)
                .height(Length::Fill),
        )
        .padding(Padding::ZERO)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|theme: &crate::theme::Theme| container::Style {
            background: Some(common::top_gradient(
                theme.styles.general.top_highlight,
                theme.styles.general.background,
            )),
            ..Default::default()
        });

        let mut layers: Vec<Elem<'_>> = vec![base.into()];

        if self.palette_open {
            layers.push(self.view_palette());
        }
        if let Some(message) = &self.toast {
            layers.push(common::toast(message));
        }

        stack(layers)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// The sticky unsaved-changes banner, shown while a navigation is deferred.
    fn view_nav_banner(&self) -> Elem<'_> {
        use iced::alignment::Vertical;
        use iced::widget::{button, text};
        if self.pending_nav.is_none() {
            return iced::widget::space::vertical()
                .height(Length::Fixed(0.0))
                .into();
        }
        container(
            row![
                text("\u{25CF}").size(10.0).style(common::danger),
                text(crate::i18n::t!("automation-nav-unsaved")).size(13.0),
                iced::widget::space::horizontal(),
                button(text(crate::i18n::t!("editor-discard")).size(13.0))
                    .style(crate::theme::builtins::button::secondary)
                    .on_press(Message::ConfirmDiscardNav),
                button(text(crate::i18n::t!("automation-keep-editing")).size(13.0))
                    .style(crate::theme::builtins::button::primary)
                    .on_press(Message::CancelDiscardNav),
            ]
            .spacing(10.0)
            .align_y(Vertical::Center),
        )
        .width(Length::Fill)
        .padding(Padding {
            top: 8.0,
            bottom: 8.0,
            left: 18.0,
            right: 18.0,
        })
        .style(common::banner_style)
        .into()
    }

    /// Dispatches to the active content pane.
    fn view_pane(&self) -> Elem<'_> {
        match &self.pane {
            Pane::Dashboard => self.view_dashboard(),
            Pane::Error(errors) => self.view_error(errors),
            Pane::Editor(state) => self.view_editor(state),
            Pane::Folder(state) => self.view_folder_editor(state),
            Pane::Module(state) => self.view_module(state),
            Pane::OwnedPackage => self.view_owned_package(),
            Pane::NewPackage { name, error } => self.view_new_package(name, error.as_deref()),
            Pane::InstalledPackage => self.view_installed_package(),
            Pane::CreatorAutomation {
                creator_id,
                kind,
                name,
            } => self.view_creator_automation(creator_id, *kind, name),
            Pane::Discover => self.view_discover(),
            Pane::Shared => self.view_shared(),
            Pane::StoreInspector => self.view_store_inspector(),
        }
    }

    fn view_error(&self, errors: &[String]) -> Elem<'_> {
        use iced::widget::text;
        let mut col = column![].spacing(8).padding(28);
        for err in errors {
            col = col.push(text(err.clone()).size(13.0).style(common::danger));
        }
        col.width(Length::Fill).into()
    }
}

#[cfg(test)]
mod tab_traversal_tests {
    use super::*;

    fn window_with_foreground(
        color: smudgy_core::models::matchers::MatcherColor,
    ) -> AutomationsWindow {
        let mut window = AutomationsWindow::new(
            window::Id::unique(),
            "truecolor-editor-test".to_string(),
            crate::cloud_account::test_handles(),
            SessionId::from(1),
        );
        window.pane = Pane::Editor(EditorState {
            mode: EditorMode::Create,
            original_name: None,
            name: "color".to_string(),
            node: EditNode::Trigger {
                enabled: true,
                language: ScriptLang::Plaintext,
                prompt: false,
                priority: 0,
                fallthrough: false,
                package: None,
                rows: vec![model::TriggerRow {
                    color: Some(smudgy_core::models::matchers::MatcherColorMatch {
                        foreground: Some(color),
                        ..Default::default()
                    }),
                    ..model::TriggerRow::new(PatternKind::Match)
                }],
            },
            error: None,
        });
        window
    }

    fn first_row(window: &AutomationsWindow) -> &model::TriggerRow {
        let Pane::Editor(EditorState {
            node: EditNode::Trigger { rows, .. },
            ..
        }) = &window.pane
        else {
            panic!("test window must contain a trigger editor");
        };
        &rows[0]
    }

    fn foreground(window: &AutomationsWindow) -> smudgy_core::models::matchers::MatcherColor {
        first_row(window)
            .color
            .as_ref()
            .and_then(|filter| filter.foreground)
            .expect("test row must have a foreground filter")
    }

    fn background(window: &AutomationsWindow) -> smudgy_core::models::matchers::MatcherColor {
        first_row(window)
            .color
            .as_ref()
            .and_then(|filter| filter.background)
            .expect("test row must have a background filter")
    }

    #[test]
    fn publish_completion_updates_the_open_pane_immediately() {
        let mut window = AutomationsWindow::new(
            window::Id::unique(),
            "publish-completion-test".to_string(),
            crate::cloud_account::test_handles(),
            SessionId::from(1),
        );
        window.selection = Selection::OwnedPackage("demo".to_string());
        window.authoring_busy = true;
        let package_id = Uuid::new_v4();
        let published_at = "2026-08-10T00:00:00Z".parse().unwrap();

        let _ = window.update(Message::PublishFinished {
            name: "demo".to_string(),
            result: Ok(PublishSummary {
                package_id,
                is_public: true,
                version: "1.2.3".to_string(),
                published_at,
                typings_generated: 1,
                typings_warnings: Vec::new(),
                locked_dependencies: Vec::new(),
                dependency_warnings: Vec::new(),
                interop_warnings: Vec::new(),
            }),
        });

        assert!(!window.authoring_busy);
        assert_eq!(window.share_package_id, Some(package_id));
        assert!(window.share_is_public);
        assert_eq!(window.share_versions.len(), 1);
        assert_eq!(window.share_versions[0].version, "1.2.3");
        assert_eq!(window.share_versions[0].published_at, published_at);
        let output = window.publish_output.as_ref().unwrap();
        assert_eq!(output.package, "demo");
        assert!(output.text.starts_with("smudgy> publish demo\n"));
    }

    #[test]
    fn plain_and_shift_tab_choose_forward_and_backward_traversal() {
        assert_eq!(
            tab_traversal(keyboard::Modifiers::empty(), Status::Ignored),
            Some(false)
        );
        assert_eq!(
            tab_traversal(keyboard::Modifiers::SHIFT, Status::Ignored),
            Some(true)
        );
    }

    #[test]
    fn captured_or_shortcut_modified_tabs_do_not_traverse() {
        assert_eq!(
            tab_traversal(keyboard::Modifiers::empty(), Status::Captured),
            None
        );
        for modifier in [
            keyboard::Modifiers::CTRL,
            keyboard::Modifiers::ALT,
            keyboard::Modifiers::LOGO,
        ] {
            assert_eq!(tab_traversal(modifier, Status::Ignored), None);
        }
    }

    #[test]
    fn command_space_requests_completion_only_when_unconsumed() {
        let space = keyboard::Key::Character(" ".into());
        assert!(code_completion_shortcut(
            &space,
            keyboard::Modifiers::CTRL,
            Status::Ignored
        ));
        assert!(!code_completion_shortcut(
            &space,
            keyboard::Modifiers::CTRL,
            Status::Captured
        ));
        assert!(!code_completion_shortcut(
            &space,
            keyboard::Modifiers::empty(),
            Status::Ignored
        ));
    }

    #[test]
    fn escape_route_closes_transient_ui_without_editing_code() {
        let mut window = AutomationsWindow::new(
            window::Id::unique(),
            "escape-code-overlay-test".to_owned(),
            crate::cloud_account::test_handles(),
            SessionId::from(1),
        );
        let _ = window.bind_code_editor(
            "const value = 1;",
            smudgy_script::language_service::Language::TypeScript,
            code_editor::CodeDocument::StandaloneModule,
        );
        window.palette_open = true;

        let _ = window.update(Message::ClosePalette);

        assert!(!window.palette_open);
        assert_eq!(window.code_editor_text(), "const value = 1;");
        assert!(!window.dirty);
    }

    #[test]
    fn hotkey_preserves_its_single_body_across_language_changes() {
        let mut window = AutomationsWindow::new(
            window::Id::unique(),
            "hotkey-code-transition-test".to_string(),
            crate::cloud_account::test_handles(),
            SessionId::from(1),
        );
        let _ = window.new_hotkey();
        window.hotkey_text_content = text_editor::Content::with_text("say hello");

        let _ = window.update(Message::SetBehavior(ScriptLang::JS));

        assert!(window.code_editor.is_some());
        assert!(window.language_service.is_some());
        assert_eq!(window.code_editor_text(), "say hello");
        let message = window
            .bind_code_editor_message(code_editor::IcedEditorMessage::CtrlEnd)
            .expect("code editor is bound");
        let _ = window.update(Message::CodeEditorAction(message));
        let message = window
            .bind_code_editor_message(code_editor::IcedEditorMessage::Paste("();".to_owned()))
            .expect("code editor is bound");
        let _ = window.update(Message::CodeEditorAction(message));
        let _ = window.update(Message::SetBehavior(ScriptLang::Plaintext));

        assert_eq!(window.hotkey_text_content.text(), "say hello();");
        assert!(window.code_editor.is_none());
        assert!(matches!(
            &window.pane,
            Pane::Editor(EditorState {
                node: EditNode::Hotkey(hotkeys::HotkeyDefinition {
                    language: ScriptLang::Plaintext,
                    ..
                }),
                ..
            })
        ));

        let _ = window.update(Message::SetBehavior(ScriptLang::TS));
        assert_eq!(window.code_editor_text(), "say hello();");
    }

    #[test]
    fn non_script_writable_files_do_not_start_the_language_service() {
        for language in [
            smudgy_script::language_service::Language::PlainText,
            smudgy_script::language_service::Language::Json,
        ] {
            let mut window = AutomationsWindow::new(
                window::Id::unique(),
                "non-script-editor-test".to_string(),
                crate::cloud_account::test_handles(),
                SessionId::from(1),
            );

            let _ = window.bind_code_editor(
                "notes only",
                language,
                code_editor::CodeDocument::OwnedPackage,
            );

            assert!(window.code_editor.is_some());
            assert!(window.language_service.is_none());
            assert_eq!(window.code_editor_text(), "notes only");
        }
    }

    #[test]
    fn new_module_rebinds_language_from_its_name_without_losing_text() {
        let mut window = AutomationsWindow::new(
            window::Id::unique(),
            "new-module-language-test".to_string(),
            crate::cloud_account::test_handles(),
            SessionId::from(1),
        );
        let _ = window.new_module();
        let original = window.code_editor_text();

        let _ = window.update(Message::SetNewModuleName("helpers.js".to_owned()));
        assert_eq!(
            window.code_editor.as_ref().unwrap().document().language,
            smudgy_script::language_service::Language::JavaScript
        );
        assert_eq!(window.code_editor_text(), original);

        let _ = window.update(Message::SetNewModuleName("data.json".to_owned()));
        let editor = window.code_editor.as_ref().unwrap();
        assert_eq!(
            editor.document().language,
            smudgy_script::language_service::Language::Json
        );
        assert!(!editor.has_language_service());
        assert_eq!(window.code_editor_text(), original);
        assert!(window.dirty);
    }

    #[test]
    fn stale_async_editor_message_cannot_mutate_a_remounted_stable_document() {
        let mut window = AutomationsWindow::new(
            window::Id::unique(),
            "stale-editor-message-test".to_string(),
            crate::cloud_account::test_handles(),
            SessionId::from(1),
        );
        window.pane = Pane::Module(ModuleState {
            mode: ModuleMode::View,
            subpath: "same.ts".to_owned(),
            path: Some(std::path::PathBuf::from("same.ts")),
            name: String::new(),
            error: None,
        });
        let _ = window.bind_code_editor(
            "first",
            smudgy_script::language_service::Language::TypeScript,
            code_editor::CodeDocument::StandaloneModule,
        );
        let first_document = window
            .code_editor
            .as_ref()
            .unwrap()
            .document()
            .document
            .key
            .document_id;
        let stale = window
            .bind_code_editor_message(code_editor::IcedEditorMessage::Paste(" stale".to_owned()))
            .unwrap();
        let _ = window.bind_code_editor(
            "second",
            smudgy_script::language_service::Language::TypeScript,
            code_editor::CodeDocument::StandaloneModule,
        );
        assert_eq!(
            window
                .code_editor
                .as_ref()
                .unwrap()
                .document()
                .document
                .key
                .document_id,
            first_document,
            "saved paths deliberately reuse their stable routing identity"
        );
        assert_ne!(
            stale.mount_generation, window.code_editor_mount_generation,
            "each surface binding needs a distinct asynchronous-task fence"
        );

        let _ = window.update(Message::CodeEditorAction(stale));

        assert_eq!(window.code_editor_text(), "second");
        assert!(!window.dirty);
    }

    #[test]
    fn module_reload_rebinds_clean_text_and_preserves_a_dirty_overlay() {
        let directory = tempfile::tempdir().expect("temporary module directory");
        let path = directory.path().join("same.ts");
        std::fs::write(&path, "export const value = 1;\n").expect("seed module");
        let mut window = AutomationsWindow::new(
            window::Id::unique(),
            "module-reload-overlay-test".to_string(),
            crate::cloud_account::test_handles(),
            SessionId::from(1),
        );
        window.modules = vec![smudgy_core::models::modules::ModuleFile {
            subpath: "same.ts".to_owned(),
            path: path.clone(),
        }];
        window.selection = Selection::Module("same.ts".to_owned());
        window.pane = Pane::Module(ModuleState {
            mode: ModuleMode::View,
            subpath: "same.ts".to_owned(),
            path: Some(path.clone()),
            name: String::new(),
            error: None,
        });
        let _ = window.bind_code_editor(
            "export const value = 1;\n",
            smudgy_script::language_service::Language::TypeScript,
            code_editor::CodeDocument::StandaloneModule,
        );
        let stable_id = window
            .code_editor
            .as_ref()
            .unwrap()
            .document()
            .document
            .key
            .document_id;

        std::fs::write(&path, "export const value = 2;\n").expect("update clean module");
        let _ = window.reconcile_module_language_project_reload();
        assert_eq!(window.code_editor_text(), "export const value = 2;\n");
        assert_eq!(
            window
                .code_editor
                .as_ref()
                .unwrap()
                .document()
                .document
                .key
                .document_id,
            stable_id
        );

        let end = window
            .bind_code_editor_message(code_editor::IcedEditorMessage::CtrlEnd)
            .unwrap();
        let _ = window.update(Message::CodeEditorAction(end));
        let paste = window
            .bind_code_editor_message(code_editor::IcedEditorMessage::Paste(
                "// unsaved\n".to_owned(),
            ))
            .unwrap();
        let _ = window.update(Message::CodeEditorAction(paste));
        let dirty_text = window.code_editor_text();
        assert!(window.dirty);

        std::fs::write(&path, "export const value = 3;\n").expect("update disk beneath overlay");
        let _ = window.reconcile_module_language_project_reload();
        assert_eq!(window.code_editor_text(), dirty_text);
        assert!(window.dirty);
    }

    #[test]
    fn every_sidebar_document_route_is_guarded_while_source_is_dirty() {
        let messages = [
            Message::SelectDependency {
                parent: "parent".to_owned(),
                spec: "smudgy://owner/package".to_owned(),
            },
            Message::SelectCreatorAutomation {
                creator_id: "creator".to_owned(),
                kind: AutomationKind::Alias,
                name: "generated".to_owned(),
            },
        ];
        for message in messages {
            let mut window = AutomationsWindow::new(
                window::Id::unique(),
                "guarded-sidebar-route-test".to_string(),
                crate::cloud_account::test_handles(),
                SessionId::from(1),
            );
            window.dirty = true;

            let _ = window.update(message);

            assert!(window.pending_nav.is_some());
            assert!(window.dirty);
        }
    }

    #[test]
    fn selecting_another_owned_file_is_guarded_while_source_is_dirty() {
        let mut window = AutomationsWindow::new(
            window::Id::unique(),
            "owned-file-navigation-test".to_string(),
            crate::cloud_account::test_handles(),
            SessionId::from(1),
        );
        window.selection = Selection::OwnedPackage("demo".to_owned());
        window.pane = Pane::OwnedPackage;
        window.owned_selected_file = Some("first.ts".to_owned());
        window.dirty = true;

        let _ = window.update(Message::SelectOwnedFile("second.ts".to_owned()));

        assert_eq!(window.owned_selected_file.as_deref(), Some("first.ts"));
        assert!(matches!(
            window.pending_nav.as_deref(),
            Some(Message::SelectOwnedFile(path)) if path == "second.ts"
        ));
    }

    #[test]
    fn color_focus_and_channel_navigation_do_not_mark_the_editor_dirty() {
        use smudgy_core::models::matchers::{MatcherColor, MatcherColorChannel};

        let mut window = window_with_foreground(MatcherColor::Ansi { index: 7 });
        assert!(!window.dirty);

        let _ = window.update(Message::FocusColorControl(iced::widget::Id::from(
            "automation-trigger-color-row-0-channel".to_string(),
        )));
        assert!(!window.dirty);

        let _ = window.update(Message::SelectRowColorChannel(
            0,
            MatcherColorChannel::Background,
        ));
        assert!(!window.dirty);

        let _ = window.update(Message::SetRowAnsiColor(0, 3));
        assert!(window.dirty);
    }

    #[test]
    fn exact_truecolor_inputs_synchronize_only_after_valid_edits() {
        use model::{MatcherColorKind, TruecolorComponent};
        use smudgy_core::models::matchers::MatcherColor;

        let mut window = window_with_foreground(MatcherColor::Ansi { index: 7 });
        assert_eq!(model::parse_matcher_hex("aéabc"), None);
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Truecolor));
        assert_eq!(
            foreground(&window),
            MatcherColor::Truecolor {
                r: 255,
                g: 255,
                b: 255,
                range: None,
            }
        );

        let _ = window.update(Message::SetRowExactTruecolorHex(0, "#0a80ff".to_string()));
        assert_eq!(
            first_row(&window)
                .color_draft(smudgy_core::models::matchers::MatcherColorChannel::Foreground)
                .exact_truecolor
                .rgb,
            ["10", "128", "255"]
        );
        assert_eq!(
            foreground(&window),
            MatcherColor::Truecolor {
                r: 10,
                g: 128,
                b: 255,
                range: None,
            }
        );

        let valid_color = foreground(&window);
        let _ = window.update(Message::SetRowExactTruecolorHex(0, "#0a80f".to_string()));
        assert_eq!(
            first_row(&window)
                .color_draft(smudgy_core::models::matchers::MatcherColorChannel::Foreground)
                .exact_truecolor
                .hex,
            "#0a80f"
        );
        assert_eq!(foreground(&window), valid_color);

        let _ = window.update(Message::SetRowExactTruecolorRgb(
            0,
            TruecolorComponent::Green,
            "300".to_string(),
        ));
        let _ = window.update(Message::SetRowExactTruecolorRgb(
            0,
            TruecolorComponent::Red,
            "17".to_string(),
        ));
        assert_eq!(foreground(&window), valid_color);
        assert_eq!(
            first_row(&window)
                .color_draft(smudgy_core::models::matchers::MatcherColorChannel::Foreground)
                .exact_truecolor
                .rgb,
            ["17", "300", "255"]
        );

        let _ = window.update(Message::SetRowExactTruecolorRgb(
            0,
            TruecolorComponent::Green,
            "42".to_string(),
        ));
        assert_eq!(
            first_row(&window)
                .color_draft(smudgy_core::models::matchers::MatcherColorChannel::Foreground)
                .exact_truecolor
                .hex,
            "#112aff"
        );
        assert_eq!(
            foreground(&window),
            MatcherColor::Truecolor {
                r: 17,
                g: 42,
                b: 255,
                range: None,
            }
        );
    }

    #[test]
    fn channel_switches_preserve_partial_color_drafts() {
        use model::{ColorRangeEndpoint, MatcherColorKind};
        use smudgy_core::models::matchers::{MatcherColor, MatcherColorChannel};

        let mut window = window_with_foreground(MatcherColor::Ansi { index: 7 });
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Truecolor));
        let _ = window.update(Message::SetRowExactTruecolorHex(0, "#12".to_string()));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Ansi));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Truecolor));

        let _ = window.update(Message::SelectRowColorChannel(
            0,
            MatcherColorChannel::Background,
        ));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::First,
            "#34".to_string(),
        ));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Ansi));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));

        let _ = window.update(Message::SelectRowColorChannel(
            0,
            MatcherColorChannel::Foreground,
        ));
        assert_eq!(
            first_row(&window)
                .color_draft(MatcherColorChannel::Foreground)
                .exact_truecolor
                .hex,
            "#12"
        );
        let _ = window.update(Message::SelectRowColorChannel(
            0,
            MatcherColorChannel::Background,
        ));
        assert_eq!(
            first_row(&window)
                .color_draft(MatcherColorChannel::Background)
                .color_range_hex[0],
            "#34"
        );
    }

    #[test]
    fn color_toggle_message_restores_foreground_background_and_attributes() {
        use smudgy_core::models::matchers::{
            MatcherColor, MatcherColorMatch, MatcherTextAttribute,
        };

        let mut window = window_with_foreground(MatcherColor::Ansi { index: 7 });
        let filter = MatcherColorMatch {
            foreground: Some(MatcherColor::Ansi { index: 2 }),
            background: Some(MatcherColor::Xterm { index: 196 }),
            attributes: vec![MatcherTextAttribute::Bold, MatcherTextAttribute::Italic],
        };
        let Pane::Editor(EditorState {
            node: EditNode::Trigger { rows, .. },
            ..
        }) = &mut window.pane
        else {
            panic!("test window must contain a trigger editor");
        };
        rows[0].color = Some(filter.clone());

        let _ = window.update(Message::ToggleRowColor(0, false));
        assert!(first_row(&window).color.is_none());

        let _ = window.update(Message::ToggleRowColor(0, true));
        assert_eq!(first_row(&window).color.as_ref(), Some(&filter));
    }

    #[test]
    fn color_kind_tabs_restore_each_channel_dormant_values() {
        use model::{ColorRangeEndpoint, MatcherColorKind};
        use smudgy_core::models::matchers::{MatcherColor, MatcherColorChannel, MatcherHsv};

        let hex = |hsv: MatcherHsv| {
            let (r, g, b) = hsv.to_rgb();
            format!("#{r:02x}{g:02x}{b:02x}")
        };
        let vivid = |hue| MatcherHsv {
            hue,
            saturation: 255,
            value: 255,
        };
        let mut window = window_with_foreground(MatcherColor::Ansi { index: 7 });

        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Truecolor));
        let _ = window.update(Message::SetRowExactTruecolorHex(0, "#0a80ff".to_string()));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::First,
            hex(vivid(350)),
        ));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::Second,
            hex(vivid(10)),
        ));
        let foreground_range = matcher_truecolor_range(foreground(&window)).unwrap();
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Ansi));

        let _ = window.update(Message::SelectRowColorChannel(
            0,
            MatcherColorChannel::Background,
        ));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Truecolor));
        let _ = window.update(Message::SetRowExactTruecolorHex(0, "#112233".to_string()));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::First,
            hex(vivid(120)),
        ));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::Second,
            hex(vivid(240)),
        ));
        let background_range = matcher_truecolor_range(background(&window)).unwrap();
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Ansi));

        let _ = window.update(Message::SelectRowColorChannel(
            0,
            MatcherColorChannel::Foreground,
        ));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Truecolor));
        assert_eq!(
            foreground(&window),
            MatcherColor::Truecolor {
                r: 10,
                g: 128,
                b: 255,
                range: None,
            }
        );
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));
        assert_eq!(
            matcher_truecolor_range(foreground(&window)),
            Some(foreground_range)
        );

        let _ = window.update(Message::SelectRowColorChannel(
            0,
            MatcherColorChannel::Background,
        ));
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::Truecolor));
        assert_eq!(
            background(&window),
            MatcherColor::Truecolor {
                r: 17,
                g: 34,
                b: 51,
                range: None,
            }
        );
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));
        assert_eq!(
            matcher_truecolor_range(background(&window)),
            Some(background_range)
        );
    }

    #[test]
    fn invalid_color_text_blocks_save_and_remains_dirty() {
        use model::{ColorRangeEndpoint, MatcherColorKind};
        use smudgy_core::models::matchers::MatcherColor;

        let mut exact = window_with_foreground(MatcherColor::Ansi { index: 7 });
        let _ = exact.update(Message::SelectRowColorKind(0, MatcherColorKind::Truecolor));
        let _ = exact.update(Message::SetRowExactTruecolorHex(0, "#123".to_string()));
        let _ = exact.update(Message::Save);
        assert!(exact.dirty);
        let Pane::Editor(state) = &exact.pane else {
            panic!("save must keep the editor open");
        };
        assert!(state.error.is_some());
        assert_eq!(
            first_row(&exact)
                .color_draft(smudgy_core::models::matchers::MatcherColorChannel::Foreground,)
                .exact_truecolor
                .hex,
            "#123"
        );

        let mut range = window_with_foreground(MatcherColor::Ansi { index: 7 });
        let _ = range.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));
        let _ = range.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::Second,
            "#abcd".to_string(),
        ));
        let _ = range.update(Message::Save);
        assert!(range.dirty);
        let Pane::Editor(state) = &range.pane else {
            panic!("save must keep the editor open");
        };
        assert!(state.error.is_some());
        assert_eq!(
            first_row(&range)
                .color_draft(smudgy_core::models::matchers::MatcherColorChannel::Foreground,)
                .color_range_hex[1],
            "#abcd"
        );
    }

    #[test]
    fn color_range_derives_the_directed_hue_interval() {
        use model::{ColorRangeEndpoint, MatcherColorKind};
        use smudgy_core::models::matchers::{MatcherColor, MatcherHsv};

        let mut window = window_with_foreground(MatcherColor::Ansi { index: 7 });
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));
        let hex = |hsv: MatcherHsv| {
            let (r, g, b) = hsv.to_rgb();
            format!("#{r:02x}{g:02x}{b:02x}")
        };
        let vivid = |hue| MatcherHsv {
            hue,
            saturation: 255,
            value: 255,
        };

        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::First,
            hex(vivid(350)),
        ));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::Second,
            hex(vivid(10)),
        ));
        let narrow = matcher_truecolor_range(foreground(&window)).unwrap();
        assert_eq!(narrow.directed_endpoints(), (vivid(350), vivid(10)));
        assert!(narrow.wrap_hue);

        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::First,
            hex(vivid(10)),
        ));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::Second,
            hex(vivid(350)),
        ));
        let broad = matcher_truecolor_range(foreground(&window)).unwrap();
        assert_eq!(broad.directed_endpoints(), (vivid(10), vivid(350)));
        assert!(!broad.wrap_hue);
    }

    #[test]
    fn achromatic_range_hex_preserves_each_endpoint_hue() {
        use model::{ColorRangeEndpoint, MatcherColorKind};
        use smudgy_core::models::matchers::{MatcherColor, MatcherHsv};

        let mut window = window_with_foreground(MatcherColor::Ansi { index: 7 });
        let _ = window.update(Message::SelectRowColorKind(0, MatcherColorKind::ColorRange));
        let hex = |hsv: MatcherHsv| {
            let (r, g, b) = hsv.to_rgb();
            format!("#{r:02x}{g:02x}{b:02x}")
        };
        let vivid = |hue| MatcherHsv {
            hue,
            saturation: 255,
            value: 255,
        };
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::First,
            hex(vivid(350)),
        ));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::Second,
            hex(vivid(10)),
        ));

        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::First,
            "#808080".to_string(),
        ));
        let _ = window.update(Message::SetRowColorRangeHex(
            0,
            ColorRangeEndpoint::Second,
            "#ffffff".to_string(),
        ));

        let range = matcher_truecolor_range(foreground(&window)).unwrap();
        let (from, to) = range.directed_endpoints();
        assert_eq!(
            from,
            MatcherHsv {
                hue: 350,
                saturation: 0,
                value: 128,
            }
        );
        assert_eq!(
            to,
            MatcherHsv {
                hue: 10,
                saturation: 0,
                value: 255,
            }
        );
        assert!(range.wrap_hue);
        assert_eq!(
            first_row(&window)
                .color_draft(smudgy_core::models::matchers::MatcherColorChannel::Foreground)
                .color_range_last_valid,
            range
        );
    }
}
