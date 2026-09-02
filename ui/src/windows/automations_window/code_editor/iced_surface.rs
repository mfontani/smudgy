//! `iced-code-editor` adapter for writable automation documents.

use std::cell::RefCell;
use std::rc::Rc;

use iced::Task;
use iced_code_editor::{
    CodeEditor, ContextMenuEntry, LspClient, LspDocument, LspFormattingOptions, LspPosition,
    LspRange, LspTextChange, Message,
};
use smudgy_script::language_service::{
    CompletionItem, DocumentChanges, FormattingOptions, Language, TextChange, TextEdit,
    Utf16Position,
};

use super::{
    CompletionIntent, EditorSurface, HoverIntent, HoverUpdate, OverlayMetrics, ScalarPosition,
    SignatureHelpIntent, SurfacePoint, SurfaceUpdate,
};

/// Message emitted by the upstream editor widget.
pub(in crate::windows::automations_window) type IcedEditorMessage = Message;

const GO_TO_DEFINITION_ACTION: &str = "smudgy.go-to-definition";
const FORMAT_DOCUMENT_ACTION: &str = "smudgy.format-document";
const SHOW_COMPLETIONS_ACTION: &str = "smudgy.show-completions";

/// Writable code surface backed by upstream `iced-code-editor`.
///
/// Smudgy deliberately does not attach the editor's built-in process LSP client. The
/// Automations window owns document identity and the embedded-Deno service lifecycle, so
/// text changes flow through [`EditorSurface`] into that versioned protocol instead.
pub(in crate::windows::automations_window) struct IcedCodeEditorSurface {
    editor: CodeEditor,
    iced_theme: iced::Theme,
    theme_generation: u64,
    completion_requests: Rc<RefCell<Vec<LspPosition>>>,
    context_menu_position: Option<LspPosition>,
    #[cfg(test)]
    canvas_focus_recoveries: usize,
    #[cfg(test)]
    theme_applications: usize,
}

struct CompletionRequestProxy(Rc<RefCell<Vec<LspPosition>>>);

impl LspClient for CompletionRequestProxy {
    fn request_completion(&mut self, _document: &LspDocument, position: LspPosition) {
        self.0.borrow_mut().push(position);
    }
}

impl IcedCodeEditorSurface {
    /// Creates a writable surface with highlighting appropriate for `language`.
    pub(in crate::windows::automations_window) fn new(text: &str, language: Language) -> Self {
        let mut editor = CodeEditor::new(text, syntax_for(language));
        // These are Smudgy's editor defaults, not accidental inheritance from
        // whichever upstream release is currently pinned.
        editor.set_show_whitespace(false);
        editor.set_show_color_previews(true);
        editor.set_bracket_pair_colorization_enabled(true);
        editor.set_wrap_enabled(true);
        editor.set_font(crate::assets::fonts::GEIST_MONO_VF);
        editor.set_font_size(13.0, true);
        let theme_generation = crate::prefs::current().generation;
        let iced_theme = nested_iced_theme(&crate::prefs::app_theme());
        editor.set_theme(iced_code_editor::theme::from_iced_theme(&iced_theme));
        if super::supports_language_service(language) {
            editor.set_custom_context_menu_entries(vec![
                ContextMenuEntry::separator(),
                ContextMenuEntry::item(
                    GO_TO_DEFINITION_ACTION,
                    crate::i18n::t!("automation-code-go-to-definition"),
                ),
                ContextMenuEntry::item(
                    FORMAT_DOCUMENT_ACTION,
                    crate::i18n::t!("automation-code-format-document"),
                ),
                ContextMenuEntry::item(
                    SHOW_COMPLETIONS_ACTION,
                    crate::i18n::t!("automation-code-show-completions"),
                ),
            ]);
        }
        // The host controller owns authoritative full-snapshot synchronization.
        // This request-only client exists solely to reuse the upstream editor's
        // automatic completion triggers and cursor tracking.
        let completion_requests = Rc::new(RefCell::new(Vec::new()));
        if super::supports_language_service(language) {
            editor.attach_lsp(
                Box::new(CompletionRequestProxy(Rc::clone(&completion_requests))),
                request_document(language),
            );
        }

        Self {
            editor,
            iced_theme,
            theme_generation,
            completion_requests,
            context_menu_position: None,
            #[cfg(test)]
            canvas_focus_recoveries: 0,
            #[cfg(test)]
            theme_applications: 1,
        }
    }

