use tower_lsp::lsp_types::{DocumentFormattingParams, Position, Range, TextEdit};

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
