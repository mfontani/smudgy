//! The script editors (alias / trigger / hotkey), the folder editor, and the
//! module pane — both the update-side logic and the views.

use std::collections::BTreeMap;
use std::sync::Arc;

use iced::alignment::Vertical;
use iced::widget::{
    Column, Space, button, checkbox, column, container, pick_list, radio, row, text, text_editor,
    text_input,
};
use iced::{Element, Font, Length, Padding};

use smudgy_core::models::matchers::{
    self, ArgKind, CmdMode, CommandOutcome, CommandSpec, MatcherSyntax,
};
use smudgy_core::models::server;
use smudgy_core::models::{ScriptLang, aliases, hotkeys, naming, packages, triggers};

use crate::assets::{bootstrap_icons, fonts};
use crate::keymap::{self as hotkey_helpers, MaybePhysicalKey};
use crate::theme::Theme;
use crate::theme::builtins::button as button_style;
use crate::update::Update;
use crate::widgets::hotkey_input::HotkeyInput;

use super::common;
use super::highlight;
use super::model::{
    AliasKind, AliasMatcherDraft, ArgKindChoice, NodeStatus, ParseModeChoice, PatternKind, Script,
    ScriptKey, SyntaxChoice, TriggerRow, pattern_error_text, rows_into_trigger, trigger_rows,
    upsert_script_folder,
};
use super::{
    AutomationsWindow, EditNode, EditorMode, EditorState, Elem, Event, FolderState, Message,
    ModuleMode, ModuleState, Pane, Selection,
};

const LABEL_WIDTH: f32 = 92.0;

/// A destination choice for the editor's folder picker: top level, or a folder
/// path. Wraps `Option<String>` so it satisfies the `Clone + Display + PartialEq`
/// `pick_list` requires, with `None`/top level rendered as a friendly sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FolderChoice {
    TopLevel,
    Folder(String),
}

impl FolderChoice {
    fn from_package(package: Option<&str>) -> Self {
        match package {
            Some(path) if !path.is_empty() => FolderChoice::Folder(path.to_string()),
            _ => FolderChoice::TopLevel,
        }
    }

    fn into_package(self) -> Option<String> {
        match self {
            FolderChoice::TopLevel => None,
            FolderChoice::Folder(path) => Some(path),
        }
    }
}

impl std::fmt::Display for FolderChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FolderChoice::TopLevel => f.write_str(&crate::i18n::t!("editor-top-level")),
            FolderChoice::Folder(path) => f.write_str(path),
        }
    }
}

/// Logs `msg` and returns an empty update (used for non-fatal save failures).
fn warn_none(msg: String) -> Update<Message, Event> {
    log::warn!("{msg}");
    Update::none()
}

// ============================================================================
// Update-side: open / create / save / delete
// ============================================================================

impl AutomationsWindow {
    pub(super) fn open_script(&mut self, key: ScriptKey) -> Update<Message, Event> {
        let Some(script) = self.find_script(&key) else {
            return Update::none();
        };
        self.clear_selection();
        self.selection = Selection::Script(key.clone());
        self.test_input.clear();
        self.order_revealed = false;
        self.try_it_open = false;

        let body = match &script {
            Script::Alias(a) => a.script.clone().unwrap_or_default(),
            Script::Hotkey(h) => h.script.clone().unwrap_or_default(),
            Script::Trigger(t) => t.script.clone().unwrap_or_default(),
            Script::Folder(_, _) => return Update::none(),
        };
        self.editor_content = text_editor::Content::with_text(&body);

        let node = match script {
            Script::Alias(a) => {
                self.alias_draft = AliasMatcherDraft::from_definition(&a);
                if self.alias_draft.degraded {
                    log::info!(
                        "alias {}: stored pattern no longer matches its sidecar; showing as regex",
                        key.script_name
                    );
                }
                self.alias_pattern_content =
                    text_editor::Content::with_text(&self.alias_draft.pattern_source);
                self.alias_regex_content =
                    text_editor::Content::with_text(&self.alias_draft.regex_source);
                EditNode::Alias(a)
            }
            Script::Hotkey(h) => {
                self.hotkey_state = hotkey_definition_to_keys(&h);
                EditNode::Hotkey(h)
            }
            Script::Trigger(t) => {
                let rows = trigger_rows(&t);
                self.trigger_row_contents = rows
                    .iter()
                    .map(|row| text_editor::Content::with_text(&row.source))
                    .collect();
                EditNode::Trigger {
                    enabled: t.enabled,
                    language: t.language,
                    prompt: t.prompt,
                    priority: t.priority,
                    fallthrough: t.fallthrough,
                    package: t.package.clone(),
                    rows,
                }
            }
            Script::Folder(_, _) => return Update::none(),
        };
        self.pane = Pane::Editor(EditorState {
            mode: EditorMode::Edit,
            original_name: Some(key.script_name.clone()),
            name: key.script_name,
            node,
            error: None,
        });
        Update::none()
    }

    pub(super) fn new_alias(&mut self) -> Update<Message, Event> {
        self.clear_selection();
        self.selection = Selection::None;
        self.editor_content = text_editor::Content::new();
        self.test_input.clear();
        self.order_revealed = false;
        self.try_it_open = false;
        // Command is the default kind for new aliases.
        self.alias_draft = AliasMatcherDraft::default();
        self.alias_pattern_content = text_editor::Content::new();
        self.alias_regex_content = text_editor::Content::new();
        self.pane = Pane::Editor(EditorState {
            mode: EditorMode::Create,
            original_name: None,
            name: String::new(),
            node: EditNode::Alias(aliases::AliasDefinition {
                pattern: String::new(),
                script: None,
                package: self.current_folder(),
                enabled: true,
                priority: 0,
                fallthrough: true,
                language: ScriptLang::Plaintext,
                matcher: None,
            }),
            error: None,
        });
        Update::none()
    }

    pub(super) fn new_trigger(&mut self) -> Update<Message, Event> {
        self.clear_selection();
        self.selection = Selection::None;
        self.editor_content = text_editor::Content::new();
        self.test_input.clear();
        self.order_revealed = false;
        self.try_it_open = false;
        self.trigger_row_contents = vec![text_editor::Content::new()];
        self.pane = Pane::Editor(EditorState {
            mode: EditorMode::Create,
            original_name: None,
            name: String::new(),
            node: EditNode::Trigger {
                enabled: true,
                language: ScriptLang::Plaintext,
                prompt: false,
                priority: 0,
                fallthrough: true,
                package: self.current_folder(),
                rows: vec![TriggerRow::new(PatternKind::Match)],
            },
            error: None,
        });
        Update::none()
    }

    pub(super) fn new_hotkey(&mut self) -> Update<Message, Event> {
        self.clear_selection();
        self.selection = Selection::None;
        self.editor_content = text_editor::Content::new();
        self.hotkey_state.clear();
        self.pane = Pane::Editor(EditorState {
            mode: EditorMode::Create,
            original_name: None,
            name: String::new(),
            node: EditNode::Hotkey(hotkeys::HotkeyDefinition {
                key: String::new(),
                modifiers: vec![],
                script: None,
                package: self.current_folder(),
                language: ScriptLang::Plaintext,
                enabled: true,
            }),
            error: None,
        });
        Update::none()
    }

    pub(super) fn new_folder(&mut self) -> Update<Message, Event> {
        self.clear_selection();
        self.pane = Pane::Folder(FolderState {
            mode: EditorMode::Create,
            original_path: None,
            path: self
                .current_folder()
                .map(|p| format!("{p}/"))
                .unwrap_or_default(),
            enabled: true,
            error: None,
        });
        Update::none()
    }

    pub(super) fn new_module(&mut self) -> Update<Message, Event> {
        self.clear_selection();
        self.selection = Selection::None;
        self.editor_content = text_editor::Content::with_text(
            "// A local module: shared helpers, private to this profile.\n",
        );
        self.pane = Pane::Module(ModuleState {
            mode: ModuleMode::Create,
            subpath: String::new(),
            path: None,
            name: String::new(),
            error: None,
        });
        Update::none()
    }

    pub(super) fn open_folder(&mut self, path: String) -> Update<Message, Event> {
        self.clear_selection();
        let enabled = packages::folder_enabled(&self.packages, &path);
        self.selection = Selection::Folder(path.clone());
        self.pane = Pane::Folder(FolderState {
            mode: EditorMode::Edit,
            original_path: Some(path.clone()),
            path,
            enabled,
            error: None,
        });
        Update::none()
    }

