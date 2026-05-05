use tower_lsp::lsp_types::{Location, ReferenceParams, Url};

use crate::document::{CursorContext, RefKind};
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn references(backend: &Backend, params: &ReferenceParams) -> Option<Vec<Location>> {
    let uri = &params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    let include_declaration = params.context.include_declaration;

    let analysis = backend.get_analysis(uri)?;
    let offset = text::position_to_offset(&analysis.rope, pos)?;
    let word = text::word_at_offset(&analysis.source, offset)?.to_owned();

    match analysis.cursor_context(offset, &analysis.source) {
        // Grammar config fields aren't referenceable.
        CursorContext::GrammarConfigField => None,
        CursorContext::BaseRuleAccess => {
            base_rule_references(&analysis, uri, &analysis.rope, &word, include_declaration)
        }
        CursorContext::ImportModuleAccess { .. } => {
            // Recover the cursor's qualified path so we can disambiguate
            // between e.g. `a::foo` and `b::foo`.
            let cursor_ref = analysis.reference_at(offset)?;
            let RefKind::ImportedMember { path, member } = &cursor_ref.kind else {
                return None;
            };
            import_member_references(
                &analysis,
                uri,
                &analysis.rope,
                path,
                member,
                include_declaration,
            )
        }
        CursorContext::Identifier { scope } => local_references(
            &analysis,
            uri,
            &analysis.source,
            &analysis.rope,
            &word,
            scope,
            include_declaration,
        ),
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

    if let Some(base) = &analysis.base_module
        && let Ok(base_uri) = Url::from_file_path(&base.path)
    {
        if include_declaration && let Some(def) = base.definitions.iter().find(|d| d.name == word) {
            locations.push(Location {
                uri: base_uri.clone(),
                range: text::span_to_range(&base.rope, def.name_span),
            });
        }
        for reference in &base.references {
            // Match both rule references and macro/variable references by
            // name - inherited names can be either rules or macros.
            if matches!(
                &reference.kind,
                RefKind::Rule(name) | RefKind::Variable(name) if name == word,
            ) {
                locations.push(Location {
                    uri: base_uri.clone(),
                    range: text::span_to_range(&base.rope, reference.span),
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
    source: &str,
    rope: &ropey::Rope,
    word: &str,
    cursor_scope: Option<tree_sitter_generate::nativedsl::ast::Span>,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let mut locations = Vec::new();

    if include_declaration {
        for def in analysis.definitions.iter().flatten() {
            if def.name == word && def.kind.visible_from(cursor_scope) {
                locations.push(Location {
                    uri: uri.clone(),
                    range: text::span_to_range(rope, def.name_span),
                });
            }
        }
    }

    for reference in analysis.references.iter().flatten() {
        // Exclude BaseRule and ImportedMember references - they refer to
        // external modules, not local definitions.
        if matches!(
            reference.kind,
            RefKind::BaseRule(_) | RefKind::ImportedMember { .. }
        ) {
            continue;
        }
        if reference.matches_word(word, source, cursor_scope) {
            locations.push(Location {
                uri: uri.clone(),
                range: text::span_to_range(rope, reference.span),
            });
        }
    }

    (!locations.is_empty()).then_some(locations)
}

/// References for a member accessed through an imported module chain
/// (`a::b::member`). Filters by full qualified path so `a::foo` and `b::foo`
/// don't collide.
fn import_member_references(
    analysis: &crate::document::Analysis,
    uri: &Url,
    rope: &ropey::Rope,
    path: &[String],
    member: &str,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let mut locations = Vec::new();

    if include_declaration
        && let Some(module_info) = analysis.resolve_import_chain(path)
        && let Some(def) = module_info.definitions.iter().find(|d| d.name == member)
        && let Ok(module_uri) = Url::from_file_path(&module_info.path)
    {
        locations.push(Location {
            uri: module_uri,
            range: text::span_to_range(&module_info.rope, def.name_span),
        });
    }

    // Match call/access sites by exact qualified path AND member name.
    for reference in analysis.references.iter().flatten() {
        if let RefKind::ImportedMember {
            path: ref_path,
            member: ref_member,
        } = &reference.kind
            && ref_path.as_slice() == path
            && ref_member == member
        {
            locations.push(Location {
                uri: uri.clone(),
                range: text::span_to_range(rope, reference.span),
            });
        }
    }

    (!locations.is_empty()).then_some(locations)
}
