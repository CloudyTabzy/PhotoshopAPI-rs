//! Range-based styling (`TextLayerRangeStyleMixin.h` upstream).
//!
//! A range handle targets one or more `[start, end)` spans in UTF-16 code
//! units (the unit of `style_run_lengths`). Each setter splits runs where a
//! span boundary falls mid-run, then writes the property to every covered
//! run. The typed setters themselves are generated alongside the run setters
//! in [`super::style`].

use std::ops::Range;

use super::invalid;
use crate::{BitDepth, Layer};

/// Which matches of a search string a range handle covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occurrence {
    /// Every non-overlapping match, searched left to right.
    All,
    /// Only the match with this zero-based index.
    Nth(usize),
}

/// Character-style handle over one or more text spans.
pub struct CharacterStyleRange<'a, T: BitDepth> {
    layer: &'a mut Layer<T>,
    spans: Vec<Range<usize>>,
}

/// Paragraph-style handle over one or more text spans.
pub struct ParagraphStyleRange<'a, T: BitDepth> {
    layer: &'a mut Layer<T>,
    spans: Vec<Range<usize>>,
}

macro_rules! range_handle {
    ($name:ident, $style:literal) => {
        impl<T: BitDepth> $name<'_, T> {
            /// The targeted spans in UTF-16 code units.
            pub fn spans(&self) -> &[Range<usize>] {
                &self.spans
            }

            /// A handle with no spans (e.g. a search without matches) makes
            /// every setter a no-op.
            pub fn is_empty(&self) -> bool {
                self.spans.is_empty()
            }

            /// Split runs at the span boundaries and call `set` for every
            /// covered run. All-or-nothing: on error the layer is restored.
            ///
            /// Fidelity delta: upstream keeps splits and
            /// earlier writes when a later setter throws.
            pub(crate) fn apply(
                &mut self,
                mut set: impl FnMut(&mut Layer<T>, usize) -> psd_core::Result<()>,
            ) -> psd_core::Result<&mut Self> {
                let snapshot = self.layer.blocks.clone();
                for span in self.spans.clone() {
                    if let Err(error) = apply_span(self.layer, span, $style, &mut set) {
                        self.layer.blocks = snapshot;
                        return Err(error);
                    }
                }
                Ok(self)
            }
        }
    };
}

range_handle!(CharacterStyleRange, true);
range_handle!(ParagraphStyleRange, false);

fn run_lengths<T: BitDepth>(layer: &Layer<T>, style: bool) -> psd_core::Result<Vec<usize>> {
    let lengths = if style {
        layer.style_run_lengths()
    } else {
        layer.paragraph_run_lengths()
    };
    lengths
        .filter(|lengths| !lengths.is_empty())
        .ok_or_else(|| invalid("text layer has no run lengths"))?
        .into_iter()
        .map(|length| usize::try_from(length).map_err(|_| invalid("text run length is negative")))
        .collect()
}

/// Run start offsets plus the total length: `[0, len0, len0 + len1, …]`.
fn boundaries(lengths: &[usize]) -> Vec<usize> {
    let mut cumulative = Vec::with_capacity(lengths.len() + 1);
    cumulative.push(0);
    for length in lengths {
        cumulative.push(cumulative.last().copied().unwrap_or(0) + length);
    }
    cumulative
}

/// The run containing code unit `offset` (`offset` < total length).
fn run_at(cumulative: &[usize], offset: usize) -> usize {
    cumulative
        .windows(2)
        .position(|bounds| offset < bounds[1])
        .unwrap_or(cumulative.len().saturating_sub(2))
}

/// Widen `[start, end)` to whole `\r`-terminated paragraphs: paragraph
/// attributes belong to entire paragraphs, as in Photoshop's paragraph panel.
///
/// Deliberate fidelity delta: upstream splits paragraph
/// runs at the exact character offsets, which leaves a paragraph styled
/// across two runs; here a split only ever lands on a paragraph boundary.
fn paragraph_extent<T: BitDepth>(layer: &Layer<T>, start: usize, end: usize) -> (usize, usize) {
    if start >= end {
        return (start, end);
    }
    let Some(units) = layer
        .engine_data()
        .and_then(|root| {
            root.get_path(["EngineDict", "Editor", "Text"])?
                .as_literal_bytes()
                .map(<[u8]>::to_vec)
        })
        .and_then(|bytes| psd_core::engine_data::decode_utf16be_literal_units(&bytes).ok())
        .map(|(units, _)| units)
    else {
        return (start, end);
    };
    let mut paragraph_start = 0;
    let (mut first, mut last) = (start, end);
    for paragraph in units.split_inclusive(|&unit| unit == u16::from(b'\r')) {
        let paragraph_end = paragraph_start + paragraph.len();
        if paragraph_start <= start && start < paragraph_end {
            first = paragraph_start;
        }
        if paragraph_start < end && end <= paragraph_end {
            last = paragraph_end;
        }
        paragraph_start = paragraph_end;
    }
    (first, last)
}

