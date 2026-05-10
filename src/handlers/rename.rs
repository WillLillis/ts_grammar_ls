use std::collections::HashMap;

use tower_lsp::lsp_types::{
    PrepareRenameResponse, RenameParams, TextDocumentPositionParams, TextEdit, Url, WorkspaceEdit,
};

use crate::document::{CursorContext, DefKind, ExternalModuleInfo, RefKind};
use crate::server::Backend;
use crate::text;

/// Check if the symbol at the cursor can be renamed, and return its range.
#[must_use]
pub fn prepare_rename(
    backend: &Backend,
    params: &TextDocumentPositionParams,
) -> Option<PrepareRenameResponse> {
    let uri = &params.text_document.uri;
    let pos = params.position;

    let analysis = backend.get_analysis(uri)?;
    let offset = text::position_to_offset(&analysis.rope, pos)?;
    let word = text::word_at_offset(&analysis.source, offset)?;

    match analysis.cursor_context(offset, &analysis.source) {
        CursorContext::GrammarConfigField => None,
        CursorContext::BaseRuleAccess => {
            // Cursor is on the member part of `base::rule_name`.
            // Find the definition in the base module.
            let base = analysis.base_module.as_ref()?;
            let def = base.definitions.iter().find(|d| d.name == word)?;
            let range = text::span_to_range(&base.rope, def.name_span);
            Some(PrepareRenameResponse::Range(range))
        }
        CursorContext::ImportModuleAccess { .. } => {
            // Cursor is on the member part of `mod::member`.
            let qualifier = text::qualified_access_module(
                analysis.tokens.as_deref()?,
                &analysis.source,
                offset,
            )?;
            let module = analysis.get_module(qualifier)?;
            let def = module.definitions.iter().find(|d| d.name == word)?;
            let range = text::span_to_range(&module.rope, def.name_span);
            Some(PrepareRenameResponse::Range(range))
        }
        CursorContext::Identifier { scope } => {
            // Resolve the cursor word to its binding using lexical scoping
            // (innermost match wins under shadowing).
            let def = analysis.binding_for(word, scope)?;
            // Only rename user-defined names, not builtins or object keys.
            match def.kind {
                DefKind::Rule
                | DefKind::OverrideRule
                | DefKind::Function { .. }
                | DefKind::Let { .. }
                | DefKind::Parameter { .. }
                | DefKind::External => {}
                DefKind::Import | DefKind::Inherit | DefKind::ObjectKey => return None,
            }
            let range = text::span_to_range(&analysis.rope, def.name_span);
            Some(PrepareRenameResponse::Range(range))
        }
    }
}

/// Rename a symbol and all its references, potentially across files.
#[must_use]
pub fn rename(backend: &Backend, params: &RenameParams) -> Option<WorkspaceEdit> {
    let uri = &params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    let new_name = &params.new_name;

    // Validate the new name is a valid identifier.
    if !is_valid_identifier(new_name) {
        return None;
    }

    let analysis = backend.get_analysis(uri)?;
    let offset = text::position_to_offset(&analysis.rope, pos)?;
    let word = text::word_at_offset(&analysis.source, offset)?;

    match analysis.cursor_context(offset, &analysis.source) {
        CursorContext::BaseRuleAccess => {
            let target_path = analysis.base_module.as_ref()?.path.clone();
            rename_cross_file(backend, uri, &analysis, &target_path, word, new_name)
        }
        CursorContext::ImportModuleAccess { .. } => {
            let qualifier = text::qualified_access_module(
                analysis.tokens.as_deref()?,
                &analysis.source,
                offset,
            )?;
            let target_path = analysis.get_module(qualifier)?.path.clone();
            rename_cross_file(backend, uri, &analysis, &target_path, word, new_name)
        }
        CursorContext::Identifier { scope } => rename_local(uri, &analysis, word, new_name, scope),
        CursorContext::GrammarConfigField => None,
    }
}

