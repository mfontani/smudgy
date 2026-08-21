//! The pattern-field syntax highlighter (`matching-logic.md` §9): real styled
//! runs inside an editable [`iced::widget::text_editor`], with a true caret —
//! never a styled overlay that can drift out of sync with the text.
//!
//! The scanners here are display-only. They deliberately do not re-derive
//! match semantics — compiling and matching stay in
//! `smudgy_core::models::matchers` — they only mark where the accent runs go.

use std::ops::Range;

use iced::advanced::text::highlighter::Highlighter;

/// One highlighted run. The editors map these to theme colors at draw time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// A `{hole}` span — the capture accent.
    Hole,
    /// A bare `*` wildcard — the accent at reduced strength.
    Wildcard,
    /// A `/…/` regex island inside a Simple pattern.
    Island,
    /// A `(?<name>` group opener in a regex source.
    GroupOpen,
    /// A `\e` or `\x1b` escape in a regex source.
    Escape,
    /// A `$ref` the current matcher provides (send-text body).
    KnownRef,
    /// A `$ref` nothing captures (send-text body).
    UnknownRef,
}

/// Which grammar a field holds, as the highlighter's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSyntax {
    /// A Simple pattern: `{holes}`, `*`, `/islands/`.
    Pattern,
    /// A regex source: group openers and escapes get the accent.
    Regex,
    /// The send-text action body; `known` is every reference the matcher
    /// provides, rendered (`$name`, `$1`, `$0`).
    SendText { known: Vec<String> },
}

/// A per-line scanner for one [`FieldSyntax`]. Stateless across lines: each
/// line is highlighted from scratch, which is exactly right for fields one
/// line tall.
pub struct PatternHighlighter {
    syntax: FieldSyntax,
    current_line: usize,
}

impl Highlighter for PatternHighlighter {
    type Settings = FieldSyntax;
    type Highlight = Token;
    type Iterator<'a> = std::vec::IntoIter<(Range<usize>, Token)>;

    fn new(settings: &Self::Settings) -> Self {
        Self {
            syntax: settings.clone(),
            current_line: 0,
        }
    }

    fn update(&mut self, new_settings: &Self::Settings) {
        self.syntax = new_settings.clone();
        self.current_line = 0;
    }

    fn change_line(&mut self, line: usize) {
        self.current_line = self.current_line.min(line);
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        self.current_line += 1;
        let spans = match &self.syntax {
            FieldSyntax::Pattern => scan_pattern(line),
            FieldSyntax::Regex => scan_regex(line),
            FieldSyntax::SendText { known } => scan_send_text(line, known),
        };
        spans.into_iter()
    }

    fn current_line(&self) -> usize {
        self.current_line
    }
}

/// Marks `{holes}`, `*` wildcards, and `/…/` islands in a Simple pattern.
/// Mirrors the compiler's tokenization shape: an unclosed brace or an unpaired
/// slash is literal text; a backslash inside an island escapes its closing
/// delimiter.
pub fn scan_pattern(line: &str) -> Vec<(Range<usize>, Token)> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => match line[i..].find('}') {
                Some(rel) => {
                    spans.push((i..i + rel + 1, Token::Hole));
                    i += rel + 1;
                }
                None => i += 1,
            },
            b'*' => {
                spans.push((i..i + 1, Token::Wildcard));
                i += 1;
            }
            b'/' => {
                let mut j = i + 1;
                let mut close = None;
                while j < bytes.len() {
                    match bytes[j] {
                        b'\\' => j += 2,
                        b'/' => {
                            close = Some(j);
                            break;
                        }
                        _ => j += 1,
                    }
                }
                match close {
                    Some(j) => {
                        spans.push((i..j + 1, Token::Island));
                        i = j + 1;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    spans
}

/// Marks `(?<name>` group openers and `\e` / `\x1b` escapes in a regex
/// source. Lookbehind-style `(?<=` / `(?<!` openers are not group names and
/// stay unhighlighted.
pub fn scan_regex(line: &str) -> Vec<(Range<usize>, Token)> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            let name_start = if line[i..].starts_with("(?<") {
                Some(i + 3)
            } else if line[i..].starts_with("(?P<") {
                Some(i + 4)
            } else {
                None
            };
            if let Some(start) = name_start
                && !matches!(bytes.get(start), Some(b'=' | b'!'))
                && let Some(rel) = line[start..].find('>')
            {
                spans.push((i..start + rel + 1, Token::GroupOpen));
                i = start + rel + 1;
                continue;
            }
            i += 1;
        } else if bytes[i] == b'\\' {
            if line[i..].starts_with("\\x1b") || line[i..].starts_with("\\x1B") {
                spans.push((i..i + 4, Token::Escape));
                i += 4;
            } else if line[i..].starts_with("\\e") {
                spans.push((i..i + 2, Token::Escape));
                i += 2;
            } else {
                // Any other escape: skip both bytes so `\\e` stays literal.
                i += 2;
            }
        } else {
            i += 1;
        }
    }
    spans
}

/// Marks `$ref` tokens in a send-text body: the accent when the current
/// matcher provides the reference, the error color when nothing captures it.
pub fn scan_send_text(line: &str, known: &[String]) -> Vec<(Range<usize>, Token)> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 1 {
                let reference = &line[i..j];
                let token = if known.iter().any(|k| k == reference) {
                    Token::KnownRef
                } else {
                    Token::UnknownRef
                };
                spans.push((i..j, token));
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(spans: &[(Range<usize>, Token)]) -> Vec<(Range<usize>, Token)> {
        spans.to_vec()
    }

    #[test]
    fn pattern_scanner_marks_holes_wildcards_and_islands() {
        let spans = scan_pattern("greet {person} * /\\d+/ {x");
        assert_eq!(
            kinds(&spans),
            vec![
                (6..14, Token::Hole),
                (15..16, Token::Wildcard),
                (17..22, Token::Island),
            ],
            "the unclosed brace at the end is literal text"
        );
    }

    #[test]
    fn island_escapes_shield_the_closing_slash() {
        let spans = scan_pattern("/a\\/b/");
        assert_eq!(spans, vec![(0..6, Token::Island)]);
    }

    #[test]
    fn regex_scanner_marks_group_openers_and_escapes() {
        let spans = scan_regex(r"\e\[31m(?<hp>\d+)(?:x)(?<=y)");
        assert_eq!(
            spans,
            vec![(0..2, Token::Escape), (7..13, Token::GroupOpen)],
            "non-capturing groups and lookbehind stay plain"
        );
        assert_eq!(
            scan_regex(r"\\e"),
            vec![],
            "a literal backslash-e is not the escape"
        );
        assert_eq!(scan_regex(r"\x1b"), vec![(0..4, Token::Escape)]);
    }

    #[test]
    fn send_text_scanner_classifies_references() {
        let known = vec!["$person".to_string(), "$1".to_string()];
        let spans = scan_send_text("say $person hits $targt for $1", &known);
        assert_eq!(
            spans,
            vec![
                (4..11, Token::KnownRef),
                (17..23, Token::UnknownRef),
                (28..30, Token::KnownRef),
            ]
        );
    }
}
