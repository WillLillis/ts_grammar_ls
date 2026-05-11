use tower_lsp::lsp_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, SymbolKind,
};

use crate::document::{DefKind, Definition};
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn document_symbol(
    backend: &Backend,
    params: &DocumentSymbolParams,
) -> Option<DocumentSymbolResponse> {
    let uri = &params.text_document.uri;
    let analysis = backend.get_analysis(uri)?;
    let defs = analysis.definitions.as_deref()?;
    let rope = &analysis.rope;

    let symbols: Vec<DocumentSymbol> = defs
        .iter()
        .filter_map(|def| top_level_symbol(def, defs, rope))
        .collect();

    Some(DocumentSymbolResponse::Nested(symbols))
}

/// Build a `DocumentSymbol` for a top-level def, nesting children where it
/// makes sense: `ObjectKey` under its owning `Let`, `Parameter` under its
/// owning `Function`. Returns `None` for kinds that shouldn't appear as
/// outline entries on their own (ObjectKey, Parameter).
#[expect(
    deprecated,
    reason = "DocumentSymbol::deprecated is deprecated but required by the struct"
)]
fn top_level_symbol(
    def: &Definition,
    defs: &[Definition],
    rope: &ropey::Rope,
) -> Option<DocumentSymbol> {
    let kind = match def.kind {
        DefKind::Rule | DefKind::OverrideRule => SymbolKind::CLASS,
        DefKind::Function { .. } => SymbolKind::FUNCTION,
        DefKind::Let { .. } => SymbolKind::VARIABLE,
        DefKind::Import | DefKind::Inherit => SymbolKind::MODULE,
        DefKind::External => SymbolKind::CONSTANT,
        DefKind::ObjectKey { .. } | DefKind::Parameter { .. } => return None,
    };
    let detail = if let DefKind::Function { signature } = &def.kind {
        Some(signature.clone())
    } else {
        None
    };
    let children = match &def.kind {
        DefKind::Let { .. } => collect_children(defs, rope, |d| {
            matches!(d.kind, DefKind::ObjectKey { .. })
                && d.name_span.start >= def.full_span.start
                && d.name_span.end <= def.full_span.end
        }),
        DefKind::Function { .. } => collect_children(defs, rope, |d| {
            matches!(d.kind, DefKind::Parameter { scope, .. } if scope == def.full_span)
        }),
        _ => None,
    };
    Some(DocumentSymbol {
        name: def.name.clone(),
        kind,
        detail,
        range: text::span_to_range(rope, def.full_span),
        selection_range: text::span_to_range(rope, def.name_span),
        children,
        tags: None,
        deprecated: None,
    })
}

/// Collect children matching `pred` as flat (leaf) symbols. Returns `None`
/// when no children matched, so the parent's `children` field stays `None`
/// rather than `Some(vec![])` (some clients render the latter differently).
#[expect(
    deprecated,
    reason = "DocumentSymbol::deprecated is deprecated but required by the struct"
)]
fn collect_children(
    defs: &[Definition],
    rope: &ropey::Rope,
    pred: impl Fn(&Definition) -> bool,
) -> Option<Vec<DocumentSymbol>> {
    let kids: Vec<DocumentSymbol> = defs
        .iter()
        .filter(|d| pred(d))
        .map(|d| {
            let (kind, detail) = match d.kind {
                DefKind::ObjectKey { .. } => (SymbolKind::FIELD, None),
                DefKind::Parameter { ty, .. } => (SymbolKind::VARIABLE, Some(ty.to_string())),
                _ => (SymbolKind::NULL, None),
            };
            DocumentSymbol {
                name: d.name.clone(),
                kind,
                detail,
                range: text::span_to_range(rope, d.full_span),
                selection_range: text::span_to_range(rope, d.name_span),
                children: None,
                tags: None,
                deprecated: None,
            }
        })
        .collect();
    (!kids.is_empty()).then_some(kids)
}