/// Rename a symbol defined in an external module identified by `target_path`
/// (an inherited base or imported helper). Produces edits for:
///   * the definition site + internal references in the external file,
///   * the cursor file's cross-module references that bind to this target,
///   * every other open file whose latest analysis depends on `target_path`
///     and which has cross-module references binding to this target.
///
/// References are matched by full qualified path (resolved through each
/// dependent's own analysis), so `helpers::foo` and `other::foo` don't
/// collide even if both are imports in the same file.
fn rename_cross_file(
    backend: &Backend,
    cursor_uri: &Url,
    cursor_analysis: &crate::document::Analysis,
    target_path: &std::path::Path,
    word: &str,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

    // External file: definition site + internal refs (Rule/Variable kinds).
    let target_module = find_external_module(cursor_analysis, target_path)?;
    let mut external_edits = Vec::new();
    for def in &target_module.definitions {
        if def.name == word {
            external_edits.push(TextEdit {
                range: text::span_to_range(&target_module.rope, def.name_span),
                new_text: new_name.into(),
            });
        }
    }
    for reference in &target_module.references {
        let name_matches = match &reference.kind {
            RefKind::Rule(name) | RefKind::Variable(name) => name == word,
            _ => false,
        };
        if name_matches {
            external_edits.push(TextEdit {
                range: text::span_to_range(&target_module.rope, reference.span),
                new_text: new_name.into(),
            });
        }
    }
    if !external_edits.is_empty() {
        let external_uri = Url::from_file_path(target_path).ok()?;
        changes.insert(external_uri, external_edits);
    }

    // Cursor file: refs that bind to target.
    add_cross_refs_in_file(
        &mut changes,
        cursor_uri,
        cursor_analysis,
        target_path,
        word,
        new_name,
    );

    // Open dependents (excluding cursor file): same treatment.
    let dep_uris: Vec<Url> = backend
        .dependents
        .get(target_path)
        .map(|set| set.iter().filter(|u| *u != cursor_uri).cloned().collect())
        .unwrap_or_default();
    for dep_uri in dep_uris {
        if let Some(dep_analysis) = backend.analysis_for_uri(&dep_uri) {
            add_cross_refs_in_file(
                &mut changes,
                &dep_uri,
                &dep_analysis,
                target_path,
                word,
                new_name,
            );
        }
    }

    if changes.is_empty() {
        return None;
    }
    Some(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    })
}

/// Find the `ExternalModuleInfo` matching `target_path` reachable from
/// `analysis` - either the inherited base or any (transitively) imported
/// module. The same-module check uses canonical paths so different binding
/// names across files still resolve to the same module.
fn find_external_module<'a>(
    analysis: &'a crate::document::Analysis,
    target_path: &std::path::Path,
) -> Option<&'a ExternalModuleInfo> {
    fn walk<'a>(
        info: &'a ExternalModuleInfo,
        target: &std::path::Path,
    ) -> Option<&'a ExternalModuleInfo> {
        if info.path == target {
            return Some(info);
        }
        for (_, sub) in &info.import_modules {
            if let Some(found) = walk(sub, target) {
                return Some(found);
            }
        }
        None
    }
    if let Some(base) = &analysis.base_module
        && let Some(found) = walk(base, target_path)
    {
        return Some(found);
    }
    for (_, info) in &analysis.import_modules {
        if let Some(found) = walk(info, target_path) {
            return Some(found);
        }
    }
    None
}

/// Append edits for all references in `analysis` that bind to the symbol
/// named `word` in the module at `target_path`. Matches both `BaseRule`
/// (when `analysis`'s inherited base is the target) and `ImportedMember`
/// (when the qualified path resolves through to the target) by canonical
/// path - never by binding name.
fn add_cross_refs_in_file(
    changes: &mut HashMap<Url, Vec<TextEdit>>,
    uri: &Url,
    analysis: &crate::document::Analysis,
    target_path: &std::path::Path,
    word: &str,
    new_name: &str,
) {
    let mut local_edits = Vec::new();
    for reference in analysis.references.iter().flatten() {
        let bound_to_target = match &reference.kind {
            RefKind::BaseRule(name) if name == word => analysis
                .base_module
                .as_ref()
                .is_some_and(|m| m.path == target_path),
            RefKind::ImportedMember { path, member } if member == word => analysis
                .resolve_import_chain(path)
                .is_some_and(|m| m.path == target_path),
            _ => false,
        };
        if bound_to_target {
            local_edits.push(TextEdit {
                range: text::span_to_range(&analysis.rope, reference.span),
                new_text: new_name.into(),
            });
        }
    }
    if !local_edits.is_empty() {
        changes.entry(uri.clone()).or_default().extend(local_edits);
    }
}

