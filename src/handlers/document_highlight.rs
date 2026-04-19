use tower_lsp::lsp_types::{DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams};

use crate::document::{Analysis, CursorContext, RefKind};
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn document_highlight(
    backend: &Backend,
    params: &DocumentHighlightParams,
) -> Option<Vec<DocumentHighlight>> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    // Snapshot document state and drop the guard before get_analysis
    // to avoid deadlocking on document_map (see hover.rs for details).
    let (source, rope, offset, word) = {
        let doc = backend.document_map.get(uri)?;
        let offset = text::position_to_offset(&doc.rope, pos)?;
        let word = text::word_at_offset(&doc.text, offset)?.to_owned();
        (doc.text.clone(), doc.rope.clone(), offset, word)
    };

    let analysis = backend.get_analysis(uri)?;

    match analysis.cursor_context(offset, &source) {
        // Grammar config fields aren't highlightable.
        CursorContext::GrammarConfigField => None,
        CursorContext::BaseRuleAccess => base_rule_highlights(&analysis, &rope, &word),
        CursorContext::ImportModuleAccess { .. } => {
            import_member_highlights(&analysis, &rope, &word)
        }
        CursorContext::Identifier { scope } => {
            local_highlights(&analysis, &source, &rope, &word, scope)
        }
    }
}

/// Highlight only `base::rule_name` references in the current file.
fn base_rule_highlights(
    analysis: &Analysis,
    rope: &ropey::Rope,
    word: &str,
) -> Option<Vec<DocumentHighlight>> {
    let highlights: Vec<DocumentHighlight> = analysis
        .references
        .iter()
        .flatten()
        .filter(|r| matches!(&r.kind, RefKind::BaseRule(name) if name == word))
        .map(|r| DocumentHighlight {
            range: text::span_to_range(rope, r.span),
            kind: Some(DocumentHighlightKind::READ),
        })
        .collect();
    (!highlights.is_empty()).then_some(highlights)
}

/// Highlight a regular identifier's definition (WRITE) and references (READ).
fn local_highlights(
    analysis: &Analysis,
    source: &str,
    rope: &ropey::Rope,
    word: &str,
    cursor_scope: Option<tree_sitter_generate::nativedsl::ast::Span>,
) -> Option<Vec<DocumentHighlight>> {
    let mut highlights = Vec::new();

    for def in analysis.definitions.iter().flatten() {
        if def.name == word {
            match (cursor_scope, def.kind.scope()) {
                (Some(cs), Some(ds)) if cs != ds => continue,
                _ => {}
            }
            highlights.push(DocumentHighlight {
                range: text::span_to_range(rope, def.name_span),
                kind: Some(DocumentHighlightKind::WRITE),
            });
        }
    }

    for reference in analysis.references.iter().flatten() {
        // Exclude BaseRule references - they refer to the base grammar's
        // version, not the local override.
        if matches!(reference.kind, RefKind::BaseRule(_)) {
            continue;
        }
        if reference.matches_word(word, source, cursor_scope) {
            highlights.push(DocumentHighlight {
                range: text::span_to_range(rope, reference.span),
                kind: Some(DocumentHighlightKind::READ),
            });
        }
    }

    (!highlights.is_empty()).then_some(highlights)
}

/// Highlight all `ImportedMember` references with the same member name.
fn import_member_highlights(
    analysis: &Analysis,
    rope: &ropey::Rope,
    word: &str,
) -> Option<Vec<DocumentHighlight>> {
    let highlights: Vec<DocumentHighlight> = analysis
        .references
        .iter()
        .flatten()
        .filter(|r| matches!(&r.kind, RefKind::ImportedMember { member, .. } if member == word))
        .map(|r| DocumentHighlight {
            range: text::span_to_range(rope, r.span),
            kind: Some(DocumentHighlightKind::READ),
        })
        .collect();
    (!highlights.is_empty()).then_some(highlights)
}
