use std::collections::HashMap;

use ropey::Rope;
use tower_lsp::lsp_types::{
    PrepareRenameResponse, RenameParams, TextDocumentPositionParams, TextEdit, Url, WorkspaceEdit,
};
use tree_sitter_generate::nativedsl::ast;

use crate::document::{BindingLocation, CursorContext, DefKind, ExternalModuleInfo, RefKind};
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
    // Validate + normalize the new name. Accepts `foo`, `r#let`, or bare
    // keywords; returns the bare identifier (auto-escape happens at edit
    // emission time).
    let new_name = parse_rename_target(&params.new_name)?;

    let analysis = backend.get_analysis(uri)?;
    let offset = text::position_to_offset(&analysis.rope, pos)?;
    let word = text::word_at_offset(&analysis.source, offset)?;

    match analysis.cursor_context(offset, &analysis.source) {
        CursorContext::BaseRuleAccess => {
            let target_path = analysis.base_module.as_ref()?.path.clone();
            rename_cross_file(backend, uri, &analysis, &target_path, word, &new_name)
        }
        CursorContext::ImportModuleAccess { .. } => {
            let qualifier = text::qualified_access_module(
                analysis.tokens.as_deref()?,
                &analysis.source,
                offset,
            )?;
            let target_path = analysis.get_module(qualifier)?.path.clone();
            rename_cross_file(backend, uri, &analysis, &target_path, word, &new_name)
        }
        CursorContext::Identifier { scope } => match analysis.resolve_bare_name(word, scope) {
            Some(BindingLocation::External { module, .. }) => rename_cross_file(
                backend,
                uri,
                &analysis,
                &module.path.clone(),
                word,
                &new_name,
            ),
            _ => rename_local(uri, &analysis, word, &new_name, scope),
        },
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
            external_edits.push(make_rename_edit(
                &target_module.rope,
                def.name_span,
                new_name,
            ));
        }
    }
    for reference in &target_module.references {
        let name_matches = match &reference.kind {
            RefKind::Rule(name) | RefKind::Variable(name) => name == word,
            _ => false,
        };
        if name_matches {
            external_edits.push(make_rename_edit(
                &target_module.rope,
                reference.span,
                new_name,
            ));
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
    // A `QualifiedCall` like `h::foo` produces two overlapping references at
    // the `foo` span: an inner `Variable("foo")` (the resolved Ident) and an
    // outer `ImportedMember`. Both can match the target binding. Track which
    // spans we've already emitted so we don't double-edit.
    let mut local_edits = Vec::new();
    let mut edited_starts = rustc_hash::FxHashSet::default();
    for reference in analysis.references.iter().flatten() {
        if edited_starts.contains(&reference.span.start) {
            continue;
        }
        let bound_to_target = match &reference.kind {
            RefKind::BaseRule(name) if name == word => analysis
                .base_module
                .as_ref()
                .is_some_and(|m| m.path == target_path),
            RefKind::ImportedMember { path, member } if member == word => analysis
                .resolve_import_chain(path)
                .is_some_and(|m| m.path == target_path),
            // Bare-name reference (e.g. `shared_rule` in the importer's body)
            // - resolve through the importer's analysis and check the binding
            // lands in the target module.
            RefKind::Rule(name) | RefKind::Variable(name) if name == word => matches!(
                analysis.resolve_bare_name(word, reference.scope),
                Some(BindingLocation::External { module, .. }) if module.path == target_path
            ),
            _ => false,
        };
        if bound_to_target {
            local_edits.push(make_rename_edit(&analysis.rope, reference.span, new_name));
            edited_starts.insert(reference.span.start);
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
            edits.push(make_rename_edit(&analysis.rope, def.name_span, new_name));
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
            edits.push(make_rename_edit(&analysis.rope, reference.span, new_name));
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

/// Validate `name` and return the *bare* identifier it represents.
///
/// Accepts:
///   * a regular identifier (`foo`, `_x`),
///   * a raw-prefixed identifier (`r#let` -> bare name `let`),
///   * a bare keyword (`let` -> bare name `let`; the caller will auto-escape).
///
/// Rejects empty strings, leading digits, whitespace, and anything else that
/// doesn't lex cleanly to a single ident-or-keyword token.
fn parse_rename_target(name: &str) -> Option<String> {
    use tree_sitter_generate::nativedsl::lexer::{Lexer, TokenKind};
    let tokens = Lexer::new(name).tokenize().ok()?;
    let [t, eof] = tokens.as_slice() else {
        return None;
    };
    if eof.kind != TokenKind::Eof {
        return None;
    }
    if t.kind == TokenKind::Ident || t.kind.is_keyword() {
        // Raw idents have their `r#` stripped by the lexer (span = bare name);
        // keywords' span is their literal text. Either way, span is the bare
        // name we want to emit (with auto-escape on output if needed).
        Some(name[t.span.start as usize..t.span.end as usize].to_owned())
    } else {
        None
    }
}

/// True if `name` lexes as a DSL keyword (so emitting it as a raw identifier
/// in source requires an `r#` prefix).
fn name_is_keyword(name: &str) -> bool {
    use tree_sitter_generate::nativedsl::lexer::Lexer;
    Lexer::new(name)
        .tokenize()
        .ok()
        .and_then(|tokens| tokens.first().map(|t| t.kind.is_keyword()))
        .unwrap_or(false)
}

/// Build a rename `TextEdit` for an identifier at `span` in `rope`.
///
/// Handles raw-identifier syntax: if the existing identifier is preceded by
/// `r#` in source, the edit range is extended backward so the result drops
/// the prefix when not needed. If `bare_new_name` is itself a DSL keyword,
/// the `new_text` is prepended with `r#` so the output stays valid.
fn make_rename_edit(rope: &Rope, span: ast::Span, bare_new_name: &str) -> TextEdit {
    let has_raw_prefix = span.start >= 2
        && rope.byte((span.start - 2) as usize) == b'r'
        && rope.byte((span.start - 1) as usize) == b'#';
    let start = if has_raw_prefix {
        span.start - 2
    } else {
        span.start
    };
    let new_text = if name_is_keyword(bare_new_name) {
        format!("r#{bare_new_name}")
    } else {
        bare_new_name.to_owned()
    };
    TextEdit {
        range: text::span_to_range(rope, ast::Span::new(start, span.end)),
        new_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rename_target_accepts_plain_idents() {
        assert_eq!(parse_rename_target("foo"), Some("foo".into()));
        assert_eq!(parse_rename_target("_bar"), Some("_bar".into()));
        assert_eq!(parse_rename_target("foo_bar"), Some("foo_bar".into()));
        assert_eq!(parse_rename_target("x1"), Some("x1".into()));
        assert_eq!(parse_rename_target("_"), Some("_".into()));
    }

    #[test]
    fn parse_rename_target_strips_raw_prefix() {
        // Lexer skips `r#` and the span covers just the bare name.
        assert_eq!(parse_rename_target("r#foo"), Some("foo".into()));
        assert_eq!(parse_rename_target("r#let"), Some("let".into()));
    }

    #[test]
    fn parse_rename_target_accepts_keywords_for_auto_escape() {
        // The caller will prefix with `r#` at edit emission time.
        assert_eq!(parse_rename_target("let"), Some("let".into()));
        assert_eq!(parse_rename_target("rule"), Some("rule".into()));
        assert_eq!(parse_rename_target("macro"), Some("macro".into()));
    }

    #[test]
    fn parse_rename_target_rejects_invalid() {
        assert_eq!(parse_rename_target(""), None);
        assert_eq!(parse_rename_target("1foo"), None);
        assert_eq!(parse_rename_target("foo bar"), None);
        assert_eq!(parse_rename_target("foo-bar"), None);
        assert_eq!(parse_rename_target("foo::bar"), None);
        assert_eq!(parse_rename_target("foo.bar"), None);
    }
}