    /// Renders the editor under an Iced theme subtree.
    ///
    /// `iced-code-editor` currently returns an element specialized to `iced::Theme`, while
    /// Smudgy uses its own theme type. `Themer` is Iced's supported adapter for exactly this
    /// kind of nested theme; the editor's own canvas colors remain controlled by its style.
    pub(in crate::windows::automations_window) fn view(
        &self,
    ) -> crate::theme::Element<'_, IcedEditorMessage> {
        iced::widget::Themer::new(Some(self.iced_theme.clone()), self.editor.view()).into()
    }

    pub(in crate::windows::automations_window) fn explicit_completion_message() -> IcedEditorMessage
    {
        Message::CustomContextMenuAction(SHOW_COMPLETIONS_ACTION.to_owned())
    }

    pub(super) fn overlay_metrics(&self) -> OverlayMetrics {
        OverlayMetrics {
            viewport_width: self.editor.viewport_width(),
            viewport_height: self.editor.viewport_height(),
            viewport_scroll: self.editor.viewport_scroll(),
            line_height: self.editor.line_height(),
            char_width: self.editor.char_width(),
        }
    }

    fn restore_canvas_focus(&mut self) {
        self.editor.request_focus();
        let _ = self.editor.update(&Message::CanvasFocusGained);
        #[cfg(test)]
        {
            self.canvas_focus_recoveries = self.canvas_focus_recoveries.saturating_add(1);
        }
    }

    /// Re-resolves the nested Iced theme and the editor's separately stored
    /// canvas palette after Smudgy publishes a new appearance generation.
    pub(super) fn sync_theme_from_prefs(&mut self) {
        let generation = crate::prefs::current().generation;
        if generation == self.theme_generation {
            return;
        }
        self.apply_theme(generation, &crate::prefs::app_theme());
    }

    fn apply_theme(&mut self, generation: u64, theme: &crate::theme::Theme) {
        if generation == self.theme_generation {
            return;
        }
        let iced_theme = nested_iced_theme(theme);
        if iced_theme.palette() == self.iced_theme.palette() {
            // Terminal font and layout preferences share this generation with
            // palette changes. Record that generation without making upstream
            // discard and rebuild every editor cache for an identical palette.
            self.theme_generation = generation;
            return;
        }
        self.editor
            .set_theme(iced_code_editor::theme::from_iced_theme(&iced_theme));
        self.iced_theme = iced_theme;
        self.theme_generation = generation;
        #[cfg(test)]
        {
            self.theme_applications = self.theme_applications.saturating_add(1);
        }
    }

    #[cfg(test)]
    fn syntax(&self) -> &str {
        self.editor.syntax()
    }

    #[cfg(test)]
    pub(super) fn cursor_position(&self) -> (usize, usize) {
        self.editor.cursor_position()
    }
}

impl EditorSurface for IcedCodeEditorSurface {
    type Message = IcedEditorMessage;
    type Effect = Task<IcedEditorMessage>;

    fn content(&self) -> String {
        self.editor.content()
    }