    pub(super) fn open_module(&mut self, subpath: String) -> Update<Message, Event> {
        self.clear_selection();
        let path = self
            .modules
            .iter()
            .find(|m| m.subpath == subpath)
            .map(|m| m.path.clone());
        self.selection = Selection::Module(subpath.clone());
        if let Some(path) = path {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    self.editor_content = text_editor::Content::with_text(&content);
                    self.pane = Pane::Module(ModuleState {
                        mode: ModuleMode::View,
                        subpath,
                        path: Some(path),
                        name: String::new(),
                        error: None,
                    });
                }
                Err(e) => {
                    self.pane = Pane::Error(Arc::new(vec![crate::i18n::t!(
                        "editor-failed-read",
                        "path" => subpath,
                        "error" => e.to_string()
                    )]));
                }
            }
        }
        Update::none()
    }

    /// The currently-selected folder, used to pre-place a new item.
    fn current_folder(&self) -> Option<String> {
        match &self.selection {
            Selection::Folder(path) => Some(path.clone()),
            Selection::Script(key) => key.folder_name.clone(),
            _ => None,
        }
    }

    /// Toggle the enable state of the node open in the editor (alias/trigger/
    /// hotkey/folder) — the single enable switch.
    pub(super) fn toggle_open_enabled(&mut self) -> Update<Message, Event> {
        match &mut self.pane {
            Pane::Editor(state) => {
                let now = match &mut state.node {
                    EditNode::Alias(a) => {
                        a.enabled = !a.enabled;
                        a.enabled
                    }
                    EditNode::Hotkey(h) => {
                        h.enabled = !h.enabled;
                        h.enabled
                    }
                    EditNode::Trigger { enabled, .. } => {
                        *enabled = !*enabled;
                        *enabled
                    }
                };
                // Enable is a persisted property — save immediately so the change
                // is live, without requiring a separate Save.
                self.dirty = true;
                let _ = now;
                self.save_open()
            }
            Pane::Folder(_) => self.toggle_folder_enabled(),
            _ => Update::none(),
        }
    }

    /// Move the open script into `folder` (`None` = top level). In edit mode this
    /// re-homes and persists immediately — like the enable switch (`save_open`
    /// rewrites the `package` field via the same path a rename uses). In create
    /// mode it only records the choice; it's applied when the user clicks Create.
    /// The palette's "Move to…" group routes here too: the selected script is the
    /// one open in the editor, so this single handler drives both surfaces.
    pub(super) fn set_script_folder(&mut self, folder: Option<String>) -> Update<Message, Event> {
        // Normalize an empty path to top level so a stray "" never becomes a folder.
        let folder = folder.filter(|p| !p.is_empty());
        let is_edit = match &mut self.pane {
            Pane::Editor(state) => {
                match &mut state.node {
                    EditNode::Alias(a) => a.package = folder,
                    EditNode::Hotkey(h) => h.package = folder,
                    EditNode::Trigger { package, .. } => *package = folder,
                }
                state.mode == EditorMode::Edit
            }
            _ => return Update::none(),
        };
        if is_edit {
            self.dirty = true;
            self.save_open()
        } else {
            Update::none()
        }
    }

    fn toggle_folder_enabled(&mut self) -> Update<Message, Event> {
        let Pane::Folder(state) = &mut self.pane else {
            return Update::none();
        };
        let Some(path) = state.original_path.clone() else {
            return Update::none();
        };
        let next = !state.enabled;
        state.enabled = next;
        packages::set_folder_enabled(&mut self.packages, &path, next);
        if let Err(e) = packages::save_packages(&self.server_name, &self.packages) {
            return warn_none(
                crate::i18n::t!("editor-failed-save-folders", "error" => e.to_string()),
            );
        }
        Update::with_event(Event::UserAutomationsChanged {
            server_name: self.server_name.clone(),
        })
    }

    pub(super) fn save_open(&mut self) -> Update<Message, Event> {
        let Pane::Editor(state) = &mut self.pane else {
            return Update::none();
        };
        state.error = None;
        let name = state.name.trim().to_string();
        if name.is_empty() {
            state.error = Some(crate::i18n::t!("editor-name-empty"));
            return Update::none();
        }
        if let Err(message) = naming::validate_name(&name) {
            state.error = Some(message);
            return Update::none();
        }

        let mode = state.mode;
        let original_name = state.original_name.clone();
        // Conflict check.
        let conflicts = match mode {
            EditorMode::Create => self.script_exists(&name),
            EditorMode::Edit => {
                // A pure case change (e.g. `combat` → `Combat`) is the same file
                // on a case-insensitive filesystem, so it is not a conflict.
                let renamed = original_name
                    .as_deref()
                    .is_none_or(|original| !naming::names_conflict(original, &name));
                renamed && self.script_exists(&name)
            }
        };
        if conflicts {
            if let Pane::Editor(state) = &mut self.pane {
                state.error = Some(crate::i18n::t!("editor-name-in-use"));
            }
            return Update::none();
        }

        // The alias matcher persists from the draft: the compiled pattern plus
        // the authoring sidecar (absent for the Regex kind). A compile error
        // blocks the save with its message.
        let alias_matcher = if matches!(
            &self.pane,
            Pane::Editor(EditorState {
                node: EditNode::Alias(_),
                ..
            })
        ) {
            match self.alias_draft.to_pattern() {
                Ok(pattern) => Some((pattern, self.alias_draft.to_matcher())),
                Err(message) => {
                    if let Pane::Editor(state) = &mut self.pane {
                        state.error = Some(message);
                    }
                    return Update::none();
                }
            }
        } else {
            None
        };

        let body = self.editor_content.text();
        let body = body.trim_end_matches('\n').to_string();
        let final_script = match &self.pane {
            Pane::Editor(EditorState { node, .. }) => match node {
                EditNode::Alias(a) => {
                    let (pattern, matcher) =
                        alias_matcher.expect("computed above for the alias arm");
                    Script::Alias(aliases::AliasDefinition {
                        script: (!body.is_empty()).then_some(body),
                        pattern,
                        matcher,
                        ..a.clone()
                    })
                }
                EditNode::Hotkey(h) => {
                    let mut h = h.clone();
                    if !self.hotkey_state.is_empty() {
                        hotkey_helpers::set_key_and_modifiers_from_maybe_physical(
                            &mut h,
                            self.hotkey_state.clone(),
                        );
                    }
                    Script::Hotkey(hotkeys::HotkeyDefinition {
                        script: (!body.is_empty()).then_some(body),
                        ..h
                    })
                }
                EditNode::Trigger {
                    enabled,
                    language,
                    prompt,
                    priority,
                    fallthrough,
                    package,
                    rows,
                } => {
                    let mut t = triggers::TriggerDefinition {
                        patterns: None,
                        raw_patterns: None,
                        anti_patterns: None,
                        script: (!body.is_empty()).then_some(body),
                        package: package.clone(),
                        language: *language,
                        enabled: *enabled,
                        prompt: *prompt,
                        priority: *priority,
                        fallthrough: *fallthrough,
                        matchers: None,
                    };
                    if let Err((i, message)) = rows_into_trigger(rows, &mut t) {
                        let message = crate::i18n::t!(
                            "editor-row-error", "row" => (i + 1).to_string(), "error" => message
                        );
                        if let Pane::Editor(state) = &mut self.pane {
                            state.error = Some(message);
                        }
                        return Update::none();
                    }
                    Script::Trigger(t)
                }
            },
            _ => return Update::none(),
        };

        // Drop the old entry first so the re-insert below re-homes the script.
        // This covers a rename (name changed) *and* a move (only the `package`
        // folder changed): in both cases the script lives under the old key/
        // folder in `self.scripts` and must be removed, or it would end up
        // duplicated under both the old and new folder. `remove_script_by_name`
        // finds it by name anywhere in the tree, so an unchanged save is a
        // harmless remove-then-reinsert in place.
        if mode == EditorMode::Edit
            && let Some(orig) = &original_name
        {
            self.remove_script_by_name(orig);
        }
        match upsert_script_folder(&mut self.scripts, final_script.folder_name()) {
            Ok(folder) => {
                folder.insert(name.clone(), final_script);
            }
            Err(e) => {
                if let Pane::Editor(state) = &mut self.pane {
                    state.error = Some(e);
                }
                return Update::none();
            }
        }
        if let Err(e) = self.serialize_scripts() {
            if let Pane::Editor(state) = &mut self.pane {
                state.error = Some(crate::i18n::t!("editor-failed-save", "error" => e.to_string()));
            }
            return Update::none();
        }
        // Reflect the saved state in the pane.
        if let Pane::Editor(state) = &mut self.pane {
            state.mode = EditorMode::Edit;
            state.original_name = Some(name.clone());
        }
        self.selection = Selection::Script(ScriptKey {
            folder_name: self.find_script_folder(&name),
            script_name: name.clone(),
        });
        self.dirty = false;
        self.pending_nav = None;
        let toast = self.show_toast(crate::i18n::t!("editor-saved", "name" => name));
        Update::new(
            toast,
            Some(Event::UserAutomationsChanged {
                server_name: self.server_name.clone(),
            }),
        )
    }

    /// The folder path a saved script ended up in (for re-selection).
    fn find_script_folder(&self, name: &str) -> Option<String> {
        fn rec(
            scripts: &BTreeMap<String, Script>,
            name: &str,
            prefix: Option<&str>,
        ) -> Option<String> {
            for (n, script) in scripts {
                if n == name && !matches!(script, Script::Folder(_, _)) {
                    return Some(prefix.map(str::to_string).unwrap_or_default());
                }
                if let Script::Folder(_, children) = script {
                    let child_prefix = match prefix {
                        Some(p) => format!("{p}/{n}"),
                        None => n.clone(),
                    };
                    if let Some(found) = rec(children, name, Some(&child_prefix)) {
                        return Some(found);
                    }
                }
            }
            None
        }
        rec(&self.scripts, name, None).filter(|p| !p.is_empty())
    }

    pub(super) fn delete_open(&mut self) -> Update<Message, Event> {
        let original = match &self.pane {
            Pane::Editor(EditorState {
                mode: EditorMode::Edit,
                original_name: Some(name),
                ..
            }) => name.clone(),
            _ => return Update::none(),
        };
        self.remove_script_by_name(&original);
        if let Err(e) = self.serialize_scripts() {
            self.pane = Pane::Error(Arc::new(vec![crate::i18n::t!(
                "editor-failed-save-delete",
                "error" => e.to_string()
            )]));
            return Update::none();
        }
        self.dirty = false;
        self.selection = Selection::Dashboard;
        self.pane = Pane::Dashboard;
        let toast = self.show_toast(crate::i18n::t!("editor-deleted", "name" => original));
        Update::new(
            toast,
            Some(Event::UserAutomationsChanged {
                server_name: self.server_name.clone(),
            }),
        )
    }

    // ---- folder save / delete ---------------------------------------------

    pub(super) fn save_folder(&mut self) -> Update<Message, Event> {
        let (mode, original_path, path, enabled) = match &self.pane {
            Pane::Folder(state) => (
                state.mode,
                state.original_path.clone(),
                state.path.trim_matches('/').to_string(),
                state.enabled,
            ),
            _ => return Update::none(),
        };
        if let Err(message) = naming::validate_folder_path(&path) {
            if let Pane::Folder(state) = &mut self.pane {
                state.error = Some(message);
            }
            return Update::none();
        }
        match mode {
            EditorMode::Create => {
                packages::insert_folder(&mut self.packages, &path);
                if let Err(e) = packages::save_packages(&self.server_name, &self.packages) {
                    if let Pane::Folder(state) = &mut self.pane {
                        state.error = Some(crate::i18n::t!(
                            "editor-failed-save-folders",
                            "error" => e.to_string()
                        ));
                    }
                    return Update::none();
                }
                self.merge_folders();
                self.selection = Selection::Folder(path.clone());
                self.pane = Pane::Folder(FolderState {
                    mode: EditorMode::Edit,
                    original_path: Some(path.clone()),
                    path,
                    enabled,
                    error: None,
                });
                Update::with_task(self.show_toast(crate::i18n::t!("editor-folder-created")))
            }
            EditorMode::Edit => {
                let Some(old_path) = original_path else {
                    return Update::none();
                };
                if old_path == path {
                    return Update::none();
                }
                packages::rename_folder(&mut self.packages, &old_path, &path);
                self.rename_script_packages(&old_path, &path);
                if let Err(e) = packages::save_packages(&self.server_name, &self.packages) {
                    return warn_none(crate::i18n::t!(
                        "editor-failed-save-folders",
                        "error" => e.to_string()
                    ));
                }
                if let Err(e) = self.serialize_scripts() {
                    return warn_none(crate::i18n::t!(
                        "editor-failed-save-scripts",
                        "error" => e.to_string()
                    ));
                }
                self.selection = Selection::Folder(path.clone());
                self.pane = Pane::Folder(FolderState {
                    mode: EditorMode::Edit,
                    original_path: Some(path.clone()),
                    path,
                    enabled,
                    error: None,
                });
                Update::new(
                    Task_batch_reload(self),
                    Some(Event::UserAutomationsChanged {
                        server_name: self.server_name.clone(),
                    }),
                )
            }
        }
    }

    pub(super) fn delete_folder(&mut self, delete_scripts: bool) -> Update<Message, Event> {
        let path = match &self.pane {
            Pane::Folder(FolderState {
                mode: EditorMode::Edit,
                original_path: Some(path),
                ..
            }) => path.clone(),
            _ => return Update::none(),
        };
        packages::remove_folder(&mut self.packages, &path);
        if delete_scripts {
            for name in self.scripts_under(&path) {
                self.remove_script_by_name(&name);
            }
        } else {
            let parent = packages::parent_path(&path);
            self.reparent_scripts(&path, parent);
        }
        self.confirm_folder_delete = false;
        if let Err(e) = packages::save_packages(&self.server_name, &self.packages) {
            return warn_none(
                crate::i18n::t!("editor-failed-save-folders", "error" => e.to_string()),
            );
        }
        if let Err(e) = self.serialize_scripts() {
            return warn_none(
                crate::i18n::t!("editor-failed-save-scripts", "error" => e.to_string()),
            );
        }
        self.selection = Selection::Dashboard;
        self.pane = Pane::Dashboard;
        Update::new(
            Task_batch_reload(self),
            Some(Event::UserAutomationsChanged {
                server_name: self.server_name.clone(),
            }),
        )
    }

    // ---- module save / create ---------------------------------------------

    pub(super) fn save_module(&mut self) -> Update<Message, Event> {
        let path = match &self.pane {
            Pane::Module(ModuleState {
                path: Some(path), ..
            }) => path.clone(),
            _ => return Update::none(),
        };
        if let Err(e) = std::fs::write(&path, self.editor_content.text()) {
            return warn_none(
                crate::i18n::t!("editor-failed-save-module", "error" => e.to_string()),
            );
        }
        self.dirty = false;
        self.pending_nav = None;
        let toast = self.show_toast(crate::i18n::t!("editor-module-saved"));
        Update::new(
            toast,
            Some(Event::ScriptsChanged {
                server_name: self.server_name.clone(),
            }),
        )
    }

    pub(super) fn create_module(&mut self) -> Update<Message, Event> {
        let name = match &self.pane {
            Pane::Module(state) => state.name.trim().to_string(),
            _ => return Update::none(),
        };
        if let Err(message) = naming::validate_module_subpath(&name) {
            if let Pane::Module(state) = &mut self.pane {
                state.error = Some(message);
            }
            return Update::none();
        }
        let dir = match server::load_server(&self.server_name) {
            Ok(server) => server.path.join("modules"),
            Err(e) => {
                if let Pane::Module(state) = &mut self.pane {
                    state.error = Some(crate::i18n::t!(
                        "editor-failed-modules-dir",
                        "error" => e.to_string()
                    ));
                }
                return Update::none();
            }
        };
        let target = dir.join(&name);
        if let Some(parent) = target.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            if let Pane::Module(state) = &mut self.pane {
                state.error =
                    Some(crate::i18n::t!("editor-failed-create-module", "error" => e.to_string()));
            }
            return Update::none();
        }
        if let Err(e) = std::fs::write(&target, self.editor_content.text()) {
            if let Pane::Module(state) = &mut self.pane {
                state.error =
                    Some(crate::i18n::t!("editor-failed-create-module", "error" => e.to_string()));
            }
            return Update::none();
        }
        self.dirty = false;
        self.selection = Selection::Dashboard;
        self.pane = Pane::Dashboard;
        let toast = self.show_toast(crate::i18n::t!("editor-module-created", "name" => name));
        Update::new(
            Task_batch_module_reload(toast),
            Some(Event::ScriptsChanged {
                server_name: self.server_name.clone(),
            }),
        )
    }

    // ---- tree mutation helpers (folder rename/delete) ---------------------

    fn scripts_under(&self, folder: &str) -> Vec<String> {
        let folder_slash = format!("{folder}/");
        let mut names = Vec::new();
        collect_scripts_under(&self.scripts, folder, &folder_slash, &mut names);
        names
    }

    fn rename_script_packages(&mut self, old: &str, new: &str) {
        let old_slash = format!("{old}/");
        for_each_script_mut(&mut self.scripts, &mut |script| {
            if let Some(pkg) = script_package_field(script) {
                let updated = pkg.as_deref().and_then(|p| {
                    if p == old {
                        Some(new.to_owned())
                    } else {
                        p.strip_prefix(&old_slash)
                            .map(|suffix| format!("{new}/{suffix}"))
                    }
                });
                if let Some(updated) = updated {
                    *pkg = Some(updated);
                }
            }
        });
    }

    fn reparent_scripts(&mut self, folder: &str, target: Option<String>) {
        let folder_slash = format!("{folder}/");
        for_each_script_mut(&mut self.scripts, &mut |script| {
            if let Some(pkg) = script_package_field(script) {
                let under = pkg
                    .as_deref()
                    .is_some_and(|p| p == folder || p.starts_with(&folder_slash));
                if under {
                    *pkg = target.clone();
                }
            }
        });
    }
}

