use tower_lsp::lsp_types::{Location, ReferenceParams, Url};

use crate::analysis;
use crate::document::{CursorContext, RefKind};
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn references(backend: &Backend, params: &ReferenceParams) -> Option<Vec<Location>> {
    let uri = &params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    let include_declaration = params.context.include_declaration;

    let doc = backend.document_map.get(uri)?;
    let offset = text::position_to_offset(&doc.rope, pos)?;
    let word = text::word_at_offset(&doc.text, offset)?;

    let ctx = backend.analysis_context();
    let analysis = analysis::analyze(&doc.text, uri, Some(&ctx));

    match analysis.cursor_context(offset) {
        // Grammar config fields aren't referenceable.
        CursorContext::GrammarConfigField => None,
        CursorContext::BaseRuleAccess => {
            base_rule_references(&analysis, uri, &doc.rope, word, include_declaration)
        }
        CursorContext::Identifier { scope } => {
            local_references(&analysis, uri, &doc, word, scope, include_declaration)
        }
    }
}

/// References for `base::rule_name`: base grammar definition + base grammar
/// usages + derived file `BaseRule` refs.
fn base_rule_references(
    analysis: &crate::document::Analysis,
    uri: &Url,
    rope: &ropey::Rope,
    word: &str,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let mut locations = Vec::new();

    if let (Some(base_path), Some(base_rope)) = (&analysis.base_grammar_path, &analysis.base_rope)
        && let Ok(base_uri) = Url::from_file_path(base_path)
    {
        if include_declaration
            && let Some(def) = analysis
                .base_definitions
                .iter()
                .flatten()
                .find(|d| d.name == word)
        {
            locations.push(Location {
                uri: base_uri.clone(),
                range: text::span_to_range(base_rope, def.name_span),
            });
        }
        for reference in analysis.base_references.iter().flatten() {
            if matches!(&reference.kind, RefKind::Rule(name) if name == word) {
                locations.push(Location {
                    uri: base_uri.clone(),
                    range: text::span_to_range(base_rope, reference.span),
                });
            }
        }
    }

    for reference in analysis.references.iter().flatten() {
        if matches!(&reference.kind, RefKind::BaseRule(name) if name == word) {
            locations.push(Location {
                uri: uri.clone(),
                range: text::span_to_range(rope, reference.span),
            });
        }
    }

    (!locations.is_empty()).then_some(locations)
}

/// References for a regular identifier in the current file.
fn local_references(
    analysis: &crate::document::Analysis,
    uri: &Url,
    doc: &crate::document::Document,
    word: &str,
    cursor_scope: Option<tree_sitter_generate::nativedsl::ast::Span>,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let mut locations = Vec::new();

    if include_declaration {
        for def in analysis.definitions.iter().flatten() {
            if def.name == word {
                match (cursor_scope, def.kind.scope()) {
                    (Some(cs), Some(ds)) if cs != ds => continue,
                    _ => {}
                }
                locations.push(Location {
                    uri: uri.clone(),
                    range: text::span_to_range(&doc.rope, def.name_span),
                });
            }
        }
    }

    for reference in analysis.references.iter().flatten() {
        // Exclude BaseRule references - they refer to the base grammar's
        // version, not the local override.
        if matches!(reference.kind, RefKind::BaseRule(_)) {
            continue;
        }
        if reference.matches_word(word, &doc.text, cursor_scope) {
            locations.push(Location {
                uri: uri.clone(),
                range: text::span_to_range(&doc.rope, reference.span),
            });
        }
    }

    (!locations.is_empty()).then_some(locations)
}