    fn update(&mut self, message: &Self::Message) -> SurfaceUpdate<Self::Effect> {
        // The daemon also fans appearance changes out eagerly. This defensive
        // check covers any future preference publisher before the next input.
        self.sync_theme_from_prefs();
        // The upstream widget does not expose its buffer revision. Comparing snapshots is
        // intentionally conservative: every real edit reaches the service, while cursor,
        // focus, scroll, and dialog messages produce no change command. Automation source
        // files are configuration-sized and this path runs only while their editor is open.
        // The upstream buffer stores carriage returns as ordinary characters,
        // so a CRLF clipboard would leave a stray `\r` on every pasted line.
        // Documents are LF-only; normalize before the buffer sees the text.
        let normalized;
        let message = match message {
            Message::Paste(text) if text.contains('\r') => {
                normalized = Message::Paste(normalize_line_endings(text));
                &normalized
            }
            _ => message,
        };
        let before = self.editor.content();
        let before_cursor = self.editor.cursor_position();
        let effect = self.editor.update(message);
        let after = self.editor.content();
        let after_cursor = self.editor.cursor_position();
        let changes = (after != before).then(|| DocumentChanges {
            changes: vec![TextChange {
                range: None,
                text: after,
            }],
        });
        // Deno/TypeScript identifiers commonly begin with `$`. Upstream's
        // automatic trigger covers alphanumeric, `_`, and `.`, so bridge this
        // one identifier character through the same request-only proxy.
        if changes.is_some() && matches!(message, Message::CharacterInput('$')) {
            self.editor.lsp_request_completion();
        }

        let automatic_completion = self
            .completion_requests
            .borrow_mut()
            .drain(..)
            .next_back()
            .map(|position| ScalarPosition {
                line: position.line,
                character: position.character,
            });
        if let Message::ContextMenuRequested(point) = message {
            self.context_menu_position = self.editor.lsp_position_at_point(*point);
        }
        let definition = match message {
            Message::JumpClick(point) => self.editor.lsp_position_at_point(*point),
            Message::CustomContextMenuAction(action) if action == GO_TO_DEFINITION_ACTION => {
                self.context_menu_position.take().or_else(|| {
                    let (line, character) = self.editor.cursor_position();
                    Some(LspPosition {
                        line: u32::try_from(line).unwrap_or(u32::MAX),
                        character: u32::try_from(character).unwrap_or(u32::MAX),
                    })
                })
            }
            _ => None,
        }
        .map(|position| ScalarPosition {
            line: position.line,
            character: position.character,
        });
        let formatting = matches!(
            message,
            Message::CustomContextMenuAction(action) if action == FORMAT_DOCUMENT_ACTION
        )
        .then(|| {
            let options = LspFormattingOptions::from(self.editor.indent_style());
            FormattingOptions {
                tab_size: u8::try_from(options.tab_size).unwrap_or(4),
                insert_spaces: options.insert_spaces,
            }
        });
        if formatting.is_some() {
            self.context_menu_position = None;
        }

        let explicit_completion = matches!(
            message,
            Message::CustomContextMenuAction(action) if action == SHOW_COMPLETIONS_ACTION
        )
        .then(|| {
            self.restore_canvas_focus();
            let (line, character) = self.editor.cursor_position();
            ScalarPosition {
                line: u32::try_from(line).unwrap_or(u32::MAX),
                character: u32::try_from(character).unwrap_or(u32::MAX),
            }
        });
        let completion = explicit_completion
            .or(automatic_completion)
            .map(|position| {
                let anchor = self.editor.cursor_screen_position().unwrap_or_default();
                CompletionIntent {
                    position,
                    anchor: SurfacePoint {
                        x: anchor.x,
                        y: anchor.y,
                    },
                }
            });
        let hover = match message {
            Message::MouseHover(point) => self.editor.lsp_hover_anchor_at_point(*point).map_or(
                HoverUpdate::Leave,
                |(position, anchor)| {
                    HoverUpdate::At(HoverIntent {
                        position: ScalarPosition {
                            line: position.line,
                            character: position.character,
                        },
                        anchor: SurfacePoint {
                            x: anchor.x,
                            y: anchor.y,
                        },
                    })
                },
            ),
            Message::CanvasFocusLost => HoverUpdate::Clear,
            _ => HoverUpdate::Unchanged,
        };
        let signature_help = (changes.is_some() || before_cursor != after_cursor).then(|| {
            let anchor = self.editor.cursor_screen_position().unwrap_or_default();
            SignatureHelpIntent {
                position: ScalarPosition {
                    line: u32::try_from(after_cursor.0).unwrap_or(u32::MAX),
                    character: u32::try_from(after_cursor.1).unwrap_or(u32::MAX),
                },
                anchor: SurfacePoint {
                    x: anchor.x,
                    y: anchor.y,
                },
                starts_new_lifecycle: matches!(message, Message::CharacterInput('(' | ',')),
            }
        });
        // Automations editors retain upstream's default wrapping, so a
        // HorizontalScrolled notification has no effective x offset and is as
        // passive as vertical scrolling. If no-wrap editing is enabled later,
        // the host will need an upstream public x offset or must dismiss these
        // content-x anchored overlays on horizontal scroll.
        let semantic_context_changed = changes.is_some()
            || before_cursor != after_cursor
            || !matches!(
                message,
                Message::Tick
                    | Message::MouseHover(_)
                    | Message::MouseRelease
                    | Message::Scrolled(_)
                    | Message::HorizontalScrolled(_)
                    | Message::CanvasFocusGained
                    | Message::CanvasFocusLost
                    | Message::Copy
                    | Message::ContextMenuRequested(_)
                    | Message::CustomContextMenuAction(_)
            );

        SurfaceUpdate {
            effect,
            changes,
            completion,
            signature_help,
            hover,
            semantic_context_changed,
            definition,
            formatting,
        }
    }