// ---- free helpers ----------------------------------------------------------

fn hotkey_definition_to_keys(hotkey: &hotkeys::HotkeyDefinition) -> Vec<MaybePhysicalKey> {
    use iced::keyboard::{Key, key::Named};
    let mut keys = Vec::new();
    for modifier in &hotkey.modifiers {
        let modifier_key = match modifier.as_str() {
            "CTRL" => MaybePhysicalKey::Key(Key::Named(Named::Control)),
            "ALT" => MaybePhysicalKey::Key(Key::Named(Named::Alt)),
            "SHIFT" => MaybePhysicalKey::Key(Key::Named(Named::Shift)),
            "SUPER" => MaybePhysicalKey::Key(Key::Named(Named::Super)),
            _ => continue,
        };
        keys.push(modifier_key);
    }
    keys.push(hotkey_helpers::hotkey_to_maybe_physical_key(hotkey));
    keys
}

fn script_package_field(script: &mut Script) -> Option<&mut Option<String>> {
    match script {
        Script::Alias(a) => Some(&mut a.package),
        Script::Hotkey(h) => Some(&mut h.package),
        Script::Trigger(t) => Some(&mut t.package),
        Script::Folder(_, _) => None,
    }
}

fn for_each_script_mut(scripts: &mut BTreeMap<String, Script>, f: &mut impl FnMut(&mut Script)) {
    for script in scripts.values_mut() {
        if let Script::Folder(_, children) = script {
            for_each_script_mut(children, f);
        } else {
            f(script);
        }
    }
}

fn collect_scripts_under(
    scripts: &BTreeMap<String, Script>,
    folder: &str,
    folder_slash: &str,
    out: &mut Vec<String>,
) {
    for (name, script) in scripts {
        if let Script::Folder(_, children) = script {
            collect_scripts_under(children, folder, folder_slash, out);
        } else {
            let pkg = script.folder_name();
            if pkg == Some(folder) || pkg.is_some_and(|p| p.starts_with(folder_slash)) {
                out.push(name.clone());
            }
        }
    }
}

#[allow(non_snake_case)]
fn Task_batch_reload(window: &AutomationsWindow) -> iced::Task<Message> {
    iced::Task::batch([
        iced::Task::done(window.load_scripts_message()),
        iced::Task::done(Message::LoadFolders),
    ])
}

#[allow(non_snake_case)]
fn Task_batch_module_reload(toast: iced::Task<Message>) -> iced::Task<Message> {
    iced::Task::batch([iced::Task::done(Message::LoadModules), toast])
}

// ============================================================================
// View-side
// ============================================================================

