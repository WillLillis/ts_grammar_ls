use tower_lsp::lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location, Range, Url};

use crate::document::{BindingLocation, DefKind, Module, RefKind};
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn goto_definition(
    backend: &Backend,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let (analysis, offset) = backend.resolve_position(uri, pos)?;

    // Check if cursor is on a known reference from the resolved AST.
    if let Some(reference) = analysis.reference_at(offset) {
        match &reference.kind {
            RefKind::BaseRule(name) => {
                return goto_base_definition(&analysis, name);
            }
            RefKind::Rule(name) | RefKind::Variable(name) => {
                return resolve_bare_name_to_location(&analysis, name, reference.scope, uri)
                    .map(GotoDefinitionResponse::Scalar);
            }
            RefKind::ObjectField { field, object } => {
                return goto_object_field(&analysis, uri, object, field);
            }
            RefKind::InheritPath(_) => {
                let base = analysis.base_module.as_deref()?;
                let base_uri = Url::from_file_path(&base.path).ok()?;
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: base_uri,
                    range: Range::default(),
                }));
            }
            RefKind::ImportPath(binding) => {
                // Jump to the imported file. The binding name was stored on
                // the reference at extraction time.
                let module_info = analysis.get_module(binding)?;
                let target = Url::from_file_path(&module_info.path).ok()?;
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: target,
                    range: Range::default(),
                }));
            }
            RefKind::ImportedMember { path, member } => {
                // Jump to the member definition in the imported module.
                return goto_imported_member(&analysis, path, member);
            }
            RefKind::Builtin => {}
        }
    }

    // Fallback: match by word at the cursor scope (for names at definition
    // sites, where reference_at returns None). `binding_for` resolves
    // shadowing by picking the innermost visible binding.
    let word = text::word_at_offset(&analysis.source, offset)?;
    let scope = analysis.scope_at(offset);
    let def = analysis.binding_for(word, scope)?;
    let range = text::span_to_range(&analysis.rope, def.name_span);

    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range,
    }))
}

/// Jump to a definition in the base grammar file.
fn goto_base_definition(analysis: &Module, name: &str) -> Option<GotoDefinitionResponse> {
    let base = analysis.base_module.as_deref()?;
    let def = base
        .definitions
        .iter()
        .flatten()
        .find(|d| d.name == *name)?;
    let range = text::span_to_range(&base.rope, def.name_span);
    let base_uri = Url::from_file_path(&base.path).ok()?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: base_uri,
        range,
    }))
}

/// Resolve a bare `name` (rule or variable) at `scope` to a `Location`
/// regardless of whether it binds locally or in an external module.
/// Helper rules and externals are reachable from the importer by bare name.
fn resolve_bare_name_to_location(
    analysis: &Module,
    name: &str,
    scope: Option<tree_sitter_generate::nativedsl::ast::Span>,
    current_uri: &Url,
) -> Option<Location> {
    match analysis.resolve_bare_name(name, scope)? {
        BindingLocation::Local(def) => Some(Location {
            uri: current_uri.clone(),
            range: text::span_to_range(&analysis.rope, def.name_span),
        }),
        BindingLocation::External { module, def } => Some(Location {
            uri: Url::from_file_path(&module.path).ok()?,
            range: text::span_to_range(&module.rope, def.name_span),
        }),
    }
}

/// Find the definition of an object field (e.g. `CALL` in `PREC.CALL`).
fn goto_object_field(
    analysis: &Module,
    uri: &Url,
    object_name: &str,
    field_name: &str,
) -> Option<GotoDefinitionResponse> {
    let definitions = analysis.definitions.as_ref()?;
    let let_def = definitions
        .iter()
        .find(|d| d.name == object_name && matches!(d.kind, DefKind::Let { .. }))?;
    let let_span = let_def.full_span;
    let key_def = definitions.iter().find(|d| {
        d.name == field_name
            && matches!(d.kind, DefKind::ObjectKey { .. })
            && d.name_span.start >= let_span.start
            && d.name_span.end <= let_span.end
    })?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range: text::span_to_range(&analysis.rope, key_def.name_span),
    }))
}

/// Jump to a member definition inside an imported module.
/// For `a::b::c`, path is `["a", "b"]` and member is `"c"`. Walks the
/// chain through nested sub-modules to find the target.
fn goto_imported_member(
    analysis: &Module,
    path: &[String],
    member: &str,
) -> Option<GotoDefinitionResponse> {
    let module_info = analysis.resolve_import_chain(path)?;
    let def = module_info
        .definitions
        .iter()
        .flatten()
        .find(|d| d.name == member)?;
    let uri = Url::from_file_path(&module_info.path).ok()?;
    let range = text::span_to_range(&module_info.rope, def.name_span);
    Some(GotoDefinitionResponse::Scalar(Location { uri, range }))
}