    fn apply_completion(&mut self, item: &CompletionItem) -> SurfaceUpdate<Self::Effect> {
        let before = self.editor.content();
        let mut effect = Task::none();
        self.restore_canvas_focus();

        if let Some(primary) = &item.primary_edit {
            let mut source_edits = Vec::with_capacity(1 + item.additional_edits.len());
            source_edits.push(primary);
            source_edits.extend(&item.additional_edits);
            let converted = source_edits
                .into_iter()
                .map(|edit| {
                    protocol_edit(edit.range.start, edit.range.end, &edit.new_text, &before)
                })
                .collect::<Option<Vec<_>>>();
            if let Some(edits) = converted
                && self.editor.apply_lsp_text_edits(&edits)
                && let Some(start) = protocol_position(primary.range.start, &before)
            {
                let (line, column) = position_after_text(start, &primary.new_text);
                effect = self.editor.update(&Message::GotoPosition(line, column));
            }
        } else {
            let insertion = item
                .insert_text
                .as_deref()
                .unwrap_or(&item.label)
                .to_owned();
            effect = self.editor.update(&Message::Paste(insertion));
        }

        let after = self.editor.content();
        let after_cursor = self.editor.cursor_position();
        let changes = (after != before).then(|| DocumentChanges {
            changes: vec![TextChange {
                range: None,
                text: after,
            }],
        });
        let semantic_context_changed = changes.is_some();
        let signature_help = changes.is_some().then(|| {
            let anchor = self.editor.cursor_screen_position().unwrap_or_default();
            SignatureHelpIntent {
                position: ScalarPosition {
                    line: u32::try_from(after_cursor.0).unwrap_or(u32::MAX),
                    character: u32::try_from(after_cursor.1).unwrap_or(u32::MAX),
                },
                anchor: SurfacePoint {
                    x: anchor.x,
                    y: anchor.y,
                },
                starts_new_lifecycle: false,
            }
        });
        SurfaceUpdate {
            effect,
            changes,
            completion: None,
            signature_help,
            hover: HoverUpdate::Clear,
            semantic_context_changed,
            definition: None,
            formatting: None,
        }
    }

    fn apply_text_edits(&mut self, edits: &[TextEdit]) -> Result<Option<DocumentChanges>, ()> {
        let before = self.editor.content();
        let converted = edits
            .iter()
            .map(|edit| {
                let range = edit.range.to_byte_range(&before).map_err(|_| ())?;
                if before[range] == edit.new_text {
                    Ok(None)
                } else {
                    protocol_edit(edit.range.start, edit.range.end, &edit.new_text, &before)
                        .map(Some)
                        .ok_or(())
                }
            })
            .collect::<Result<Vec<_>, ()>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if converted.is_empty() {
            return Ok(None);
        }
        if !self.editor.apply_lsp_text_edits(&converted) {
            return Err(());
        }
        let after = self.editor.content();
        Ok((after != before).then(|| DocumentChanges {
            changes: vec![TextChange {
                range: None,
                text: after,
            }],
        }))
    }

    fn goto_position(&mut self, position: ScalarPosition) -> Self::Effect {
        self.restore_canvas_focus();
        self.editor.update(&Message::GotoPosition(
            usize::try_from(position.line).unwrap_or(usize::MAX),
            usize::try_from(position.character).unwrap_or(usize::MAX),
        ))
    }

    fn reset(&mut self, text: &str, language: Language) -> Self::Effect {
        self.completion_requests.borrow_mut().clear();
        self.context_menu_position = None;
        self.editor.set_syntax(syntax_for(language));
        let task = self.editor.reset(text);
        if super::supports_language_service(language) {
            self.editor.lsp_open_document(request_document(language));
            self.editor.set_custom_context_menu_entries(vec![
                ContextMenuEntry::separator(),
                ContextMenuEntry::item(
                    GO_TO_DEFINITION_ACTION,
                    crate::i18n::t!("automation-code-go-to-definition"),
                ),
                ContextMenuEntry::item(
                    FORMAT_DOCUMENT_ACTION,
                    crate::i18n::t!("automation-code-format-document"),
                ),
                ContextMenuEntry::item(
                    SHOW_COMPLETIONS_ACTION,
                    crate::i18n::t!("automation-code-show-completions"),
                ),
            ]);
        } else {
            self.editor.set_custom_context_menu_entries(Vec::new());
        }
        task
    }

    fn is_modified(&self) -> bool {
        self.editor.is_modified()
    }

    fn mark_saved(&mut self) {
        self.editor.mark_saved();
    }

    fn request_focus(&self) {
        self.editor.request_focus();
    }

    fn lose_focus(&mut self) {
        self.editor.lose_focus();
    }

    fn is_dialog_open(&self) -> bool {
        self.editor.is_dialog_open()
    }
}

/// Builds the native Iced theme used by iced-code-editor's own dialogs and by
/// its canvas-style resolver. Smudgy's `Base::palette` intentionally uses a
/// button foreground for `primary`, so map the editor accent explicitly.
fn nested_iced_theme(theme: &crate::theme::Theme) -> iced::Theme {
    let palette = iced::theme::Palette {
        background: theme.styles.general.container_background,
        text: theme.styles.text.normal,
        primary: theme.styles.general.accent,
        success: theme.styles.text.success,
        warning: theme.styles.text.error,
        danger: theme.styles.text.error,
    };
    iced::Theme::custom_with_fn("Smudgy code editor", palette, |palette| {
        let mut extended = iced::theme::palette::Extended::generate(palette);
        // iced-code-editor independently selects its syntax and rainbow-
        // bracket palettes with this weighted luminance test. Match its
        // explicit tone branches (gutters, line numbers, and guides) to that
        // canvas mode. Keep Iced's contrast-aware generation for weak/strong
        // pairs instead of copying its private policy here.
        extended.is_dark = editor_background_is_dark(palette.background);
        extended
    })
}