impl AutomationsWindow {
    /// A scene header: leading dot · large title · subtitle · right-aligned actions.
    pub(super) fn scene_header<'a>(
        &self,
        status: Option<NodeStatus>,
        title: &str,
        subtitle: Option<String>,
        actions: Option<Elem<'a>>,
    ) -> Elem<'a> {
        self.scene_header_impl(status, title, subtitle, actions, None)
    }

    /// Like [`scene_header`], but with a right-aligned control on the subtitle
    /// line (the folder picker). Placing it there keeps it directly beneath the
    /// header actions without deepening the header — the subtitle row already
    /// exists, so panes with and without the aside stay the same height.
    pub(super) fn scene_header_with_aside<'a>(
        &self,
        status: Option<NodeStatus>,
        title: &str,
        subtitle: Option<String>,
        actions: Option<Elem<'a>>,
        subtitle_aside: Elem<'a>,
    ) -> Elem<'a> {
        self.scene_header_impl(status, title, subtitle, actions, Some(subtitle_aside))
    }

    fn scene_header_impl<'a>(
        &self,
        status: Option<NodeStatus>,
        title: &str,
        subtitle: Option<String>,
        actions: Option<Elem<'a>>,
        subtitle_aside: Option<Elem<'a>>,
    ) -> Elem<'a> {
        let mut title_row = row![].spacing(10.0).align_y(Vertical::Center);
        if let Some(status) = status {
            title_row = title_row.push(common::status_dot(status));
        }
        title_row = title_row.push(text(title.to_string()).size(30.0).font(Font {
            weight: iced::font::Weight::Light,
            ..fonts::GEIST_VF
        }));
        title_row = title_row.push(iced::widget::space::horizontal());
        if let Some(actions) = actions {
            title_row = title_row.push(actions);
        }
        let mut header = column![title_row].spacing(4.0);
        if let Some(aside) = subtitle_aside {
            // Subtitle text on the left, the aside control right-aligned so it
            // sits beneath the header actions.
            let mut sub_row = row![].spacing(10.0).align_y(Vertical::Center);
            if let Some(subtitle) = subtitle {
                sub_row = sub_row.push(text(subtitle).size(13.0).style(common::muted));
            }
            sub_row = sub_row.push(iced::widget::space::horizontal());
            sub_row = sub_row.push(aside);
            header = header.push(sub_row);
        } else if let Some(subtitle) = subtitle {
            header = header.push(text(subtitle).size(13.0).style(common::muted));
        }
        column![header, iced::widget::rule::horizontal(1.0),]
            .spacing(12.0)
            .into()
    }

    /// The sticky save bar shown for dirty editors / create panes. A
    /// `delete_link` label renders the destructive affordance as the deck's
    /// red underlined text link (with `Cancel` beside `Save`); `None` keeps
    /// the plain `Delete` button the other panes use.
    pub(super) fn save_bar<'a>(
        &self,
        create: bool,
        can_delete: bool,
        save_label: &str,
        delete_link: Option<&str>,
    ) -> Option<Elem<'a>> {
        if !create && !self.dirty && !can_delete {
            return None;
        }
        let mut bar = row![]
            .spacing(12.0)
            .align_y(Vertical::Center)
            .padding(Padding {
                top: 12.0,
                bottom: 4.0,
                left: 0.0,
                right: 0.0,
            });
        if can_delete {
            bar = bar.push(match delete_link {
                Some(label) => danger_link(label.to_string(), Message::Delete),
                None => button(text(crate::i18n::t!("editor-delete")).size(13.0))
                    .style(button_style::secondary)
                    .on_press(Message::Delete)
                    .into(),
            });
        }
        if self.dirty {
            bar = bar.push(text("\u{25CF}").size(9.0).style(common::accent));
            bar = bar.push(
                text(crate::i18n::t!("editor-unsaved"))
                    .size(13.0)
                    .style(common::muted),
            );
            bar = bar.push(iced::widget::space::horizontal());
            let cancel = if delete_link.is_some() {
                crate::i18n::t!("action-cancel")
            } else {
                crate::i18n::t!("editor-discard")
            };
            bar = bar.push(
                button(text(cancel).size(13.0))
                    .style(button_style::secondary)
                    .on_press(Message::Discard),
            );
            bar = bar.push(
                button(text(save_label.to_string()).size(13.0))
                    .style(button_style::primary)
                    .on_press(Message::Save),
            );
        }
        Some(container(bar).width(Length::Fill).into())
    }

    fn behavior_radios<'a>(&self, current: ScriptLang) -> Elem<'a> {
        row![
            radio(
                crate::i18n::t!("editor-send-text"),
                ScriptLang::Plaintext,
                Some(current),
                Message::SetBehavior
            ),
            radio(
                "JavaScript",
                ScriptLang::JS,
                Some(current),
                Message::SetBehavior
            ),
        ]
        .spacing(24.0)
        .align_y(Vertical::Center)
        .into()
    }

    /// The "When it runs" module behind its disclosure: hidden as a text link
    /// (its grid label rendered empty) until clicked, forced open — and not
    /// re-hideable — while any value is non-default (`prompt` included), with
    /// a hide link when open on pure defaults.
    fn order_module<'a>(
        &self,
        priority: i32,
        fallthrough: bool,
        prompt: Option<bool>,
        trigger: bool,
    ) -> Elem<'a> {
        let non_default = priority != 0 || !fallthrough || prompt == Some(true);
        if !non_default && !self.order_revealed {
            let label = if trigger {
                crate::i18n::t!("editor-reveal-order-triggers")
            } else {
                crate::i18n::t!("editor-reveal-order-aliases")
            };
            return field_row("", text_link(label, Message::RevealOrder));
        }

        // The priority stepper: a collapsed-border [-|value|+] segment.
        let stepper = container(
            row![
                button(text("-").size(14.0))
                    .style(button_style::toolbar)
                    .on_press(Message::AdjustPriority(-1))
                    .padding(Padding {
                        top: 2.0,
                        bottom: 2.0,
                        left: 10.0,
                        right: 10.0,
                    }),
                container(text(priority.to_string()).size(13.0))
                    .width(Length::Fixed(40.0))
                    .align_x(iced::alignment::Horizontal::Center),
                button(text("+").size(14.0))
                    .style(button_style::toolbar)
                    .on_press(Message::AdjustPriority(1))
                    .padding(Padding {
                        top: 2.0,
                        bottom: 2.0,
                        left: 10.0,
                        right: 10.0,
                    }),
            ]
            .align_y(Vertical::Center),
        )
        .style(common::outline_box_style);

        let priority_row = row![
            text(crate::i18n::ts!("editor-priority"))
                .size(13.0)
                .style(common::muted),
            stepper,
            text(if trigger {
                crate::i18n::ts!("editor-priority-note-triggers")
            } else {
                crate::i18n::ts!("editor-priority-note-aliases")
            })
            .size(12.0)
            .style(common::muted),
        ]
        .spacing(10.0)
        .align_y(Vertical::Center);

        let continue_row = checkbox(fallthrough)
            .label(if trigger {
                crate::i18n::ts!("editor-continue-triggers")
            } else {
                crate::i18n::ts!("editor-continue-aliases")
            })
            .on_toggle(|_| Message::ToggleFallthrough)
            .size(14.0)
            .text_size(13.0);

        let mut inner = column![priority_row, continue_row].spacing(10.0);
        if let Some(prompt) = prompt {
            inner = inner.push(
                column![
                    checkbox(prompt)
                        .label(crate::i18n::ts!("editor-prompt"))
                        .on_toggle(|_| Message::TogglePrompt)
                        .size(14.0)
                        .text_size(13.0),
                    container(
                        text(crate::i18n::ts!("editor-prompt-note"))
                            .size(12.0)
                            .style(common::muted),
                    )
                    .padding(Padding {
                        top: 0.0,
                        bottom: 0.0,
                        left: 22.0,
                        right: 0.0,
                    }),
                ]
                .spacing(2.0),
            );
        }
        if !non_default {
            inner = inner.push(text_link(
                crate::i18n::t!("editor-hide-order"),
                Message::HideOrder,
            ));
        }
        field_row(crate::i18n::ts!("editor-when-it-runs"), inner.into())
    }

    /// The Matched-values rail: one clickable badge per capture the current
    /// matcher provides, inserting its reference at the caret in the action
    /// body. Absent entirely when nothing is captured.
    fn matched_values_rail<'a>(&self, references: Vec<String>) -> Option<Elem<'a>> {
        if references.is_empty() {
            return None;
        }
        let mut rail = row![].spacing(6.0).align_y(Vertical::Center);
        for reference in references {
            rail = rail.push(
                button(
                    text(reference.clone())
                        .size(12.0)
                        .font(fonts::GEIST_MONO_VF),
                )
                .style(button_style::secondary)
                .on_press(Message::InsertReference(reference))
                .padding([3, 8]),
            );
        }
        Some(
            column![
                common::section_label(crate::i18n::ts!("editor-matched-values")),
                rail,
            ]
            .spacing(4.0)
            .into(),
        )
    }

    /// The capture references the open alias's draft provides, rendered in the
    /// action language's vocabulary (`$name` for text, `matches.name` for JS).
    fn alias_capture_references(&self, language: ScriptLang) -> Vec<String> {
        let draft = &self.alias_draft;
        let captures: Vec<Option<String>> = match draft.kind {
            AliasKind::Command => draft
                .args
                .iter()
                .map(|arg| Some(arg.name.clone()))
                .collect(),
            AliasKind::Pattern => {
                let compiled = matchers::compile_pattern(
                    &draft.pattern_source,
                    draft.anchor_start,
                    draft.anchor_end,
                );
                if compiled.errors.is_empty() {
                    compiled.captures
                } else {
                    Vec::new()
                }
            }
            AliasKind::Regex => regex::Regex::new(&draft.regex_source)
                .map(|re| {
                    re.capture_names()
                        .skip(1)
                        .map(|n| n.map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        };
        render_references(&captures, language)
    }

    /// The capture references a trigger's Match/Raw rows provide (the union,
    /// in row order).
    fn trigger_capture_references(rows: &[TriggerRow], language: ScriptLang) -> Vec<String> {
        let mut captures: Vec<Option<String>> = Vec::new();
        for row in rows {
            if row.role == PatternKind::Anti || row.source.trim().is_empty() {
                continue;
            }
            let Ok(source) = row.compiled() else { continue };
            let Ok(re) = regex::Regex::new(&source) else {
                continue;
            };
            for name in re.capture_names().skip(1) {
                let name = name.map(str::to_string);
                if name.is_some() && captures.contains(&name) {
                    continue;
                }
                captures.push(name);
            }
        }
        render_references(&captures, language)
    }

    /// The "Folder" control in a script editor: a `pick_list` of every folder
    /// (plus "(top level)"). Picking a destination emits [`Message::SetScriptFolder`],
    /// which moves the script (immediately in edit mode, on Create otherwise).
    fn folder_picker<'a>(&self, current: Option<&str>) -> Elem<'a> {
        let selected = FolderChoice::from_package(current);
        let mut options = vec![FolderChoice::TopLevel];
        options.extend(
            self.all_folder_paths()
                .into_iter()
                .map(FolderChoice::Folder),
        );
        // The current folder is normally already a real tree folder, but guard so
        // the picker never shows a blank selection if it somehow isn't listed.
        if !options.contains(&selected) {
            options.push(selected.clone());
        }
        pick_list(options, Some(selected), |choice: FolderChoice| {
            Message::SetScriptFolder(choice.into_package())
        })
        .text_size(13.0)
        .padding(Padding {
            top: 3.0,
            bottom: 3.0,
            left: 8.0,
            right: 6.0,
        })
        .into()
    }

    /// The send-text action body: the shared body editor with `$ref`
    /// highlighting — known references take the capture accent, unknown ones
    /// the error color (`matching-logic.md` §9).
    fn send_text_editor<'a>(&'a self, known: Vec<String>) -> Elem<'a> {
        let editor = text_editor(&self.editor_content)
            .highlight_with::<highlight::PatternHighlighter>(
                highlight::FieldSyntax::SendText { known },
                token_format,
            )
            .font(fonts::GEIST_MONO_VF)
            .on_action(Message::ScriptEditorAction)
            .height(Length::Fixed(220.0));
        column![
            common::section_label(crate::i18n::ts!("editor-script")),
            container(editor).style(common::code_surface_style),
        ]
        .spacing(6.0)
        .into()
    }

    /// The action body editor for `language`: `$ref`-aware for send-text
    /// bodies, the stock code highlighter for scripts. `known` is every
    /// reference the current matcher provides, `$0` included.
    fn action_body_editor<'a>(&'a self, language: ScriptLang, mut known: Vec<String>) -> Elem<'a> {
        if language == ScriptLang::Plaintext {
            known.push("$0".to_string());
            self.send_text_editor(known)
        } else {
            self.code_editor(language)
        }
    }

    /// The syntax-highlighted code body editor.
    fn code_editor<'a>(&'a self, language: ScriptLang) -> Elem<'a> {
        let token = match language {
            ScriptLang::JS => "js",
            ScriptLang::TS => "ts",
            ScriptLang::Plaintext => "txt",
        }
        .to_string();
        let editor = text_editor(&self.editor_content)
            .highlight_with::<iced::highlighter::Highlighter>(
                iced::highlighter::Settings {
                    theme: iced::highlighter::Theme::SolarizedDark,
                    token,
                },
                |h: &iced::highlighter::Highlight, _| h.to_format(),
            )
            .font(fonts::GEIST_MONO_VF)
            .on_action(Message::ScriptEditorAction)
            .height(Length::Fixed(220.0));
        column![
            common::section_label(crate::i18n::ts!("editor-script")),
            container(editor).style(common::code_surface_style),
        ]
        .spacing(6.0)
        .into()
    }

    fn field_label<'a>(label: &str) -> Elem<'a> {
        container(text(label.to_string()).size(13.0).style(common::muted))
            .width(Length::Fixed(LABEL_WIDTH))
            .align_y(Vertical::Center)
            .height(Length::Fixed(34.0))
            .into()
    }

    pub(super) fn view_editor<'a>(&'a self, state: &'a EditorState) -> Elem<'a> {
        match &state.node {
            EditNode::Alias(alias) => self.view_alias_editor(state, alias),
            EditNode::Hotkey(hotkey) => self.view_hotkey_editor(state, hotkey),
            EditNode::Trigger {
                enabled,
                language,
                prompt,
                priority,
                fallthrough,
                rows,
                ..
            } => self.view_trigger_editor(
                state,
                *enabled,
                *language,
                *prompt,
                *priority,
                *fallthrough,
                rows,
            ),
        }
    }

    fn editor_status(create: bool, enabled: bool, has_error: bool) -> NodeStatus {
        if !enabled {
            NodeStatus::Disabled
        } else if has_error && !create {
            NodeStatus::Error
        } else {
            NodeStatus::Ok
        }
    }

    fn header_actions<'a>(&self, badge_label: &str, enabled: bool) -> Elem<'a> {
        row![
            common::badge(badge_label.to_string()),
            common::pill_switch(enabled, false, Some(Message::ToggleEnabled)),
        ]
        .spacing(14.0)
        .align_y(Vertical::Center)
        .into()
    }

    /// The right-aligned "Folder" placement picker shown on a script editor's
    /// subtitle line, directly beneath the header's enable switch. Living on the
    /// existing subtitle row keeps the header the same height as panes without a
    /// picker, with the dropdown sized to match the switch above it.
    fn folder_aside<'a>(&self, folder: Option<&str>) -> Elem<'a> {
        row![
            text(crate::i18n::t!("editor-folder"))
                .size(13.0)
                .style(common::muted),
            self.folder_picker(folder),
        ]
        .spacing(8.0)
        .align_y(Vertical::Center)
        .into()
    }

    fn view_alias_editor<'a>(
        &'a self,
        state: &'a EditorState,
        alias: &'a aliases::AliasDefinition,
    ) -> Elem<'a> {
        let create = state.mode == EditorMode::Create;
        let badge_label = if alias.language == ScriptLang::JS {
            "JavaScript"
        } else {
            crate::i18n::ts!("editor-text")
        };
        let title = if create {
            crate::i18n::ts!("editor-new-alias")
        } else {
            state.name.as_str()
        };
        let subtitle = subtitle_for(
            create,
            crate::i18n::ts!("automation-alias"),
            alias.package.as_deref(),
        );
        let status = Self::editor_status(create, alias.enabled, false);

        let mut body = column![self.scene_header_with_aside(
            Some(status),
            title,
            Some(subtitle),
            Some(self.header_actions(badge_label, alias.enabled)),
            self.folder_aside(alias.package.as_deref()),
        ),]
        .spacing(16.0);

        body = body.push(
            text(crate::i18n::ts!("editor-deck-alias"))
                .size(13.0)
                .style(common::muted),
        );

        if let Some(error) = &state.error {
            body = body.push(error_bar(error));
        }

        body = body.push(field_row(
            crate::i18n::ts!("editor-name"),
            text_input(crate::i18n::ts!("editor-example-alias-name"), &state.name)
                .on_input(Message::SetName)
                .size(14.0)
                .into(),
        ));
        body = body.push(field_row(
            crate::i18n::ts!("editor-match-input-as"),
            self.alias_kind_cards(),
        ));
        match self.alias_draft.kind {
            AliasKind::Command => {
                body = self.alias_command_fields(body);
            }
            AliasKind::Pattern => {
                body = body.push(field_row(
                    crate::i18n::ts!("editor-pattern"),
                    matcher_field(
                        &self.alias_pattern_content,
                        crate::i18n::ts!("editor-example-alias-simple"),
                        highlight::FieldSyntax::Pattern,
                        (!self.alias_draft.anchor_start, !self.alias_draft.anchor_end),
                        true,
                        Message::AliasPatternAction,
                    ),
                ));
                body = body.push(field_row(
                    "",
                    row![
                        checkbox(!self.alias_draft.anchor_start)
                            .label(crate::i18n::ts!("editor-allow-before"))
                            .on_toggle(|_| Message::ToggleAnchorStart)
                            .size(14.0)
                            .text_size(13.0),
                        checkbox(!self.alias_draft.anchor_end)
                            .label(crate::i18n::ts!("editor-allow-after"))
                            .on_toggle(|_| Message::ToggleAnchorEnd)
                            .size(14.0)
                            .text_size(13.0),
                    ]
                    .spacing(16.0)
                    .into(),
                ));
                if let Some(warning) = self.alias_pattern_warning() {
                    body = body.push(field_row(
                        "",
                        text(warning).size(12.0).style(common::warning).into(),
                    ));
                }
            }
            AliasKind::Regex => {
                body = body.push(field_row(
                    crate::i18n::ts!("editor-regex"),
                    matcher_field(
                        &self.alias_regex_content,
                        crate::i18n::ts!("editor-example-alias-regex"),
                        highlight::FieldSyntax::Regex,
                        regex_loose_sides(&self.alias_draft.regex_source),
                        false,
                        Message::AliasRegexAction,
                    ),
                ));
            }
        }
        body = body.push(self.tester_box(true, false));
        body = body.push(self.order_module(alias.priority, alias.fallthrough, None, false));
        body = body.push(field_row("Behavior", self.behavior_radios(alias.language)));
        let references = self.alias_capture_references(alias.language);
        if let Some(rail) = self.matched_values_rail(references.clone()) {
            body = body.push(rail);
        }
        body = body.push(self.action_body_editor(alias.language, references));
        if let Some(bar) = self.save_bar(
            create,
            !create,
            if create {
                crate::i18n::ts!("editor-create-alias")
            } else {
                crate::i18n::ts!("action-save")
            },
            Some(crate::i18n::ts!("editor-delete-this-alias")),
        ) {
            body = body.push(bar);
        }
        pane_scroll(body)
    }

    fn view_hotkey_editor<'a>(
        &'a self,
        state: &'a EditorState,
        hotkey: &'a hotkeys::HotkeyDefinition,
    ) -> Elem<'a> {
        let create = state.mode == EditorMode::Create;
        let badge_label = if hotkey.language == ScriptLang::JS {
            "JavaScript"
        } else {
            crate::i18n::ts!("editor-text")
        };
        let title = if create {
            crate::i18n::ts!("editor-new-hotkey")
        } else {
            state.name.as_str()
        };
        let subtitle = subtitle_for(
            create,
            crate::i18n::ts!("automation-hotkey"),
            hotkey.package.as_deref(),
        );
        let status = Self::editor_status(create, hotkey.enabled, false);

        let mut body = column![self.scene_header_with_aside(
            Some(status),
            title,
            Some(subtitle),
            Some(self.header_actions(badge_label, hotkey.enabled)),
            self.folder_aside(hotkey.package.as_deref()),
        )]
        .spacing(16.0);
        if let Some(error) = &state.error {
            body = body.push(error_bar(error));
        }
        body = body.push(field_row(
            crate::i18n::ts!("editor-name"),
            text_input(crate::i18n::ts!("editor-example-hotkey-name"), &state.name)
                .on_input(Message::SetName)
                .size(14.0)
                .into(),
        ));
        body = body.push(field_row(
            crate::i18n::ts!("editor-shortcut"),
            Element::new(
                HotkeyInput::new(&self.hotkey_state, true)
                    .id(iced::widget::Id::new("automation-hotkey-shortcut"))
                    .height(Length::Fixed(34.0))
                    .on_action(Message::MarkHotkeyState),
            ),
        ));
        body = body.push(field_row(
            crate::i18n::ts!("editor-behavior"),
            self.behavior_radios(hotkey.language),
        ));
        body = body.push(self.code_editor(hotkey.language));
        if let Some(bar) = self.save_bar(
            create,
            !create,
            if create {
                crate::i18n::ts!("editor-create-hotkey")
            } else {
                crate::i18n::ts!("action-save")
            },
            None,
        ) {
            body = body.push(bar);
        }
        pane_scroll(body)
    }

    #[allow(clippy::too_many_arguments)]
    fn view_trigger_editor<'a>(
        &'a self,
        state: &'a EditorState,
        enabled: bool,
        language: ScriptLang,
        prompt: bool,
        priority: i32,
        fallthrough: bool,
        rows: &'a [TriggerRow],
    ) -> Elem<'a> {
        let create = state.mode == EditorMode::Create;
        let title = if create {
            crate::i18n::ts!("editor-new-trigger")
        } else {
            state.name.as_str()
        };
        let subtitle = subtitle_for(
            create,
            crate::i18n::ts!("automation-trigger"),
            trigger_package(state),
        );
        let any_invalid = rows
            .iter()
            .any(|row| !row.source.trim().is_empty() && row.compiled().is_err());
        let status = Self::editor_status(create, enabled, any_invalid);

        let mut body = column![self.scene_header_with_aside(
            Some(status),
            title,
            Some(subtitle),
            Some(self.header_actions("JavaScript", enabled)),
            self.folder_aside(trigger_package(state)),
        )]
        .spacing(16.0);

        // Keep this slot mounted while a pattern crosses the valid/invalid
        // boundary. Inserting the banner ahead of the form used to shift the
        // focused text input to a different iced tree position, resetting its
        // state after the first character that made the regex invalid.
        let error = state
            .error
            .as_deref()
            .or_else(|| any_invalid.then(|| crate::i18n::ts!("editor-patterns-invalid")));

        body = body.push(
            text(crate::i18n::ts!("editor-deck-trigger"))
                .size(13.0)
                .style(common::muted),
        );
        body = body.push(error_slot(error));

        body = body.push(field_row(
            crate::i18n::ts!("editor-name"),
            text_input(crate::i18n::ts!("editor-example-trigger-name"), &state.name)
                .on_input(Message::SetName)
                .size(14.0)
                .into(),
        ));

        // The unified matcher row list: role + syntax dropdowns, the source
        // field, its status dot against the test line, and (Pattern syntax)
        // the anchor checkboxes on a second line.
        let raw_subject = raw_of(&self.test_input);
        let plain_subject = plain_of(&raw_subject);
        let mut patterns = Column::new().spacing(6.0);
        for (i, (trigger_row, row_content)) in
            rows.iter().zip(&self.trigger_row_contents).enumerate()
        {
            let subject = if trigger_row.role == PatternKind::Raw {
                raw_subject.as_str()
            } else {
                plain_subject.as_str()
            };
            let valid = if trigger_row.source.trim().is_empty() {
                NodeStatus::Disabled
            } else {
                match trigger_row.compiled().map(|s| regex::Regex::new(&s)) {
                    Err(_) | Ok(Err(_)) => NodeStatus::Error,
                    Ok(Ok(re)) if !self.test_input.is_empty() && re.is_match(subject) => {
                        // A matching exception is what BLOCKS the trigger.
                        if trigger_row.role == PatternKind::Anti {
                            NodeStatus::Error
                        } else {
                            NodeStatus::Ok
                        }
                    }
                    Ok(Ok(_)) => NodeStatus::Disabled,
                }
            };
            let mut row_column = Column::new().spacing(4.0).push(
                row![
                    pick_list(
                        SyntaxChoice::ALL.to_vec(),
                        Some(SyntaxChoice(trigger_row.syntax)),
                        move |s| { Message::SetRowSyntax(i, s.0) }
                    )
                    .text_size(13.0),
                    pick_list(
                        PatternKind::ALL.to_vec(),
                        Some(trigger_row.role),
                        move |k| { Message::SetPatternKind(i, k) }
                    )
                    .text_size(13.0),
                    matcher_field(
                        row_content,
                        if trigger_row.syntax == MatcherSyntax::Pattern {
                            crate::i18n::ts!("editor-example-trigger-pattern")
                        } else if trigger_row.role == PatternKind::Raw {
                            crate::i18n::ts!("editor-example-trigger-raw")
                        } else {
                            crate::i18n::ts!("editor-example-trigger-regex")
                        },
                        if trigger_row.syntax == MatcherSyntax::Pattern {
                            highlight::FieldSyntax::Pattern
                        } else {
                            highlight::FieldSyntax::Regex
                        },
                        if trigger_row.syntax == MatcherSyntax::Pattern {
                            (!trigger_row.anchor_start, !trigger_row.anchor_end)
                        } else {
                            regex_loose_sides(&trigger_row.source)
                        },
                        trigger_row.syntax == MatcherSyntax::Pattern,
                        move |action| Message::RowSourceAction(i, action),
                    ),
                    container(common::status_dot(valid)).padding(Padding {
                        top: 0.0,
                        bottom: 0.0,
                        left: 4.0,
                        right: 4.0,
                    }),
                    button(
                        text(bootstrap_icons::TRASH_3)
                            .font(fonts::BOOTSTRAP_ICONS)
                            .size(14.0)
                    )
                    .style(button_style::secondary)
                    .on_press(Message::RemovePattern(i))
                    .padding(8),
                ]
                .spacing(8.0)
                .align_y(Vertical::Center),
            );
            if trigger_row.syntax == MatcherSyntax::Pattern {
                row_column = row_column.push(
                    row![
                        checkbox(!trigger_row.anchor_start)
                            .label(crate::i18n::ts!("editor-allow-before"))
                            .on_toggle(move |_| Message::ToggleRowAnchorStart(i))
                            .size(14.0)
                            .text_size(12.0),
                        checkbox(!trigger_row.anchor_end)
                            .label(crate::i18n::ts!("editor-allow-after"))
                            .on_toggle(move |_| Message::ToggleRowAnchorEnd(i))
                            .size(14.0)
                            .text_size(12.0),
                    ]
                    .spacing(16.0),
                );
            }
            patterns = patterns.push(row_column);
        }
        patterns = patterns.push(
            button(
                row![
                    text(bootstrap_icons::PLUS_LG)
                        .font(fonts::BOOTSTRAP_ICONS)
                        .size(12.0),
                    text(crate::i18n::t!("editor-add-pattern")).size(13.0),
                ]
                .spacing(6.0)
                .align_y(Vertical::Center),
            )
            .style(button_style::secondary)
            .on_press(Message::AddPattern),
        );
        body = body.push(field_row(
            crate::i18n::ts!("editor-patterns"),
            patterns.into(),
        ));

        let has_raw = rows
            .iter()
            .any(|row| row.role == PatternKind::Raw && !row.source.trim().is_empty());
        body = body.push(self.tester_box(false, has_raw));
        body = body.push(self.order_module(priority, fallthrough, Some(prompt), true));
        body = body.push(field_row("Behavior", self.behavior_radios(language)));
        let references = Self::trigger_capture_references(rows, language);
        if let Some(rail) = self.matched_values_rail(references.clone()) {
            body = body.push(rail);
        }
        body = body.push(self.action_body_editor(language, references));
        if let Some(bar) = self.save_bar(
            create,
            !create,
            if create {
                crate::i18n::ts!("editor-create-trigger")
            } else {
                crate::i18n::ts!("action-save")
            },
            Some(crate::i18n::ts!("editor-delete-this-trigger")),
        ) {
            body = body.push(bar);
        }
        pane_scroll(body)
    }

    /// The three alias type cards. Selection is the draft's kind; every kind's
    /// buffers survive a switch.
    fn alias_kind_cards<'a>(&self) -> Elem<'a> {
        let card = |label: &str, kind: AliasKind| {
            let selected = self.alias_draft.kind == kind;
            button(text(label.to_string()).size(13.0))
                .style(if selected {
                    button_style::primary
                } else {
                    button_style::secondary
                })
                .on_press(Message::SetAliasKind(kind))
                .padding([6, 12])
        };
        row![
            card(crate::i18n::ts!("editor-kind-command"), AliasKind::Command),
            card(crate::i18n::ts!("editor-kind-pattern"), AliasKind::Pattern),
            card(crate::i18n::ts!("editor-kind-regex"), AliasKind::Regex),
        ]
        .spacing(8.0)
        .into()
    }

    /// The Command kind's field block: name + mode, the argument rows, the
    /// generated usage line, and (Advanced only) the parsing picker.
    fn alias_command_fields<'a>(
        &'a self,
        mut body: Column<'a, Message, Theme>,
    ) -> Column<'a, Message, Theme> {
        let draft = &self.alias_draft;
        body = body.push(field_row(
            crate::i18n::ts!("editor-command"),
            row![
                text_input(crate::i18n::ts!("editor-example-command"), &draft.command)
                    .on_input(Message::SetCommandName)
                    .size(14.0)
                    .width(Length::Fill),
                radio(
                    crate::i18n::ts!("editor-cmd-simple"),
                    CmdMode::Simple,
                    Some(draft.cmd_mode),
                    Message::SetCmdMode,
                )
                .size(14.0)
                .text_size(13.0),
                radio(
                    crate::i18n::ts!("editor-cmd-advanced"),
                    CmdMode::Advanced,
                    Some(draft.cmd_mode),
                    Message::SetCmdMode,
                )
                .size(14.0)
                .text_size(13.0),
            ]
            .spacing(12.0)
            .align_y(Vertical::Center)
            .into(),
        ));

        let mut args = Column::new().spacing(6.0);
        let last = draft.args.len().saturating_sub(1);
        for (i, arg) in draft.args.iter().enumerate() {
            let mut arg_row = row![
                text_input(crate::i18n::ts!("editor-example-arg-name"), &arg.name)
                    .on_input(move |v| Message::SetArgName(i, v))
                    .size(14.0)
                    .width(Length::Fill),
            ]
            .spacing(8.0)
            .align_y(Vertical::Center);
            if draft.cmd_mode == CmdMode::Advanced {
                // Rest of line is offered only on the last row.
                let options: Vec<ArgKindChoice> = if i == last {
                    vec![
                        ArgKindChoice(ArgKind::Required),
                        ArgKindChoice(ArgKind::Optional),
                        ArgKindChoice(ArgKind::Rest),
                    ]
                } else {
                    vec![
                        ArgKindChoice(ArgKind::Required),
                        ArgKindChoice(ArgKind::Optional),
                    ]
                };
                arg_row = arg_row.push(
                    pick_list(options, Some(ArgKindChoice(arg.kind)), move |choice| {
                        Message::SetArgKind(i, choice.0)
                    })
                    .text_size(13.0),
                );
            }
            arg_row = arg_row.push(
                button(
                    text(bootstrap_icons::TRASH_3)
                        .font(fonts::BOOTSTRAP_ICONS)
                        .size(14.0),
                )
                .style(button_style::secondary)
                .on_press(Message::RemoveArg(i))
                .padding(8),
            );
            args = args.push(arg_row);
        }
        args = args.push(
            button(
                row![
                    text(bootstrap_icons::PLUS_LG)
                        .font(fonts::BOOTSTRAP_ICONS)
                        .size(12.0),
                    text(crate::i18n::t!("editor-add-argument")).size(13.0),
                ]
                .spacing(6.0)
                .align_y(Vertical::Center),
            )
            .style(button_style::secondary)
            .on_press(Message::AddArg),
        );
        body = body.push(field_row(crate::i18n::ts!("editor-arguments"), args.into()));

        // The Usage row (label included) is omitted while the name is empty.
        if !draft.command.trim().is_empty() {
            body = body.push(field_row(
                crate::i18n::ts!("editor-usage"),
                column![
                    text(matchers::usage_line(draft.command.trim(), &draft.args))
                        .size(13.0)
                        .font(fonts::GEIST_MONO_VF),
                    text(crate::i18n::t!("editor-command-completion-note"))
                        .size(12.0)
                        .style(common::muted),
                ]
                .spacing(4.0)
                .into(),
            ));
        }
        if draft.cmd_mode == CmdMode::Advanced {
            body = body.push(field_row(
                crate::i18n::ts!("editor-parsing"),
                pick_list(
                    ParseModeChoice::ALL.to_vec(),
                    Some(ParseModeChoice(draft.parse)),
                    |choice| Message::SetParseMode(choice.0),
                )
                .text_size(13.0)
                .into(),
            ));
        }
        body
    }

    /// The non-blocking matches-every-line warning for the Pattern kind.
    fn alias_pattern_warning(&self) -> Option<String> {
        let draft = &self.alias_draft;
        let compiled =
            matchers::compile_pattern(&draft.pattern_source, draft.anchor_start, draft.anchor_end);
        (compiled.errors.is_empty()
            && compiled
                .warnings
                .contains(&matchers::PatternWarning::MatchesEveryLine)
            && !draft.pattern_source.trim().is_empty())
        .then(|| crate::i18n::t!("editor-matches-every-line"))
    }

    /// The Try-it module: a collapsed accordion whose header is a call to
    /// action; expanded, the test field, its verdict, and (triggers, when a
    /// raw row exists) the byte view of the simulated raw line.
    fn tester_box<'a>(&self, alias: bool, show_bytes: bool) -> Elem<'a> {
        let header = |chevron: &'static str, label: String| {
            button(
                row![text(chevron).size(11.0), text(label).size(13.0)]
                    .spacing(8.0)
                    .align_y(Vertical::Center),
            )
            .style(button_style::quiet_link)
            .padding(0)
            .width(Length::Fill)
            .on_press(Message::ToggleTryIt)
        };

        if !self.try_it_open {
            let label = if alias {
                crate::i18n::t!("editor-try-alias-cta")
            } else {
                crate::i18n::t!("editor-try-trigger-cta")
            };
            let body = container(header("\u{25B8}", label))
                .padding(12.0)
                .width(Length::Fill)
                .style(common::banner_style);
            return field_row("", body.into());
        }

        let mut inner = column![header("\u{25BE}", crate::i18n::t!("editor-try-it"))].spacing(8.0);
        if alias {
            inner = inner.push(
                row![
                    text("\u{276F}")
                        .size(13.0)
                        .font(fonts::GEIST_MONO_VF)
                        .style(common::capture_accent),
                    text_input(
                        crate::i18n::ts!("editor-test-placeholder-alias"),
                        &self.test_input
                    )
                    .on_input(Message::SetTestInput)
                    .size(13.0),
                ]
                .spacing(8.0)
                .align_y(Vertical::Center),
            );
        } else {
            inner = inner.push(common::section_label(crate::i18n::ts!("editor-game-sent")));
            inner = inner.push(
                text_input(
                    crate::i18n::ts!("editor-test-placeholder-trigger"),
                    &self.test_input,
                )
                .on_input(Message::SetTestInput)
                .size(13.0),
            );
            if show_bytes && !self.test_input.is_empty() {
                let bytes = raw_of(&self.test_input).replace('\x1b', "\u{241B}");
                inner = inner.push(
                    text(format!(
                        "{} {bytes}",
                        crate::i18n::t!("editor-try-bytes-prefix")
                    ))
                    .size(11.0)
                    .font(fonts::GEIST_MONO_VF)
                    .style(common::faint),
                );
            }
        }
        let (verdict, status): (String, NodeStatus) = if alias {
            self.alias_draft_verdict()
        } else {
            self.trigger_verdict()
        };
        inner = inner.push(container(
            row![
                common::status_dot(status),
                text(verdict).size(12.0).style(verdict_style(status)),
            ]
            .spacing(6.0)
            .align_y(Vertical::Center),
        ));
        let body = container(inner)
            .padding(12.0)
            .width(Length::Fill)
            .style(common::banner_style);
        field_row("", body.into())
    }

    /// The alias tester's verdict, per the draft's kind.
    fn alias_draft_verdict(&self) -> (String, NodeStatus) {
        let draft = &self.alias_draft;
        let sample = &self.test_input;
        match draft.kind {
            AliasKind::Regex => alias_verdict(&draft.regex_source, sample),
            AliasKind::Pattern => {
                if draft.pattern_source.trim().is_empty() {
                    return (
                        crate::i18n::t!("editor-verdict-no-pattern"),
                        NodeStatus::Disabled,
                    );
                }
                let compiled = matchers::compile_pattern(
                    &draft.pattern_source,
                    draft.anchor_start,
                    draft.anchor_end,
                );
                if let Some(error) = compiled.errors.first() {
                    return (
                        crate::i18n::t!(
                            "editor-verdict-compile-error", "error" => pattern_error_text(error)
                        ),
                        NodeStatus::Error,
                    );
                }
                if sample.is_empty() {
                    return (
                        crate::i18n::t!("editor-enter-command"),
                        NodeStatus::Disabled,
                    );
                }
                match compiled.regex {
                    Some(re) if re.is_match(sample) => {
                        (crate::i18n::t!("editor-would-fire"), NodeStatus::Ok)
                    }
                    _ => (crate::i18n::t!("editor-no-match"), NodeStatus::Disabled),
                }
            }
            AliasKind::Command => {
                let name = draft.command.trim();
                if name.is_empty() {
                    return (
                        crate::i18n::t!("editor-verdict-no-command"),
                        NodeStatus::Disabled,
                    );
                }
                if sample.is_empty() {
                    return (
                        crate::i18n::t!("editor-enter-command"),
                        NodeStatus::Disabled,
                    );
                }
                let spec = CommandSpec {
                    name: name.to_string(),
                    args: draft.args.clone(),
                    parse: draft.parse,
                };
                match matchers::assign(sample, &spec.name, &spec.args, spec.parse) {
                    CommandOutcome::Fired { .. } => {
                        (crate::i18n::t!("editor-would-fire"), NodeStatus::Ok)
                    }
                    CommandOutcome::NotFired(miss) => command_miss_verdict(name, &miss),
                }
            }
        }
    }

    /// The trigger tester's verdict: exceptions veto against each phase's own
    /// subject, then raw rows in order, then normal rows — first hit wins,
    /// one fire per line (the runtime's semantics, told truthfully).
    fn trigger_verdict(&self) -> (String, NodeStatus) {
        let rows = match &self.pane {
            Pane::Editor(EditorState {
                node: EditNode::Trigger { rows, .. },
                ..
            }) => rows,
            _ => {
                return (crate::i18n::t!("editor-no-match"), NodeStatus::Disabled);
            }
        };
        let line = &self.test_input;
        let filled: Vec<(usize, &TriggerRow)> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !row.source.trim().is_empty())
            .collect();
        if line.is_empty() {
            return (crate::i18n::t!("editor-enter-line"), NodeStatus::Disabled);
        }
        if !filled.iter().any(|(_, row)| row.role != PatternKind::Anti) {
            return (
                crate::i18n::t!("editor-verdict-no-matchers"),
                NodeStatus::Disabled,
            );
        }

        let raw_subject = raw_of(line);
        let plain_subject = plain_of(&raw_subject);
        let compile = |row: &TriggerRow| -> Result<regex::Regex, String> {
            let source = row.compiled()?;
            regex::Regex::new(&source)
                .map_err(|e| crate::i18n::t!("editor-invalid-regex", "error" => e.to_string()))
        };

        // Any compile error surfaces first, as a failing verdict.
        for (_, row) in &filled {
            if let Err(message) = row.compiled() {
                return (
                    crate::i18n::t!("editor-verdict-compile-error", "error" => message),
                    NodeStatus::Error,
                );
            }
        }

        let blocked_in = |subject: &str| -> Option<usize> {
            filled
                .iter()
                .filter(|(_, row)| row.role == PatternKind::Anti)
                .enumerate()
                .find_map(|(nth, (_, row))| compile(row).ok()?.is_match(subject).then_some(nth + 1))
        };

        let mut first_block = None;
        for role in [PatternKind::Raw, PatternKind::Match] {
            let subject = if role == PatternKind::Raw {
                raw_subject.as_str()
            } else {
                plain_subject.as_str()
            };
            let phase: Vec<&TriggerRow> = filled
                .iter()
                .filter(|(_, row)| row.role == role)
                .map(|(_, row)| *row)
                .collect();
            if phase.is_empty() {
                continue;
            }
            if let Some(nth) = blocked_in(subject) {
                first_block.get_or_insert(nth);
                continue;
            }
            for (nth, row) in phase.iter().enumerate() {
                if compile(row).is_ok_and(|re| re.is_match(subject)) {
                    let key = if role == PatternKind::Raw {
                        crate::i18n::t!("editor-fires-on-raw", "n" => (nth + 1).to_string())
                    } else {
                        crate::i18n::t!("editor-fires-on-match", "n" => (nth + 1).to_string())
                    };
                    return (key, NodeStatus::Ok);
                }
            }
        }
        if let Some(nth) = first_block {
            return (
                crate::i18n::t!("editor-blocked-by", "n" => nth.to_string()),
                NodeStatus::Error,
            );
        }
        (crate::i18n::t!("editor-no-match"), NodeStatus::Disabled)
    }

    // ---- folder + module views --------------------------------------------

    pub(super) fn view_folder_editor<'a>(&'a self, state: &'a FolderState) -> Elem<'a> {
        let create = state.mode == EditorMode::Create;
        let count = if let Some(path) = &state.original_path {
            self.folder_child_rows(path).len()
        } else {
            0
        };
        let title = if create {
            crate::i18n::t!("editor-new-folder")
        } else {
            state
                .original_path
                .as_deref()
                .and_then(|p| p.rsplit('/').next())
                .unwrap_or(crate::i18n::ts!("editor-folder"))
                .to_string()
        };
        let subtitle = if create {
            crate::i18n::t!("editor-folder")
        } else {
            crate::i18n::t!("editor-folder-summary", "count" => count)
        };
        let actions: Option<Elem<'a>> = if create {
            None
        } else {
            Some(common::pill_switch(
                state.enabled,
                false,
                Some(Message::ToggleEnabled),
            ))
        };
        let status = if create || state.enabled {
            NodeStatus::Ok
        } else {
            NodeStatus::Disabled
        };

        let mut body =
            column![self.scene_header(Some(status), &title, Some(subtitle), actions)].spacing(16.0);

        if let Some(error) = &state.error {
            body = body.push(error_bar(error));
        }
        body = body.push(field_row(
            crate::i18n::ts!("editor-path"),
            text_input(crate::i18n::ts!("editor-example-folder-path"), &state.path)
                .on_input(Message::SetFolderPath)
                .size(14.0)
                .into(),
        ));
        let hint = if !create && !state.enabled {
            crate::i18n::ts!("editor-folder-disabled-help")
        } else {
            crate::i18n::ts!("editor-folder-help")
        };
        body = body.push(text(hint).size(12.0).style(common::muted));

        // Contents.
        if let Some(path) = &state.original_path {
            let rows = self.folder_child_rows(path);
            if !rows.is_empty() {
                let mut contents = Column::new()
                    .spacing(4.0)
                    .push(common::section_label(crate::i18n::ts!("editor-contents")));
                for (status, kind_icon, name, msg) in rows {
                    contents = contents.push(
                        button(
                            row![
                                common::status_dot(status),
                                text(kind_icon).font(fonts::BOOTSTRAP_ICONS).size(14.0),
                                text(name).size(13.0),
                            ]
                            .spacing(8.0)
                            .align_y(Vertical::Center),
                        )
                        .style(button_style::list_item)
                        .on_press(msg)
                        .width(Length::Fill),
                    );
                }
                body = body.push(contents);
            }
        }

        // Footer: delete confirm or the save bar.
        if self.confirm_folder_delete {
            body = body.push(
                container(
                    row![
                        text(crate::i18n::t!("editor-delete-folder-question"))
                            .size(13.0)
                            .align_y(Vertical::Center),
                        iced::widget::space::horizontal(),
                        button(text(crate::i18n::t!("editor-move-scripts-parent")).size(13.0))
                            .style(button_style::secondary)
                            .on_press(Message::ConfirmDeleteFolder(false)),
                        button(text(crate::i18n::t!("editor-delete-scripts-too")).size(13.0))
                            .style(button_style::secondary)
                            .on_press(Message::ConfirmDeleteFolder(true)),
                        button(text(crate::i18n::t!("action-cancel")).size(13.0))
                            .style(button_style::secondary)
                            .on_press(Message::CancelDeleteFolder),
                    ]
                    .spacing(10.0)
                    .align_y(Vertical::Center),
                )
                .padding(12.0)
                .style(common::banner_style),
            );
        } else {
            let mut bar = row![]
                .spacing(12.0)
                .align_y(Vertical::Center)
                .padding(Padding {
                    top: 12.0,
                    bottom: 4.0,
                    left: 0.0,
                    right: 0.0,
                });
            if !create {
                bar = bar.push(
                    button(text(crate::i18n::t!("action-delete")).size(13.0))
                        .style(button_style::secondary)
                        .on_press(Message::RequestDeleteFolder),
                );
            }
            bar = bar.push(iced::widget::space::horizontal());
            bar = bar.push(
                button(text(crate::i18n::t!("editor-discard")).size(13.0))
                    .style(button_style::secondary)
                    .on_press(Message::Discard),
            );
            bar = bar.push(
                button(
                    text(if create {
                        crate::i18n::t!("editor-create-folder")
                    } else {
                        crate::i18n::t!("action-save")
                    })
                    .size(13.0),
                )
                .style(button_style::primary)
                .on_press(Message::SaveFolder),
            );
            body = body.push(bar);
        }
        pane_scroll(body)
    }

    /// (status, icon, name, open-message) for each child of `folder`.
    fn folder_child_rows(&self, folder: &str) -> Vec<(NodeStatus, &'static str, String, Message)> {
        let mut out = Vec::new();
        // Find the folder's child map.
        let mut current = &self.scripts;
        for segment in folder.split('/') {
            match current.get(segment) {
                Some(Script::Folder(_, children)) => current = children,
                _ => return out,
            }
        }
        for (name, script) in current {
            let (icon, msg, status) = match script {
                Script::Folder(_, _) => {
                    let path = format!("{folder}/{name}");
                    (
                        bootstrap_icons::FOLDER_PLUS,
                        Message::SelectFolder(path.clone()),
                        if packages::is_package_effectively_enabled(&path, &self.packages) {
                            NodeStatus::Ok
                        } else {
                            NodeStatus::Disabled
                        },
                    )
                }
                other => {
                    let icon = match other {
                        Script::Alias(_) => bootstrap_icons::AT,
                        Script::Trigger(_) => bootstrap_icons::LIGHTNING,
                        Script::Hotkey(_) => bootstrap_icons::DPAD,
                        Script::Folder(_, _) => bootstrap_icons::FOLDER_PLUS,
                    };
                    (
                        icon,
                        Message::SelectScript(ScriptKey {
                            folder_name: other.folder_name().map(str::to_string),
                            script_name: name.clone(),
                        }),
                        self.script_status(other),
                    )
                }
            };
            out.push((status, icon, name.clone(), msg));
        }
        out
    }

    pub(super) fn view_module<'a>(&'a self, state: &'a ModuleState) -> Elem<'a> {
        let create = state.mode == ModuleMode::Create;
        let title = if create {
            crate::i18n::t!("editor-new-module")
        } else {
            state.subpath.clone()
        };
        let subtitle = crate::i18n::t!("editor-module-help");
        let mut body =
            column![self.scene_header(Some(NodeStatus::Ok), &title, Some(subtitle), None)]
                .spacing(16.0);
        if let Some(error) = &state.error {
            body = body.push(error_bar(error));
        }
        if create {
            body = body.push(field_row(
                crate::i18n::ts!("editor-name"),
                text_input(crate::i18n::ts!("editor-example-module-path"), &state.name)
                    .on_input(Message::SetNewModuleName)
                    .size(14.0)
                    .into(),
            ));
        }

        let token = state
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                if create {
                    state.name.clone()
                } else {
                    state.subpath.clone()
                }
            });
        let token = std::path::Path::new(&token)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("ts")
            .to_string();
        let editor = text_editor(&self.editor_content)
            .highlight_with::<iced::highlighter::Highlighter>(
                iced::highlighter::Settings {
                    theme: iced::highlighter::Theme::SolarizedDark,
                    token,
                },
                |h: &iced::highlighter::Highlight, _| h.to_format(),
            )
            .font(fonts::GEIST_MONO_VF)
            .on_action(Message::ScriptEditorAction)
            .height(Length::Fixed(360.0));
        body = body.push(
            column![
                common::section_label(crate::i18n::ts!("editor-source")),
                container(editor).style(common::code_surface_style),
            ]
            .spacing(6.0),
        );

        let mut bar = row![]
            .spacing(12.0)
            .align_y(Vertical::Center)
            .padding(Padding {
                top: 12.0,
                bottom: 4.0,
                left: 0.0,
                right: 0.0,
            });
        bar = bar.push(iced::widget::space::horizontal());
        bar = bar.push(
            button(text(crate::i18n::t!("editor-discard")).size(13.0))
                .style(button_style::secondary)
                .on_press(Message::Discard),
        );
        if create {
            bar = bar.push(
                button(text(crate::i18n::t!("editor-create-module")).size(13.0))
                    .style(button_style::primary)
                    .on_press(Message::CreateModule),
            );
        } else {
            bar = bar.push(
                button(text(crate::i18n::t!("action-save")).size(13.0))
                    .style(button_style::primary)
                    .on_press(Message::SaveModule),
            );
        }
        body = body.push(bar);
        pane_scroll(body)
    }
}

