#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferPosition {
    pub line: usize,
    pub column: usize,
}

pub type LineSelection = Option<(usize, usize)>;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    #[default]
    None,
    Selecting {
        origin: BufferPosition,
        from: BufferPosition,
        to: BufferPosition,
    },
    Selected {
        from: BufferPosition,
        to: BufferPosition,
    },
}

/// The `[start, end)` byte range of the whitespace-delimited WORD containing
/// `column` in `text`, or `None` if `column` lands on whitespace, or past
/// the end of the line. Callers fall back to plain single-click behavior in
/// that case. `column` is snapped to the nearest char boundary at or before
/// it first, the same way selection columns are already handled elsewhere
/// (see `TerminalBuffer::selected_text`'s own char-boundary clamp).
pub fn word_span_at(text: &str, column: usize) -> Option<(usize, usize)> {
    let mut column = column.min(text.len());
    while column > 0 && !text.is_char_boundary(column) {
        column -= 1;
    }

    let under_cursor = text[column..].chars().next();
    if under_cursor.is_none_or(char::is_whitespace) {
        return None;
    }

    let start = text[..column]
        .char_indices()
        .rev()
        .find(|&(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let end = text[column..]
        .char_indices()
        .find(|&(_, c)| c.is_whitespace())
        .map(|(i, _)| column + i)
        .unwrap_or(text.len());

    Some((start, end))
}

impl Selection {
    /// Whether this selection should block a click-release from focusing an
    /// input. The release handler runs after the terminal's own (the terminal
    /// is the `mouse_area`'s content), so a drag has settled into `Selected`
    /// with a non-empty range by the time this is read; a plain click reads
    /// as `None` or an empty `Selected`. Only the selection-less click
    /// focuses.
    pub fn blocks_focus(&self) -> bool {
        match self {
            Selection::None => false,
            Selection::Selected { from, to } => from != to,
            Selection::Selecting { .. } => true,
        }
    }

    pub fn for_line(&self, line_number: usize) -> LineSelection {
        match self {
            Selection::None => None,
            Selection::Selecting {
                from,
                to,
                origin: _,
            }
            | Selection::Selected { from, to } => {
                // see if this line_number fals in the range of from.line_number..=to.line_number
                if from.line <= line_number && to.line >= line_number {
                    Some((
                        if from.line == line_number {
                            from.column
                        } else {
                            0
                        },
                        if to.line == line_number {
                            to.column
                        } else {
                            usize::MAX
                        },
                    ))
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod word_span_at_tests {
    use super::word_span_at;

    #[test]
    fn mid_word() {
        let text = "advanced knifeplay foo";
        assert_eq!(word_span_at(text, 3), Some((0, 8)));
        assert_eq!(&text[0..8], "advanced");
    }

    #[test]
    fn start_of_word() {
        let text = "foo bar";
        assert_eq!(word_span_at(text, 4), Some((4, 7)));
        assert_eq!(&text[4..7], "bar");
    }

    #[test]
    fn end_of_word_last_char() {
        let text = "foo bar";
        // The 'r' in "bar", the last character of the line.
        assert_eq!(word_span_at(text, 6), Some((4, 7)));
    }

    #[test]
    fn single_word_line() {
        let text = "backstab";
        assert_eq!(word_span_at(text, 0), Some((0, 8)));
        assert_eq!(word_span_at(text, 7), Some((0, 8)));
    }

    #[test]
    fn on_whitespace_returns_none() {
        let text = "foo bar";
        assert_eq!(word_span_at(text, 3), None);
    }

    #[test]
    fn multiple_runs_of_whitespace() {
        let text = "foo   bar";
        assert_eq!(word_span_at(text, 0), Some((0, 3)));
        assert_eq!(word_span_at(text, 4), None); // inside the run of spaces
        assert_eq!(word_span_at(text, 6), Some((6, 9)));
    }

    #[test]
    fn past_end_of_line_returns_none() {
        let text = "foo";
        assert_eq!(word_span_at(text, 3), None);
        assert_eq!(word_span_at(text, 100), None);
    }

    #[test]
    fn empty_line_returns_none() {
        assert_eq!(word_span_at("", 0), None);
    }
}
