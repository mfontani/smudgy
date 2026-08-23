use std::{borrow::Cow, cmp::min, rc::Rc};

use crate::terminal_buffer::selection::LineSelection;
use iced::widget::text::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchRange {
    pub start: usize,
    pub end: usize,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchSpan {
    pub span_index: usize,
    pub match_index: usize,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct Spans<Link: Clone> {
    spans: Rc<Vec<Span<'static, Link>>>,
    selected: Vec<usize>,
    search_ranges: Vec<SearchRange>,
    search_spans: Vec<SearchSpan>,
    spans_with_selection: Option<Rc<Vec<Span<'static, Link>>>>,
}

impl<Link: Clone> Spans<Link> {
    #[cfg(test)]
    pub fn with_selection(spans: Rc<Vec<Span<'static, Link>>>, selection: LineSelection) -> Self {
        Self::with_search(spans, selection, &[])
    }

    pub fn with_search(
        spans: Rc<Vec<Span<'static, Link>>>,
        selection: LineSelection,
        search_ranges: &[SearchRange],
    ) -> Self {
        let spans = split_at_search_boundaries(spans, search_ranges);
        let mut decorated = Self {
            spans,
            selected: Vec::new(),
            search_ranges: search_ranges.to_vec(),
            search_spans: Vec::new(),
            spans_with_selection: None,
        };
        match selection {
            None => decorated.select_none(),
            Some((0, usize::MAX)) => {
                decorated.select_all();
            }
            Some((from, to)) => {
                decorated.select_range(from, to);
            }
        }
        decorated
    }

    pub fn spans(&self) -> Rc<Vec<Span<'static, Link>>> {
        self.spans_with_selection
            .as_ref()
            .map(|spans| spans.clone())
            .unwrap_or_else(|| self.spans.clone())
    }

    pub fn select_none(&mut self) {
        self.selected.clear();
        self.spans_with_selection = None;
        self.refresh_search_spans();
    }

    pub fn select_all(&mut self) {
        self.selected = (0..self.spans.len()).collect();
        self.spans_with_selection = None;
        self.refresh_search_spans();
    }

    pub fn select_range(&mut self, sel_start: usize, sel_end: usize) {
        let mut byte_position = 0;

        self.selected.clear();

        self.spans_with_selection = Some(Rc::new(
            self.spans
                .iter()
                .flat_map(|span| {
                    let span_text = span.text.as_ref();
                    let span_byte_end = byte_position + span_text.len();
                    let boundary = |mut offset: usize| {
                        offset = offset.min(span_text.len());
                        while offset > 0 && !span_text.is_char_boundary(offset) {
                            offset -= 1;
                        }
                        offset
                    };

                    let mut spans = Vec::with_capacity(3);

                    // Part before selection
                    if sel_start > byte_position {
                        let unselected_end =
                            boundary(min(sel_start, span_byte_end).saturating_sub(byte_position));
                        if unselected_end > 0 {
                            spans.push((
                                false,
                                Span {
                                    text: Cow::Owned(span_text[..unselected_end].to_string()),
                                    link: span.link.clone(),
                                    ..*span
                                },
                            ));
                        }
                    }

                    // Selected part
                    if sel_start < span_byte_end && sel_end > byte_position {
                        let selected_start = boundary(sel_start.saturating_sub(byte_position));
                        let selected_end =
                            boundary(min(sel_end, span_byte_end).saturating_sub(byte_position));

                        if selected_end > selected_start {
                            spans.push((
                                true,
                                Span {
                                    text: Cow::Owned(
                                        span_text[selected_start..selected_end].to_string(),
                                    ),
                                    link: span.link.clone(),
                                    ..*span
                                },
                            ));
                        }
                    }

                    // Part after selection
                    if sel_end < span_byte_end {
                        let unselected_start = boundary(sel_end.saturating_sub(byte_position));
                        if unselected_start < span_text.len() {
                            spans.push((
                                false,
                                Span {
                                    text: Cow::Owned(span_text[unselected_start..].to_string()),
                                    link: span.link.clone(),
                                    ..*span
                                },
                            ));
                        }
                    }

                    byte_position = span_byte_end;
                    spans
                })
                .enumerate()
                .map(|(i, (selected, span))| {
                    if selected {
                        self.selected.push(i);
                    }
                    span
                })
                .collect(),
        ));
        self.refresh_search_spans();
    }

    pub fn selected(&self) -> &[usize] {
        &self.selected
    }

    pub fn search_spans(&self) -> &[SearchSpan] {
        &self.search_spans
    }

    fn refresh_search_spans(&mut self) {
        let spans = self.spans();
        let mut byte_position = 0;
        self.search_spans = spans
            .iter()
            .enumerate()
            .filter_map(|(span_index, span)| {
                let span_end = byte_position + span.text.len();
                let matched = self
                    .search_ranges
                    .iter()
                    .enumerate()
                    .find(|(_, range)| byte_position < range.end && span_end > range.start)
                    .map(|(match_index, range)| SearchSpan {
                        span_index,
                        match_index,
                        active: range.active,
                    });
                byte_position = span_end;
                matched
            })
            .collect();
    }
}

fn split_at_search_boundaries<Link: Clone>(
    spans: Rc<Vec<Span<'static, Link>>>,
    search_ranges: &[SearchRange],
) -> Rc<Vec<Span<'static, Link>>> {
    if search_ranges.is_empty() {
        return spans;
    }

    let mut byte_position = 0;
    Rc::new(
        spans
            .iter()
            .flat_map(|span| {
                let text = span.text.as_ref();
                let span_start = byte_position;
                let span_end = span_start + text.len();
                let boundary = |mut offset: usize| {
                    offset = offset.min(text.len());
                    while offset > 0 && !text.is_char_boundary(offset) {
                        offset -= 1;
                    }
                    offset
                };
                let mut cuts = vec![0, text.len()];
                for range in search_ranges {
                    if range.start < span_end && range.end > span_start {
                        cuts.push(boundary(range.start.saturating_sub(span_start)));
                        cuts.push(boundary(range.end.saturating_sub(span_start)));
                    }
                }
                cuts.sort_unstable();
                cuts.dedup();
                byte_position = span_end;
                cuts.windows(2)
                    .filter_map(|window| {
                        let [start, end] = *window else {
                            return None;
                        };
                        (end > start).then(|| Span {
                            text: Cow::Owned(text[start..end].to_string()),
                            link: span.link.clone(),
                            ..*span
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_ranges_are_utf8_byte_offsets() {
        let mut spans = Spans::with_selection(
            Rc::new(vec![Span::<'static, ()>::new(Cow::Borrowed("A🗝️B"))]),
            Some((1, 8)),
        );
        let rendered = spans.spans();

        assert_eq!(rendered.len(), 3);
        assert_eq!(rendered[0].text, "A");
        assert_eq!(rendered[1].text, "🗝️");
        assert_eq!(rendered[2].text, "B");
        assert_eq!(spans.selected(), &[1]);

        spans.select_none();
        assert!(spans.selected().is_empty());
    }

    #[test]
    fn search_ranges_split_styled_spans_and_survive_selection_splits() {
        let spans = Spans::with_search(
            Rc::new(vec![
                Span::<'static, ()>::new(Cow::Borrowed("old dra")),
                Span::<'static, ()>::new(Cow::Borrowed("gon new")),
            ]),
            Some((5, 8)),
            &[SearchRange {
                start: 4,
                end: 10,
                active: true,
            }],
        );
        let rendered = spans.spans();
        let matched: String = spans
            .search_spans()
            .iter()
            .map(|matched| rendered[matched.span_index].text.as_ref())
            .collect();

        assert_eq!(matched, "dragon");
        assert!(spans.search_spans().iter().all(|matched| matched.active));
        assert_eq!(
            spans
                .selected()
                .iter()
                .map(|index| rendered[*index].text.as_ref())
                .collect::<String>(),
            "rag"
        );
    }
}