// ---- view helpers ----------------------------------------------------------

fn subtitle_for(create: bool, kind: &str, package: Option<&str>) -> String {
    if create {
        kind.to_string()
    } else if let Some(folder) = package {
        crate::i18n::t!("editor-kind-in-folder", "kind" => kind, "folder" => folder)
    } else {
        crate::i18n::t!("editor-kind-top-level", "kind" => kind)
    }
}

fn trigger_package(state: &EditorState) -> Option<&str> {
    match &state.node {
        EditNode::Trigger { package, .. } => package.as_deref(),
        _ => None,
    }
}

fn field_row<'a>(label: &str, control: Elem<'a>) -> Elem<'a> {
    row![AutomationsWindow::field_label(label), control]
        .spacing(12.0)
        .align_y(Vertical::Center)
        .into()
}

/// An underlined text link (D8): quiet at rest, full-strength on hover. The
/// underline rule and both colors come from the theme crate so every link in
/// these panes reads the same.
fn text_link<'a>(label: String, message: Message) -> Elem<'a> {
    button(button_style::underlined(text(label).size(12.0)))
        .style(button_style::quiet_link)
        .padding(0)
        .on_press(message)
        .into()
}

/// The destructive underlined link (the `Delete this alias/trigger` footer).
fn danger_link<'a>(label: String, message: Message) -> Elem<'a> {
    button(button_style::underlined(text(label).size(13.0)))
        .style(button_style::danger_link)
        .padding(0)
        .on_press(message)
        .into()
}