fn apply_span<T: BitDepth>(
    layer: &mut Layer<T>,
    span: Range<usize>,
    style: bool,
    set: &mut impl FnMut(&mut Layer<T>, usize) -> psd_core::Result<()>,
) -> psd_core::Result<()> {
    let split = |layer: &mut Layer<T>, run, offset| {
        if style {
            layer.split_style_run(run, offset)
        } else {
            layer.split_paragraph_run(run, offset)
        }
    };

    let mut cumulative = boundaries(&run_lengths(layer, style)?);
    let total = *cumulative.last().unwrap_or(&0);
    let (start, end) = if style {
        (span.start, span.end.min(total))
    } else {
        paragraph_extent(layer, span.start, span.end.min(total))
    };
    if start >= end {
        return Ok(());
    }

    let mut first = run_at(&cumulative, start);
    if cumulative[first] < start {
        split(layer, first, start - cumulative[first])?;
        first += 1;
        cumulative = boundaries(&run_lengths(layer, style)?);
    }
    let last = run_at(&cumulative, end - 1);
    if cumulative[last + 1] > end {
        split(layer, last, end - cumulative[last])?;
    }
    for run in first..=last {
        set(layer, run)?;
    }
    Ok(())
}

impl<T: BitDepth> Layer<T> {
    /// Character styles for `[range.start, range.end)` in UTF-16 code units.
    pub fn style_range(&mut self, range: Range<usize>) -> CharacterStyleRange<'_, T> {
        CharacterStyleRange {
            layer: self,
            spans: vec![range],
        }
    }

    /// Character styles for matches of `needle` in the visible text.
    pub fn style_text(
        &mut self,
        needle: &str,
        occurrence: Occurrence,
    ) -> CharacterStyleRange<'_, T> {
        let spans = self.text_spans(needle, occurrence);
        CharacterStyleRange { layer: self, spans }
    }

    /// Character styles for the whole text, including the terminal carriage
    /// return that EngineData counts in its run lengths.
    pub fn style_all(&mut self) -> CharacterStyleRange<'_, T> {
        let spans = self.whole_text_span(true);
        CharacterStyleRange { layer: self, spans }
    }

    /// Paragraph styles for `[range.start, range.end)` in UTF-16 code units.
    pub fn paragraph_range(&mut self, range: Range<usize>) -> ParagraphStyleRange<'_, T> {
        ParagraphStyleRange {
            layer: self,
            spans: vec![range],
        }
    }

    /// Paragraph styles for matches of `needle` in the visible text.
    pub fn paragraph_text(
        &mut self,
        needle: &str,
        occurrence: Occurrence,
    ) -> ParagraphStyleRange<'_, T> {
        let spans = self.text_spans(needle, occurrence);
        ParagraphStyleRange { layer: self, spans }
    }

    /// Paragraph styles for the whole text.
    pub fn paragraph_all(&mut self) -> ParagraphStyleRange<'_, T> {
        let spans = self.whole_text_span(false);
        ParagraphStyleRange { layer: self, spans }
    }

    fn whole_text_span(&self, style: bool) -> Vec<Range<usize>> {
        run_lengths(self, style)
            .map(|lengths| std::iter::once(0..lengths.iter().sum()).collect())
            .unwrap_or_default()
    }

    /// Non-overlapping, left-to-right UTF-16 matches of `needle`.
    fn text_spans(&self, needle: &str, occurrence: Occurrence) -> Vec<Range<usize>> {
        let needle: Vec<u16> = needle.encode_utf16().collect();
        let Some(text) = self.text() else {
            return Vec::new();
        };
        let text: Vec<u16> = text.encode_utf16().collect();
        if needle.is_empty() || needle.len() > text.len() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        let mut cursor = 0;
        while cursor + needle.len() <= text.len() {
            if text[cursor..cursor + needle.len()] == needle[..] {
                matches.push(cursor..cursor + needle.len());
                cursor += needle.len();
            } else {
                cursor += 1;
            }
        }
        match occurrence {
            Occurrence::All => matches,
            Occurrence::Nth(index) => matches.into_iter().nth(index).into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_lookup_uses_half_open_run_bounds() {
        let cumulative = boundaries(&[6, 4, 7]);
        assert_eq!(cumulative, vec![0, 6, 10, 17]);
        assert_eq!(run_at(&cumulative, 0), 0);
        assert_eq!(run_at(&cumulative, 5), 0);
        assert_eq!(run_at(&cumulative, 6), 1);
        assert_eq!(run_at(&cumulative, 16), 2);
    }
}
