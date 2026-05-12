use tower_lsp::lsp_types::{Location, ReferenceParams, Url};

use crate::document::{BindingLocation, CursorContext, Module, RefKind};
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn references(backend: &Backend, params: &ReferenceParams) -> Option<Vec<Location>> {
    let uri = &params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    let include_declaration = params.context.include_declaration;

    let (analysis, offset) = backend.resolve_position(uri, pos)?;
    let word = text::word_at_offset(&analysis.source, offset)?.to_owned();

    match analysis.cursor_context(offset, &analysis.source)? {
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
        CursorContext::Identifier { scope } => match analysis.resolve_bare_name(&word, scope) {
            Some(BindingLocation::External { module, .. }) => bare_name_external_references(
                backend,
                &analysis,
                uri,
                &module.path.clone(),
                &word,
                include_declaration,
            ),
            _ => local_references(
                &analysis,
                uri,
                &analysis.source,
                &analysis.rope,
                &word,
                scope,
                include_declaration,
            ),
        },
    }
}

/// References for a bare name (rule/macro/let) that resolves to a definition
/// in an external module - typically a rule defined in a helper that the
/// importer reaches by bare name. Includes the helper's own def + internal
/// refs and every open dependent's bare-name uses that bind to this target.
fn bare_name_external_references(
    backend: &Backend,
    cursor_analysis: &Module,
    cursor_uri: &Url,
    target_path: &std::path::Path,
    word: &str,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let mut locations = Vec::new();

    // Helper file: declaration + internal references.
    let target_module = cursor_analysis
        .base_module
        .as_deref()
        .filter(|m| m.path == target_path)
        .or_else(|| {
            cursor_analysis
                .import_modules
                .iter()
                .map(|(_, m)| m)
                .find(|m| m.path == target_path)
        })?;
    if let Ok(module_uri) = Url::from_file_path(&target_module.path) {
        for (span, is_decl) in target_module.bare_name_occurrences(word) {
            if !include_declaration && is_decl {
                continue;
            }
            locations.push(Location {
                uri: module_uri.clone(),
                range: text::span_to_range(&target_module.rope, span),
            });
        }
    }

    // Cursor file + open dependents.
    add_bare_name_refs_in_file(&mut locations, cursor_uri, cursor_analysis, target_path, word);
    let dep_uris: Vec<Url> = backend
        .dependents
        .get(target_path)
        .map(|set| set.iter().filter(|u| *u != cursor_uri).cloned().collect())
        .unwrap_or_default();
    for dep_uri in dep_uris {
        if let Some(dep_analysis) = backend.analysis_for_uri(&dep_uri) {
            add_bare_name_refs_in_file(
                &mut locations,
                &dep_uri,
                &dep_analysis,
                target_path,
                word,
            );
        }
    }

    (!locations.is_empty()).then_some(locations)
}

/// Append locations of bare-name references in `analysis` that bind to the
/// definition at `target_path`. Uses `resolve_bare_name` per-reference so
/// e.g. a local parameter shadowing the helper rule isn't mistakenly
/// matched.
fn add_bare_name_refs_in_file(
    out: &mut Vec<Location>,
    uri: &Url,
    analysis: &Module,
    target_path: &std::path::Path,
    word: &str,
) {
    for reference in analysis.references.iter().flatten() {
        let name_matches = matches!(
            &reference.kind,
            RefKind::Rule(name) | RefKind::Variable(name) if name == word,
        );
        if !name_matches {
            continue;
        }
        if matches!(
            analysis.resolve_bare_name(word, reference.scope),
            Some(BindingLocation::External { module, .. }) if module.path == target_path
        ) {
            out.push(Location {
                uri: uri.clone(),
                range: text::span_to_range(&analysis.rope, reference.span),
            });
        }
    }
}

/// References for `base::rule_name`: base grammar definition + base grammar
/// usages + derived file `BaseRule` refs.
fn base_rule_references(
    analysis: &Module,
    uri: &Url,
    rope: &ropey::Rope,
    word: &str,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let mut locations = Vec::new();

    if let Some(base) = analysis.base_module.as_deref()
        && let Ok(base_uri) = Url::from_file_path(&base.path)
    {
        // Includes inherited rules and macros - both can be referenced
        // unqualified from a derived grammar.
        for (span, is_decl) in base.bare_name_occurrences(word) {
            if !include_declaration && is_decl {
                continue;
            }
            locations.push(Location {
                uri: base_uri.clone(),
                range: text::span_to_range(&base.rope, span),
            });
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
    analysis: &Module,
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
        // Exclude qualified (`base::foo`, `mod::foo`) refs - those are
        // handled by the cross-module reference routes.
        if reference.kind.is_qualified() {
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
    analysis: &Module,
    uri: &Url,
    rope: &ropey::Rope,
    path: &[String],
    member: &str,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let mut locations = Vec::new();

    // Owning module: definition site + internal bare-name uses of `member`.
    if let Some(module_info) = analysis.resolve_import_chain(path)
        && let Ok(module_uri) = Url::from_file_path(&module_info.path)
    {
        for (span, is_decl) in module_info.bare_name_occurrences(member) {
            if !include_declaration && is_decl {
                continue;
            }
            locations.push(Location {
                uri: module_uri.clone(),
                range: text::span_to_range(&module_info.rope, span),
            });
        }
    }

    // Cursor file: call/access sites matched by exact qualified path + member.
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