// ---- one-line matcher fields ------------------------------------------------

/// Applies an action to a one-line field's buffer: Enter is dropped and
/// pasted newlines flatten to spaces, so the buffer never grows a second line.
pub(super) fn perform_single_line(content: &mut text_editor::Content, action: text_editor::Action) {
    use text_editor::{Action, Edit};
    let action = match action {
        Action::Edit(Edit::Enter) => return,
        Action::Edit(Edit::Paste(pasted)) if pasted.contains('\n') || pasted.contains('\r') => {
            Action::Edit(Edit::Paste(Arc::new(
                pasted.replace("\r\n", "\n").replace(['\r', '\n'], " "),
            )))
        }
        other => other,
    };
    content.perform(action);
}

/// A one-line buffer's text, without the trailing newline `Content::text`
/// always appends.
pub(super) fn single_line_text(content: &text_editor::Content) -> String {
    let mut text = content.text();
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    text
}

/// Which sides of a regex source are unanchored (fixtures §10): left iff no
/// leading `^`, right iff no unescaped trailing `$`; neither while empty.
fn regex_loose_sides(source: &str) -> (bool, bool) {
    let source = source.trim();
    if source.is_empty() {
        return (false, false);
    }
    let anchored_end =
        source.ends_with('$') && (source.len() == 1 || !source[..source.len() - 1].ends_with('\\'));
    (!source.starts_with('^'), !anchored_end)
}