fn editor_background_is_dark(background: iced::Color) -> bool {
    0.2126 * background.r + 0.7152 * background.g + 0.0722 * background.b < 0.5
}

fn request_document(language: Language) -> LspDocument {
    LspDocument::new("smudgy-request-proxy:///active", language_id(language))
}

fn protocol_edit(
    start: Utf16Position,
    end: Utf16Position,
    text: &str,
    source: &str,
) -> Option<LspTextChange> {
    Some(LspTextChange {
        range: LspRange {
            start: protocol_position(start, source)?,
            end: protocol_position(end, source)?,
        },
        text: text.to_owned(),
    })
}

/// Rewrites CRLF and lone CR line breaks as LF.
fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn protocol_position(position: Utf16Position, source: &str) -> Option<LspPosition> {
    let byte_offset = position.to_byte_offset(source).ok()?;
    let line_start = source[..byte_offset]
        .rfind('\n')
        .map_or(0, |offset| offset.saturating_add(1));
    Some(LspPosition {
        line: position.line,
        character: u32::try_from(source[line_start..byte_offset].chars().count()).ok()?,
    })
}

fn position_after_text(start: LspPosition, text: &str) -> (usize, usize) {
    let line_delta = text.chars().filter(|character| *character == '\n').count();
    let line = usize::try_from(start.line)
        .unwrap_or(usize::MAX)
        .saturating_add(line_delta);
    let column = if line_delta == 0 {
        usize::try_from(start.character)
            .unwrap_or(usize::MAX)
            .saturating_add(text.chars().count())
    } else {
        text.rsplit_once('\n')
            .map_or(0, |(_, tail)| tail.chars().count())
    };
    (line, column)
}

const fn language_id(language: Language) -> &'static str {
    match language {
        Language::JavaScript | Language::JavaScriptReact => "javascript",
        Language::TypeScript | Language::TypeScriptReact => "typescript",
        Language::Json => "json",
        Language::PlainText => "plaintext",
    }
}

