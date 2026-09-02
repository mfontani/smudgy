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
    SurfacePoint, SurfaceUpdate,
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
    completion_requests: Rc<RefCell<Vec<LspPosition>>>,
    context_menu_position: Option<LspPosition>,
    #[cfg(test)]
    canvas_focus_recoveries: usize,
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
        editor.set_font(crate::assets::fonts::GEIST_MONO_VF);
        editor.set_font_size(13.0, true);
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
            completion_requests,
            context_menu_position: None,
            #[cfg(test)]
            canvas_focus_recoveries: 0,
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
        iced::widget::Themer::new(None::<iced::Theme>, self.editor.view()).into()
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
        // The upstream widget does not expose its buffer revision. Comparing snapshots is
        // intentionally conservative: every real edit reaches the service, while cursor,
        // focus, scroll, and dialog messages produce no change command. Automation source
        // files are configuration-sized and this path runs only while their editor is open.
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
                HoverUpdate::Clear,
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
        let changes = (after != before).then(|| DocumentChanges {
            changes: vec![TextChange {
                range: None,
                text: after,
            }],
        });
        let semantic_context_changed = changes.is_some();
        SurfaceUpdate {
            effect,
            changes,
            completion: None,
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
    fn passive_tick_preserves_semantic_context_and_pointer_hover_has_word_anchor() {
        let mut surface = IcedCodeEditorSurface::new("console", Language::TypeScript);
        let _ = surface.update(&Message::GotoPosition(0, 4));
        let tick = surface.update(&Message::Tick);
        assert!(!tick.semantic_context_changed);

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
    fn goto_position_restores_editor_focus_before_moving_the_caret() {
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