/// A small tooltip chip.
fn tip<'a>(content: Elem<'a>, label: String) -> Elem<'a> {
    iced::widget::tooltip(
        content,
        container(text(label).size(11.0))
            .padding(6.0)
            .style(common::banner_style),
        iced::widget::tooltip::Position::Top,
    )
    .into()
}

/// A `. . .` gutter cell (visual-contract §6): the literal spaced string in
/// the mono font on a faint wash, flush against the field inside the
/// composite's single border.
fn gutter_cell<'a>(tooltip_label: String) -> Elem<'a> {
    let cell = container(text(". . .").size(11.0).font(fonts::GEIST_MONO_VF).style(
        |theme: &Theme| iced::widget::text::Style {
            color: Some(theme.styles.text.normal.scale_alpha(0.32)),
        },
    ))
    .padding(Padding {
        top: 0.0,
        bottom: 0.0,
        left: 8.0,
        right: 8.0,
    })
    .height(Length::Fill)
    .align_y(Vertical::Center)
    .style(|theme: &Theme| iced::widget::container::Style {
        background: Some(iced::Background::Color(
            theme.styles.text.normal.scale_alpha(0.04),
        )),
        ..Default::default()
    });
    tip(cell.into(), tooltip_label)
}

/// The color a highlighted run takes (visual-contract §1). The island run is
/// specified as ink on a wash; a highlighter `Format` has no background
/// channel, so the island borrows the Regex kind hue instead — the
/// in-language way to mark "this run is raw regex".
fn token_format(
    token: &highlight::Token,
    theme: &Theme,
) -> iced::advanced::text::highlighter::Format<Font> {
    use highlight::Token;
    let color = match token {
        Token::Hole | Token::GroupOpen | Token::Escape | Token::KnownRef => common::KIND_PATTERN,
        Token::Wildcard => common::KIND_PATTERN.scale_alpha(0.65),
        Token::Island => common::KIND_REGEX,
        Token::UnknownRef => theme.styles.text.error,
    };
    iced::advanced::text::highlighter::Format {
        color: Some(color),
        font: None,
    }
}