/// Rename a locally-defined symbol within the current file.
///
/// Resolves the cursor name to its binding using lexical scoping, then for
/// each occurrence (definition or reference) verifies it binds to the same
/// target before including it. This correctly handles shadowing: only the
/// occurrences that resolve to the clicked binding are renamed.
fn rename_local(
    uri: &Url,
    analysis: &crate::document::Analysis,
    word: &str,
    new_name: &str,
    cursor_scope: Option<tree_sitter_generate::nativedsl::ast::Span>,
) -> Option<WorkspaceEdit> {
    let target = analysis.binding_for(word, cursor_scope)?;
    let target_id = target.name_span;

    let mut edits = Vec::new();

    // Definition sites: a same-named def is the same binding iff it shares
    // the target's scope. (Two top-level defs with the same name would be a
    // resolver error; two scoped defs with the same scope span are the same
    // binding.)
    for def in analysis.definitions.iter().flatten() {
        if def.name == word && def.kind.scope() == target.kind.scope() {
            edits.push(TextEdit {
                range: text::span_to_range(&analysis.rope, def.name_span),
                new_text: new_name.into(),
            });
        }
    }

    // Reference sites: re-resolve each candidate at its own enclosing scope
    // and include only if it binds to the target.
    for reference in analysis.references.iter().flatten() {
        if matches!(
            reference.kind,
            RefKind::BaseRule(_) | RefKind::ImportedMember { .. }
        ) {
            continue;
        }
        let ref_name = match &reference.kind {
            RefKind::Rule(n) | RefKind::Variable(n) => n.as_str(),
            RefKind::ObjectField { field, .. } => field.as_str(),
            RefKind::Builtin => {
                &analysis.source[reference.span.start as usize..reference.span.end as usize]
            }
            RefKind::InheritPath | RefKind::ImportPath => continue,
            RefKind::BaseRule(_) | RefKind::ImportedMember { .. } => unreachable!(),
        };
        if ref_name != word {
            continue;
        }
        if analysis
            .binding_for(word, reference.scope)
            .is_some_and(|d| d.name_span == target_id)
        {
            edits.push(TextEdit {
                range: text::span_to_range(&analysis.rope, reference.span),
                new_text: new_name.into(),
            });
        }
    }

    if edits.is_empty() {
        return None;
    }

    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    Some(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    })
}

/// Check that `name` is a valid DSL identifier: it must lex as exactly one
/// `Ident` token (with EOF following), which rules out empty strings, leading
/// digits, whitespace, punctuation, and DSL keywords like `rule`/`let`/`macro`.
fn is_valid_identifier(name: &str) -> bool {
    use tree_sitter_generate::nativedsl::lexer::{Lexer, TokenKind};
    let Ok(tokens) = Lexer::new(name).tokenize() else {
        return false;
    };
    matches!(
        tokens.as_slice(),
        [t, eof] if t.kind == TokenKind::Ident && eof.kind == TokenKind::Eof
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_identifiers() {
        assert!(is_valid_identifier("foo"));
        assert!(is_valid_identifier("_bar"));
        assert!(is_valid_identifier("foo_bar"));
        assert!(is_valid_identifier("x1"));
        assert!(is_valid_identifier("_"));
    }

    #[test]
    fn invalid_identifiers() {
        assert!(!is_valid_identifier(""));
        assert!(!is_valid_identifier("1foo"));
        assert!(!is_valid_identifier("foo bar"));
        assert!(!is_valid_identifier("foo-bar"));
        assert!(!is_valid_identifier("foo::bar"));
        assert!(!is_valid_identifier("foo.bar"));
    }
}