const fn syntax_for(language: Language) -> &'static str {
    match language {
        Language::JavaScript => "js",
        Language::TypeScript => "ts",
        Language::JavaScriptReact => "jsx",
        Language::TypeScriptReact => "tsx",
        Language::Json => "json",
        Language::PlainText => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smudgy_script::language_service::{
        CompletionItemId, CompletionKind, InsertTextFormat, TextEdit, Utf16Range,
    };

    // iced-code-editor currently owns one process-global focus id. Serialize
    // only tests that must claim that id so parallel test execution cannot
    // redirect a synthetic keystroke into a neighboring surface.
    static EDITOR_FOCUS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn editor_focus_test_guard() -> std::sync::MutexGuard<'static, ()> {
        EDITOR_FOCUS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn edit_emits_full_document_change_and_keeps_editor_authoritative() {
        let mut surface = IcedCodeEditorSurface::new("const n = 1;", Language::JavaScript);

        let update = surface.update(&Message::Paste("// note\n".to_owned()));

        assert_eq!(surface.content(), "// note\nconst n = 1;");
        assert!(surface.is_modified());
        assert_eq!(
            update.changes,
            Some(DocumentChanges {
                changes: vec![TextChange {
                    range: None,
                    text: "// note\nconst n = 1;".to_owned(),
                }],
            })
        );
    }

    #[test]
    fn navigation_does_not_emit_a_text_change() {
        let mut surface = IcedCodeEditorSurface::new("let n = 1;", Language::JavaScript);

        let update = surface.update(&Message::CtrlEnd);

        assert!(update.changes.is_none());
        assert_eq!(surface.content(), "let n = 1;");
    }

    #[test]
    fn reset_rebinds_content_and_syntax_without_marking_modified() {
        let mut surface = IcedCodeEditorSurface::new("let n = 1;", Language::JavaScript);
        let _ = surface.update(&Message::CharacterInput('x'));

        let _ = surface.reset("const n: number = 1;", Language::TypeScript);

        assert_eq!(surface.content(), "const n: number = 1;");
        assert_eq!(surface.syntax(), "ts");
        assert!(!surface.is_modified());
    }

    #[test]
    fn language_modes_map_to_upstream_syntax_keys() {
        assert_eq!(syntax_for(Language::JavaScript), "js");
        assert_eq!(syntax_for(Language::TypeScript), "ts");
        assert_eq!(syntax_for(Language::JavaScriptReact), "jsx");
        assert_eq!(syntax_for(Language::TypeScriptReact), "tsx");
        assert_eq!(syntax_for(Language::Json), "json");
        assert_eq!(syntax_for(Language::PlainText), "text");
    }

    #[test]
    fn smudgy_editor_defaults_hide_whitespace_and_retain_requested_features() {
        let surface = IcedCodeEditorSurface::new("const color = '#ff00aa';", Language::TypeScript);

        assert!(!surface.editor.show_whitespace());
        assert!(surface.editor.show_color_previews());
        assert!(surface.editor.bracket_pair_colorization_enabled());
        assert!(surface.editor.wrap_enabled());
        assert!(surface.editor.line_numbers_enabled());
        assert!(surface.editor.show_indent_guides());
        assert!(surface.editor.bracket_match_highlight_enabled());
        assert!(surface.editor.auto_indent_enabled());
        assert!(surface.editor.auto_close_brackets());
    }

    #[test]
    fn nested_editor_theme_preserves_smudgy_palette_roles_and_tone() {
        let mut dark = crate::theme::smudgy();
        dark.styles.general.container_background = iced::Color::from_rgb8(8, 12, 18);
        dark.styles.general.accent = iced::Color::from_rgb8(120, 80, 220);
        dark.styles.text.normal = iced::Color::from_rgb8(238, 240, 244);
        dark.styles.text.success = iced::Color::from_rgb8(50, 180, 90);
        dark.styles.text.error = iced::Color::from_rgb8(220, 70, 80);

        let nested_dark = nested_iced_theme(&dark);
        let dark_palette = nested_dark.palette();
        assert_eq!(
            dark_palette.background,
            dark.styles.general.container_background
        );
        assert_eq!(dark_palette.text, dark.styles.text.normal);
        assert_eq!(dark_palette.primary, dark.styles.general.accent);
        assert_eq!(dark_palette.success, dark.styles.text.success);
        assert_eq!(dark_palette.warning, dark.styles.text.error);
        assert_eq!(dark_palette.danger, dark.styles.text.error);
        assert!(nested_dark.extended_palette().is_dark);
        let dark_style = iced_code_editor::theme::from_iced_theme(&nested_dark);
        assert_eq!(
            dark_style.background,
            dark.styles.general.container_background
        );
        assert_eq!(dark_style.text_color, dark.styles.text.normal);

        let mut light = crate::theme::smudgy();
        light.styles.general.container_background = iced::Color::from_rgb8(248, 246, 240);
        light.styles.general.accent = iced::Color::from_rgb8(30, 90, 190);
        light.styles.text.normal = iced::Color::from_rgb8(25, 28, 32);
        let nested_light = nested_iced_theme(&light);
        assert!(!nested_light.extended_palette().is_dark);
        let light_style = iced_code_editor::theme::from_iced_theme(&nested_light);
        assert_eq!(
            light_style.background,
            light.styles.general.container_background
        );
        assert_eq!(light_style.text_color, light.styles.text.normal);
        assert_ne!(dark_style.gutter_background, light_style.gutter_background);
        assert_ne!(dark_style.line_number_color, light_style.line_number_color);

        // Iced's native OKLCH classifier considers pure red light, while the
        // pinned editor's weighted luminance classifier considers it dark.
        // The explicit editor-style branches must follow the canvas mode;
        // Iced remains responsible for contrast-aware weak/strong pairs.
        let mut saturated = crate::theme::smudgy();
        saturated.styles.general.container_background = iced::Color::from_rgb(1.0, 0.0, 0.0);
        let nested_saturated = nested_iced_theme(&saturated);
        assert!(editor_background_is_dark(
            saturated.styles.general.container_background
        ));
        let native = iced::theme::palette::Extended::generate(nested_saturated.palette());
        let aligned = nested_saturated.extended_palette();
        assert!(!native.is_dark);
        assert!(aligned.is_dark);
        assert_eq!(aligned.background, native.background);
        assert_eq!(aligned.primary, native.primary);
        assert_eq!(aligned.secondary, native.secondary);
        assert_eq!(aligned.success, native.success);
        assert_eq!(aligned.warning, native.warning);
        assert_eq!(aligned.danger, native.danger);
    }

    #[test]
    fn editor_theme_sync_is_generation_gated() {
        let mut surface = IcedCodeEditorSurface::new("const n = 1;", Language::TypeScript);
        let initial_generation = surface.theme_generation;
        let initial_background = surface.iced_theme.palette().background;
        let mut light = crate::theme::smudgy();
        light.styles.general.container_background = iced::Color::from_rgb8(250, 248, 242);
        light.styles.text.normal = iced::Color::from_rgb8(20, 24, 28);

        surface.apply_theme(initial_generation, &light);
        assert_eq!(surface.iced_theme.palette().background, initial_background);
        assert_eq!(surface.theme_applications, 1);

        let next_generation = initial_generation.wrapping_add(1);
        surface.apply_theme(next_generation, &light);
        assert_eq!(surface.theme_generation, next_generation);
        assert_eq!(
            surface.iced_theme.palette().background,
            light.styles.general.container_background
        );
        assert_eq!(surface.theme_applications, 2);
    }

    #[test]
    fn editor_theme_sync_skips_cache_reset_for_an_equivalent_palette() {
        let mut surface = IcedCodeEditorSurface::new("const n = 1;", Language::TypeScript);
        let initial_generation = surface.theme_generation;
        let initial_palette = surface.iced_theme.palette();

        surface.apply_theme(
            initial_generation.wrapping_add(1),
            &crate::prefs::app_theme(),
        );

        assert_eq!(surface.theme_generation, initial_generation.wrapping_add(1));
        assert_eq!(surface.iced_theme.palette(), initial_palette);
        assert_eq!(surface.theme_applications, 1);
    }

    #[test]
    fn upstream_completion_callback_reports_the_current_scalar_cursor() {
        let mut surface = IcedCodeEditorSurface::new("const n = 1;\ncon", Language::TypeScript);
        let _ = surface.update(&Message::GotoPosition(1, 3));
        surface.editor.lsp_request_completion();

        let update = surface.update(&Message::Tick);

        assert_eq!(surface.content(), "const n = 1;\ncon");
        assert_eq!(
            update.completion.map(|intent| intent.position),
            Some(ScalarPosition {
                line: 1,
                character: 3,
            })
        );
    }

    #[test]
    fn focused_character_input_emits_automatic_completion_intent() {
        let _focus_guard = editor_focus_test_guard();
        let mut surface = IcedCodeEditorSurface::new("con", Language::TypeScript);
        surface.request_focus();
        let _ = surface.update(&Message::CanvasFocusGained);
        let _ = surface.update(&Message::GotoPosition(0, 3));
        // The upstream focus id is process-global; reclaim it immediately
        // before input so parallel editor tests cannot own the keystroke.
        surface.request_focus();

        let update = surface.update(&Message::CharacterInput('s'));

        assert_eq!(surface.content(), "cons");
        assert_eq!(
            update.completion.map(|intent| intent.position),
            Some(ScalarPosition {
                line: 0,
                character: 4,
            })
        );
        assert!(update.semantic_context_changed);

        surface.request_focus();
        let dollar = surface.update(&Message::CharacterInput('$'));
        assert_eq!(surface.content(), "cons$");
        assert_eq!(
            dollar.completion.map(|intent| intent.position),
            Some(ScalarPosition {
                line: 0,
                character: 5,
            })
        );
    }

    #[test]
    fn opening_parenthesis_emits_signature_help_at_the_post_edit_cursor() {
        let _focus_guard = editor_focus_test_guard();
        let mut surface = IcedCodeEditorSurface::new("send", Language::TypeScript);
        surface.request_focus();
        let _ = surface.update(&Message::CanvasFocusGained);
        let _ = surface.update(&Message::GotoPosition(0, 4));
        surface.request_focus();

        let update = surface.update(&Message::CharacterInput('('));

        assert_eq!(surface.content(), "send()");
        assert_eq!(
            update.signature_help,
            Some(SignatureHelpIntent {
                position: ScalarPosition {
                    line: 0,
                    character: 5,
                },
                anchor: update.signature_help.unwrap().anchor,
                starts_new_lifecycle: true,
            })
        );
    }

    #[test]
    fn passive_tick_preserves_semantic_context_and_pointer_hover_has_word_anchor() {
        let mut surface = IcedCodeEditorSurface::new("console", Language::TypeScript);
        let _ = surface.update(&Message::GotoPosition(0, 4));
        let tick = surface.update(&Message::Tick);
        assert!(!tick.semantic_context_changed);
        assert!(tick.signature_help.is_none());

        let point = surface.editor.cursor_screen_position().unwrap();
        let hover = surface.update(&Message::MouseHover(point));
        assert!(!hover.semantic_context_changed);
        assert!(matches!(
            hover.hover,
            HoverUpdate::At(HoverIntent {
                position: ScalarPosition {
                    line: 0,
                    character: 0,
                },
                ..
            })
        ));
    }

    #[test]
    fn host_actions_report_definition_position_and_indent_preferences() {
        let _focus_guard = editor_focus_test_guard();
        let mut surface = IcedCodeEditorSurface::new("🙂value", Language::TypeScript);
        let _ = surface.update(&Message::GotoPosition(0, 2));

        let definition = surface.update(&Message::CustomContextMenuAction(
            GO_TO_DEFINITION_ACTION.to_owned(),
        ));
        assert_eq!(
            definition.definition,
            Some(ScalarPosition {
                line: 0,
                character: 2,
            })
        );
        assert!(definition.changes.is_none());

        surface
            .editor
            .set_indent_style(iced_code_editor::IndentStyle::Spaces(2));
        let formatting = surface.update(&Message::CustomContextMenuAction(
            FORMAT_DOCUMENT_ACTION.to_owned(),
        ));
        assert_eq!(
            formatting.formatting,
            Some(FormattingOptions {
                tab_size: 2,
                insert_spaces: true,
            })
        );
        assert!(formatting.changes.is_none());

        let recoveries = surface.canvas_focus_recoveries;
        let completion = surface.update(&Message::CustomContextMenuAction(
            SHOW_COMPLETIONS_ACTION.to_owned(),
        ));
        assert_eq!(
            completion.completion.map(|intent| intent.position),
            Some(ScalarPosition {
                line: 0,
                character: 2,
            })
        );
        assert_eq!(surface.canvas_focus_recoveries, recoveries + 1);
    }

    #[test]
    fn pasted_carriage_returns_become_line_feeds() {
        let _focus_guard = editor_focus_test_guard();
        let mut surface = IcedCodeEditorSurface::new("", Language::TypeScript);
        surface.request_focus();
        let _ = surface.update(&Message::CanvasFocusGained);
        surface.request_focus();

        let update = surface.update(&Message::Paste("a\r\nb\rc\n".to_owned()));

        assert_eq!(surface.content(), "a\nb\nc\n");
        assert_eq!(
            update
                .changes
                .map(|changes| changes.changes[0].text.clone())
                .as_deref(),
            Some("a\nb\nc\n")
        );
    }

    #[test]
    fn goto_position_restores_editor_focus_before_moving_the_caret() {
        let _focus_guard = editor_focus_test_guard();
        let mut surface = IcedCodeEditorSurface::new("first\nsecond", Language::TypeScript);
        surface.lose_focus();
        let recoveries = surface.canvas_focus_recoveries;

        let _ = surface.goto_position(ScalarPosition {
            line: 1,
            character: 3,
        });

        assert_eq!(surface.canvas_focus_recoveries, recoveries + 1);
        assert_eq!(surface.editor.cursor_position(), (1, 3));
    }

    #[test]
    fn formatting_edits_convert_utf16_and_undo_as_one_transaction() {
        let original = "🙂a=1;\nb=2;";
        let mut surface = IcedCodeEditorSurface::new(original, Language::TypeScript);
        let edits = [
            TextEdit {
                range: Utf16Range {
                    start: Utf16Position {
                        line: 0,
                        character: 2,
                    },
                    end: Utf16Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "a = 1".to_owned(),
            },
            TextEdit {
                range: Utf16Range {
                    start: Utf16Position {
                        line: 1,
                        character: 0,
                    },
                    end: Utf16Position {
                        line: 1,
                        character: 3,
                    },
                },
                new_text: "b = 2".to_owned(),
            },
        ];

        let changes = surface
            .apply_text_edits(&edits)
            .expect("valid simultaneous edits");

        assert!(changes.is_some());
        assert_eq!(surface.content(), "🙂a = 1;\nb = 2;");
        let undo = surface.update(&Message::Undo);
        assert_eq!(surface.content(), original);
        assert!(
            undo.changes.is_some(),
            "one undo must revert the whole reply"
        );

        let no_op = surface
            .apply_text_edits(&[TextEdit {
                range: Utf16Range {
                    start: Utf16Position {
                        line: 0,
                        character: 2,
                    },
                    end: Utf16Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "a=1".to_owned(),
            }])
            .expect("in-bounds no-op");
        assert!(no_op.is_none());
        assert_eq!(surface.content(), original);
    }

    #[test]
    fn completion_primary_edit_converts_utf16_and_replaces_the_prefix() {
        let _focus_guard = editor_focus_test_guard();
        let mut surface = IcedCodeEditorSurface::new("🙂cons", Language::TypeScript);
        let item = CompletionItem {
            id: CompletionItemId::new(1).unwrap(),
            label: "console".to_owned(),
            detail: None,
            documentation: None,
            kind: CompletionKind::Variable,
            deprecated: false,
            filter_text: None,
            sort_text: None,
            insert_text: Some("console".to_owned()),
            insert_text_format: InsertTextFormat::PlainText,
            primary_edit: Some(TextEdit {
                range: Utf16Range {
                    start: Utf16Position {
                        line: 0,
                        character: 2,
                    },
                    end: Utf16Position {
                        line: 0,
                        character: 6,
                    },
                },
                new_text: "console".to_owned(),
            }),
            additional_edits: Vec::new(),
        };

        let update = surface.apply_completion(&item);

        assert_eq!(surface.content(), "🙂console");
        assert!(update.changes.is_some());
        assert_eq!(surface.editor.cursor_position(), (0, 8));
    }
}
