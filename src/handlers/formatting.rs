use tower_lsp::lsp_types::{
    DocumentFormattingParams, DocumentRangeFormattingParams, Position, Range, TextEdit,
};

use crate::analysis::uri_to_grammar_path;
use crate::formatter;
use crate::server::Backend;
use crate::text;

/// Handles `textDocument/formatting`. Returns a single `TextEdit` replacing
/// the whole document with the formatted output, or `None` if the document
/// is unknown, unparseable, or already formatted.
///
/// Client-provided `FormattingOptions` (tab size, insert spaces, trim-trailing-
/// whitespace, etc.) are ignored - the formatter reads its settings from our
/// own `config.formatting` so project-level overrides apply consistently
/// between CLI and LSP invocations.
pub async fn formatting(
    backend: &Backend,
    params: &DocumentFormattingParams,
) -> Option<Vec<TextEdit>> {
    let uri = &params.text_document.uri;
    let path = uri_to_grammar_path(uri)?;

    // Acquire the config read guard first - it's the only awaiting call. With
    // that held, the rest of the work is synchronous, so it's safe to grab a
    // DashMap ref to the document without risking a deadlock across await.
    let config = backend.config.read().await;
    let doc = backend.document_map.get(uri)?;

    let formatted = formatter::format(&doc.text, &path, &config.formatting)?;
    drop(config);

    if formatted == doc.text {
        return Some(Vec::new());
    }

    let end_pos = text::offset_to_position(&doc.rope, u32::try_from(doc.rope.len_bytes()).ok()?);
    drop(doc);
    Some(vec![TextEdit {
        range: Range::new(Position::new(0, 0), end_pos),
        new_text: formatted,
    }])
}

/// Handles `textDocument/rangeFormatting`. The formatter is whole-document by
/// design, so we format the entire buffer and emit the full-replace edit only
/// when the requested range actually overlaps changed bytes. Most editors
/// invoke range formatting on selection-only; if the selection is already
/// formatted, returning an empty edit list avoids surprising cursor jumps.
pub async fn range_formatting(
    backend: &Backend,
    params: &DocumentRangeFormattingParams,
) -> Option<Vec<TextEdit>> {
    let uri = &params.text_document.uri;
    let path = uri_to_grammar_path(uri)?;

    let config = backend.config.read().await;
    let doc = backend.document_map.get(uri)?;

    let formatted = formatter::format(&doc.text, &path, &config.formatting)?;
    drop(config);

    if formatted == doc.text {
        return Some(Vec::new());
    }

    let range_start = text::position_to_offset(&doc.rope, params.range.start)?;
    let range_end = text::position_to_offset(&doc.rope, params.range.end)?;
    if !range_overlaps_diff(&doc.text, &formatted, range_start, range_end) {
        return Some(Vec::new());
    }

    let end_pos = text::offset_to_position(&doc.rope, u32::try_from(doc.rope.len_bytes()).ok()?);
    drop(doc);
    Some(vec![TextEdit {
        range: Range::new(Position::new(0, 0), end_pos),
        new_text: formatted,
    }])
}

/// True if the byte range `[start, end)` in `original` overlaps the region
/// where `original` and `formatted` differ. Lets range-formatting return an
/// empty edit when the user's selection is already formatted even though the
/// rest of the file isn't.
fn range_overlaps_diff(original: &str, formatted: &str, start: u32, end: u32) -> bool {
    let prefix_len = original
        .as_bytes()
        .iter()
        .zip(formatted.as_bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix_len = original
        .as_bytes()
        .iter()
        .rev()
        .zip(formatted.as_bytes().iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(original.len() - prefix_len)
        .min(formatted.len() - prefix_len);
    let diff_start = prefix_len as u32;
    let diff_end = (original.len() - suffix_len) as u32;
    start < diff_end && end > diff_start
}
