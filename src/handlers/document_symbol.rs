use tower_lsp::lsp_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, SymbolKind,
};

use crate::analysis;
use crate::document::DefKind;
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn document_symbol(
    backend: &Backend,
    params: &DocumentSymbolParams,
) -> Option<DocumentSymbolResponse> {
    let uri = &params.text_document.uri;
    let doc = backend.document_map.get(uri)?;

    let ctx = backend.analysis_context();
    let analysis = analysis::analyze(&doc.text, uri, Some(&ctx));

    let symbols: Vec<DocumentSymbol> = analysis
        .definitions
        .iter()
        .flatten()
        .filter_map(|def| {
            // ObjectKey defs are kept internal (used by field-access completion
            // and goto-def) but not surfaced as outline symbols: they're value
            // literal members, not schema definitions, which matches how code
            // LSPs (tsserver, rust-analyzer, gopls, pylsp) treat object literals.
            let kind = match def.kind {
                DefKind::Rule | DefKind::OverrideRule => SymbolKind::CLASS,
                DefKind::Function { .. } => SymbolKind::FUNCTION,
                DefKind::Let { .. } => SymbolKind::VARIABLE,
                DefKind::ObjectKey | DefKind::Parameter { .. } => return None,
            };
            let range = text::span_to_range(&doc.rope, def.full_span);
            let selection_range = text::span_to_range(&doc.rope, def.name_span);
            let detail = if let DefKind::Function { signature } = &def.kind {
                Some(signature.clone())
            } else {
                None
            };
            #[expect(
                deprecated,
                reason = "DocumentSymbol::deprecated is deprecated but required by the struct"
            )]
            Some(DocumentSymbol {
                name: def.name.clone(),
                kind,
                range,
                selection_range,
                detail,
                children: None,
                tags: None,
                deprecated: None,
            })
        })
        .collect();
    drop(doc);

    Some(DocumentSymbolResponse::Nested(symbols))
}
