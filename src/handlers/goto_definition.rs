use tower_lsp::lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location, Range, Url};

use crate::analysis;
use crate::document::{Analysis, DefKind, RefKind};
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn goto_definition(
    backend: &Backend,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    // Snapshot document state and drop the guard before get_analysis
    // to avoid deadlocking on document_map (see hover.rs for details).
    let (source, rope, offset) = {
        let doc = backend.document_map.get(uri)?;
        let offset = text::position_to_offset(&doc.rope, pos)?;
        (doc.text.clone(), doc.rope.clone(), offset)
    };

    let analysis = backend.get_analysis(uri)?;

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
                    let range = text::span_to_range(&rope, def.name_span);
                    return Some(GotoDefinitionResponse::Scalar(Location {
                        uri: uri.clone(),
                        range,
                    }));
                }
                return goto_base_definition(&analysis, name);
            }
            RefKind::ObjectField { field, object } => {
                return goto_object_field(uri, &source, object, field);
            }
            RefKind::InheritPath => {
                let base_path = analysis.base_grammar_path.as_ref()?;
                let base_uri = Url::from_file_path(base_path).ok()?;
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: base_uri,
                    range: Range::default(),
                }));
            }
            RefKind::ImportPath => {
                // Jump to the imported file.
                return goto_import_file(&analysis, offset);
            }
            RefKind::ImportedMember { path, member } => {
                // Jump to the member definition in the imported module.
                return goto_imported_member(&analysis, path, member);
            }
            RefKind::Builtin => {}
        }
    }

    // Fallback: match by word against local definitions (for names at definition sites).
    let word = text::word_at_offset(&source, offset)?;
    let def = analysis
        .definitions
        .iter()
        .flatten()
        .find(|d| d.name == word)?;
    let range = text::span_to_range(&rope, def.name_span);

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

/// Jump to the file referenced by an `import("path")` call.
fn goto_import_file(analysis: &Analysis, offset: u32) -> Option<GotoDefinitionResponse> {
    // Find which import definition contains this offset, then use its module info.
    let reference = analysis.references.iter().flatten().find(|r| {
        offset >= r.span.start && offset < r.span.end && matches!(r.kind, RefKind::ImportPath)
    })?;

    // Find the let binding that owns this import by matching spans.
    let defs = analysis.definitions.as_ref()?;
    let import_def = defs.iter().find(|d| {
        matches!(d.kind, DefKind::Import)
            && reference.span.start >= d.full_span.start
            && reference.span.end <= d.full_span.end
    })?;
    let module_info = analysis.get_import(&import_def.name)?;
    let uri = Url::from_file_path(&module_info.path).ok()?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: Range::default(),
    }))
}

/// Jump to a member definition inside an imported module.
/// For `a::b::c`, path is `["a", "b"]` and member is `"c"`. Walks the
/// chain through nested imports to find the target module.
///
/// TODO: nested imports (`path.len()` > 1) require recursive module loading
/// in `ExternalModuleInfo` - currently only single-level imports resolve.
fn goto_imported_member(
    analysis: &Analysis,
    path: &[String],
    member: &str,
) -> Option<GotoDefinitionResponse> {
    let module_info = resolve_import_chain(analysis, path)?;
    let def = module_info.definitions.iter().find(|d| d.name == member)?;
    let uri = Url::from_file_path(&module_info.path).ok()?;
    let range = text::span_to_range(&module_info.rope, def.name_span);
    Some(GotoDefinitionResponse::Scalar(Location { uri, range }))
}

/// Walk an import path chain to find the target module info.
/// For `a::b::c`, path is `["a", "b"]` - looks up `a` in the analysis,
/// then `b` in `a`'s sub-imports.
fn resolve_import_chain<'a>(
    analysis: &'a Analysis,
    path: &[String],
) -> Option<&'a crate::document::ExternalModuleInfo> {
    let first = path.first()?;
    let mut module_info = analysis.get_import(first.as_str())?;
    for segment in &path[1..] {
        module_info = module_info.get_import(segment)?;
    }
    Some(module_info)
}
