use tower_lsp::lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location, Range, Url};

use crate::analysis;
use crate::document::{Analysis, RefKind};
use crate::server::Backend;
use crate::text;

#[must_use]
#[expect(
    clippy::significant_drop_tightening,
    reason = "doc borrow is held intentionally while analysis borrows doc.text"
)]
pub fn goto_definition(
    backend: &Backend,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let doc = backend.document_map.get(uri)?;
    let offset = text::position_to_offset(&doc.rope, pos)?;

    let ctx = backend.analysis_context();
    let analysis = analysis::analyze(&doc.text, uri, Some(&ctx));

    // Check if cursor is on a known reference from the resolved AST.
    if let Some(reference) = analysis
        .references
        .iter()
        .flatten()
        .find(|r| offset >= r.span.start && offset < r.span.end)
    {
        match &reference.kind {
            RefKind::BaseRule(name) => {
                return goto_base_definition(&analysis, name);
            }
            RefKind::Rule(name) | RefKind::Variable(name) => {
                let ref_scope = reference.scope;
                // Try scoped definitions first (parameters), then top-level, then base.
                if let Some(def) = analysis.definitions.iter().flatten().find(|d| {
                    d.name == *name
                        && match (ref_scope, d.kind.scope()) {
                            (Some(rs), Some(ds)) => rs.start == ds.start && rs.end == ds.end,
                            // top-level defs visible from any scope
                            (_, None) => true,
                            (None, Some(_)) => false, // scoped defs not visible from top level
                        }
                }) {
                    let range = text::span_to_range(&doc.rope, def.name_span);
                    return Some(GotoDefinitionResponse::Scalar(Location {
                        uri: uri.clone(),
                        range,
                    }));
                }
                return goto_base_definition(&analysis, name);
            }
            RefKind::ObjectField { field, object } => {
                return goto_object_field(uri, &doc.text, object, field);
            }
            RefKind::InheritPath => {
                let base_path = analysis.base_grammar_path.as_ref()?;
                let base_uri = Url::from_file_path(base_path).ok()?;
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: base_uri,
                    range: Range::default(),
                }));
            }
            RefKind::Builtin => {}
        }
    }

    // Fallback: match by word against local definitions (for names at definition sites).
    let word = text::word_at_offset(&doc.text, offset)?;
    let def = analysis
        .definitions
        .iter()
        .flatten()
        .find(|d| d.name == word)?;
    let range = text::span_to_range(&doc.rope, def.name_span);

    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range,
    }))
}

/// Jump to a definition in the base grammar file.
fn goto_base_definition(analysis: &Analysis, name: &str) -> Option<GotoDefinitionResponse> {
    let base_path = analysis.base_grammar_path.as_ref()?;
    let base_rope = analysis.base_rope.as_ref()?;
    let def = analysis
        .base_definitions
        .iter()
        .flatten()
        .find(|d| d.name == *name)?;
    let range = text::span_to_range(base_rope, def.name_span);
    let base_uri = Url::from_file_path(base_path).ok()?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: base_uri,
        range,
    }))
}

/// Find the definition of an object field (e.g. `CALL` in `PREC.CALL`).
fn goto_object_field(
    uri: &Url,
    text: &str,
    object_name: &str,
    field_name: &str,
) -> Option<GotoDefinitionResponse> {
    analysis::with_ast(text, uri, |parsed_ast| {
        let (key_span, _) = analysis::find_object_field(parsed_ast, object_name, field_name)?;
        let rope = ropey::Rope::from_str(text);
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: text::span_to_range(&rope, key_span),
        }))
    })?
}