/// One matcher source field: `[gutter | editor | gutter]` composed inside a
/// single bordered container (README §5.3) — the editor is chromeless, the
/// composite owns the one border, and the `. . .` gutters appear per `loose`.
/// The editor is a real `text_editor` with highlighted runs and a true caret;
/// Enter is swallowed at the key-binding layer and again in the update path.
fn matcher_field<'a>(
    content: &'a text_editor::Content,
    placeholder: &'a str,
    syntax: highlight::FieldSyntax,
    loose: (bool, bool),
    pattern_tips: bool,
    on_action: impl Fn(text_editor::Action) -> Message + 'a,
) -> Elem<'a> {
    let editor = text_editor(content)
        .placeholder(placeholder)
        .size(13.0)
        .padding(8.0)
        .font(fonts::GEIST_MONO_VF)
        .class(crate::theme::TextEditorClass::Inline)
        .key_binding(|key_press| {
            match text_editor::Binding::from_key_press(key_press) {
                // Swallow the break: captured, but edits nothing.
                Some(text_editor::Binding::Enter) => {
                    Some(text_editor::Binding::Sequence(Vec::new()))
                }
                other => other,
            }
        })
        .highlight_with::<highlight::PatternHighlighter>(syntax, token_format)
        .on_action(on_action);

    let (left_tip, right_tip) = if pattern_tips {
        (
            crate::i18n::t!("editor-gutter-before-pattern"),
            crate::i18n::t!("editor-gutter-after-pattern"),
        )
    } else {
        (
            crate::i18n::t!("editor-gutter-before-regex"),
            crate::i18n::t!("editor-gutter-after-regex"),
        )
    };
    let mut inner = row![];
    if loose.0 {
        inner = inner.push(gutter_cell(left_tip));
    }
    inner = inner.push(editor);
    if loose.1 {
        inner = inner.push(gutter_cell(right_tip));
    }
    container(inner)
        .width(Length::Fill)
        .style(|theme: &Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(
                theme.styles.general.container_background,
            )),
            border: iced::Border {
                color: theme.styles.general.border,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn error_bar<'a>(message: &str) -> Elem<'a> {
    container(
        row![
            text(bootstrap_icons::EXCLAMATION_TRIANGLE)
                .font(fonts::BOOTSTRAP_ICONS)
                .size(13.0)
                .style(common::danger),
            text(message.to_string()).size(13.0).style(common::danger),
        ]
        .spacing(8.0)
        .align_y(Vertical::Center),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 8.0,
        bottom: 8.0,
        left: 12.0,
        right: 12.0,
    })
    .style(|theme: &Theme| iced::widget::container::Style {
        background: Some(iced::Background::Color(
            theme.styles.text.error.scale_alpha(0.1),
        )),
        border: iced::Border {
            color: theme.styles.text.error.scale_alpha(0.4),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    })
    .into()
}

/// A stable editor-tree position for an optional error banner.
///
/// The outer container is always present, even when its zero-height child is
/// empty. This matters for errors derived live from an input: conditionally
/// inserting a column child ahead of that input makes iced reconcile its
/// focus state against the wrong child on the next frame.
fn error_slot<'a>(message: Option<&str>) -> Elem<'a> {
    let content: Elem<'a> = match message {
        Some(message) => error_bar(message),
        None => Space::new().height(0).into(),
    };
    container(content).into()
}

fn verdict_style(status: NodeStatus) -> fn(&Theme) -> iced::widget::text::Style {
    match status {
        NodeStatus::Ok => common::success,
        NodeStatus::Error => common::danger,
        NodeStatus::Warning => common::warning,
        NodeStatus::Disabled => common::muted,
    }
}

/// Renders capture references in the action language's vocabulary: `$name` /
/// `$N` for a text body, `matches.name` / `matches[N]` for JavaScript.
fn render_references(captures: &[Option<String>], language: ScriptLang) -> Vec<String> {
    captures
        .iter()
        .enumerate()
        .map(|(i, name)| match (name, language) {
            (Some(name), ScriptLang::Plaintext) => format!("${name}"),
            (Some(name), _) => format!("matches.{name}"),
            (None, ScriptLang::Plaintext) => format!("${}", i + 1),
            (None, _) => format!("matches[{}]", i + 1),
        })
        .collect()
}

/// The Try-it verdict for a Command miss, in the deck's words.
fn command_miss_verdict(name: &str, miss: &matchers::CommandMiss) -> (String, NodeStatus) {
    use matchers::{CommandMiss, TokenizeError};
    match miss {
        CommandMiss::Empty => (
            crate::i18n::t!("editor-enter-command"),
            NodeStatus::Disabled,
        ),
        CommandMiss::WrongFirstWord => (
            crate::i18n::t!("editor-wrong-first-word", "name" => name),
            NodeStatus::Disabled,
        ),
        CommandMiss::MissingRequired { name } => (
            crate::i18n::t!("editor-missing-arg", "name" => name.clone()),
            NodeStatus::Error,
        ),
        CommandMiss::Unclaimed { text } => (
            crate::i18n::t!("editor-unclaimed", "text" => text.clone()),
            NodeStatus::Disabled,
        ),
        CommandMiss::Tokenize(TokenizeError::UnterminatedQuote) => (
            crate::i18n::t!("editor-unterminated-quote"),
            NodeStatus::Error,
        ),
        CommandMiss::Tokenize(TokenizeError::UnbalancedBraces) => (
            crate::i18n::t!("editor-unbalanced-braces"),
            NodeStatus::Error,
        ),
    }
}

/// The Try-it field's raw-line simulation: `\e` means the ESC byte, so escape
/// sequences can be typed into the tester (`matching-logic.md` §6).
fn raw_of(test: &str) -> String {
    test.replace("\\e", "\x1b")
}

/// The ANSI-stripped subject normal matchers see.
fn plain_of(raw: &str) -> String {
    static ANSI: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new("\x1b\\[[0-9;]*[A-Za-z]").unwrap());
    ANSI.replace_all(raw, "").into_owned()
}

fn alias_verdict(pattern: &str, sample: &str) -> (String, NodeStatus) {
    if pattern.is_empty() {
        return (
            crate::i18n::t!("editor-verdict-no-regex"),
            NodeStatus::Disabled,
        );
    }
    match regex::Regex::new(pattern) {
        Err(e) => (
            crate::i18n::t!("editor-verdict-invalid-regex", "error" => e.to_string()),
            NodeStatus::Error,
        ),
        Ok(re) => {
            if sample.is_empty() {
                (
                    crate::i18n::t!("editor-enter-command"),
                    NodeStatus::Disabled,
                )
            } else if re.is_match(sample) {
                (crate::i18n::t!("editor-would-fire"), NodeStatus::Ok)
            } else {
                (crate::i18n::t!("editor-no-match"), NodeStatus::Disabled)
            }
        }
    }
}

/// Wraps a pane body in the standard padded, width-capped column.
pub(super) fn pane_scroll<'a>(body: Column<'a, Message, Theme>) -> Elem<'a> {
    container(body.max_width(860.0).width(Length::Fill))
        .padding(Padding {
            top: 26.0,
            bottom: 32.0,
            left: 30.0,
            right: 30.0,
        })
        .width(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use iced::advanced::Widget;
    use iced::advanced::widget::tree::Tree;

    use super::*;

    #[test]
    fn live_error_slot_keeps_following_input_focus_state() {
        let value = String::new();
        let valid = iced::widget::column![
            error_slot(None),
            text_input("pattern", &value).on_input(Message::SetName)
        ];
        let mut tree = Tree::new(&valid as &dyn Widget<Message, Theme, iced::Renderer>);

        type Paragraph = <iced::Renderer as iced::advanced::text::Renderer>::Paragraph;
        let input_state = tree.children[1]
            .state
            .downcast_mut::<iced::widget::text_input::State<Paragraph>>();
        input_state.focus();

        let invalid = iced::widget::column![
            error_slot(Some("invalid regular expression")),
            text_input("pattern", &value).on_input(Message::SetName)
        ];
        tree.diff(&invalid as &dyn Widget<Message, Theme, iced::Renderer>);

        let input_state = tree.children[1]
            .state
            .downcast_ref::<iced::widget::text_input::State<Paragraph>>();
        assert!(input_state.is_focused());
    }

    /// The fixtures §10 gutter-derivation table for regex sources.
    #[test]
    fn regex_gutters_derive_from_the_source_anchors() {
        assert_eq!(regex_loose_sides("^greet$"), (false, false));
        assert_eq!(regex_loose_sides("greet$"), (true, false));
        assert_eq!(regex_loose_sides("^greet"), (false, true));
        assert_eq!(regex_loose_sides("greet"), (true, true));
        // An escaped `$` is not an anchor.
        assert_eq!(regex_loose_sides(r"costs 5\$"), (true, true));
        assert_eq!(regex_loose_sides(""), (false, false));
        assert_eq!(regex_loose_sides("$"), (true, false));
    }

    /// Enter never reaches a one-line buffer, and pasted newlines flatten.
    #[test]
    fn single_line_fields_stay_single_line() {
        use iced::widget::text_editor::{Action, Content, Edit};

        let mut content = Content::new();
        perform_single_line(&mut content, Action::Edit(Edit::Insert('a')));
        perform_single_line(&mut content, Action::Edit(Edit::Enter));
        perform_single_line(
            &mut content,
            Action::Edit(Edit::Paste(Arc::new("b\r\nc\nd".to_string()))),
        );
        assert_eq!(single_line_text(&content), "ab c d");
    }
}
