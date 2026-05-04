use std::collections::HashMap;

use tower_lsp::lsp_types::{
    PrepareRenameResponse, RenameParams, TextDocumentPositionParams, TextEdit, Url, WorkspaceEdit,
};

use ropey::Rope;

use crate::document::{CursorContext, DefKind, DiagnosticCache, Document, ExternalModuleInfo, RefKind};
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

    let offset = {
        let doc = backend.document_map.get(uri)?;
        text::position_to_offset(&doc.rope, pos)?
    };
    let analysis = backend.get_analysis(uri)?;
    let word = text::word_at_offset(&analysis.source, offset)?;

    match analysis.cursor_context(offset, &analysis.source) {
        CursorContext::GrammarConfigField => return None,
        CursorContext::BaseRuleAccess => {
            // Cursor is on the member part of `base::rule_name`.
            // Find the definition in the base module.
            let base = analysis.base_module.as_ref()?;
            let def = base.definitions.iter().find(|d| d.name == word)?;
            let range = text::span_to_range(&base.rope, def.name_span);
            return Some(PrepareRenameResponse::Range(range));
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
            return Some(PrepareRenameResponse::Range(range));
        }
        CursorContext::Identifier { .. } => {}
    }

    // Check if the word matches a definition we can rename.
    let def = analysis
        .definitions
        .as_ref()?
        .iter()
        .find(|d| d.name == word)?;

    // Only rename user-defined names, not builtins or object keys.
    match def.kind {
        DefKind::Rule
        | DefKind::OverrideRule
        | DefKind::Function { .. }
        | DefKind::Let { .. }
        | DefKind::Parameter { .. } => {}
        DefKind::Import | DefKind::Inherit | DefKind::ObjectKey => return None,
    }

    let range = text::span_to_range(&analysis.rope, def.name_span);
    Some(PrepareRenameResponse::Range(range))
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

    let offset = {
        let doc = backend.document_map.get(uri)?;
        text::position_to_offset(&doc.rope, pos)?
    };
    let analysis = backend.get_analysis(uri)?;
    let word = text::word_at_offset(&analysis.source, offset)?;

    match analysis.cursor_context(offset, &analysis.source) {
        CursorContext::BaseRuleAccess => {
            let base = analysis.base_module.as_ref()?;
            let (edit, new_source) = rename_cross_file(
                uri,
                &analysis,
                base,
                word,
                new_name,
                |r| matches!(&r.kind, RefKind::BaseRule(name) if name == word),
            )?;
            seed_external_document(backend, &base.path, new_source);
            Some(edit)
        }
        CursorContext::ImportModuleAccess { .. } => {
            let qualifier = text::qualified_access_module(
                analysis.tokens.as_deref()?,
                &analysis.source,
                offset,
            )?;
            let module = analysis.get_module(qualifier)?;
            let (edit, new_source) = rename_cross_file(
                uri,
                &analysis,
                module,
                word,
                new_name,
                |r| matches!(&r.kind, RefKind::ImportedMember { member, .. } if member == word),
            )?;
            seed_external_document(backend, &module.path, new_source);
            Some(edit)
        }
        CursorContext::Identifier { scope } => rename_local(uri, &analysis, word, new_name, scope),
        CursorContext::GrammarConfigField => None,
    }
}

/// Insert the post-rename content of an external file into the document_map.
/// The analysis pipeline prefers document_map content over disk, so subsequent
/// `get_analysis` calls will pick up the renamed symbols immediately.
fn seed_external_document(backend: &Backend, path: &std::path::Path, content: String) {
    if let Ok(uri) = Url::from_file_path(path) {
        backend.document_map.insert(
            uri,
            Document {
                rope: Rope::from_str(&content),
                text: content,
                version: 0,
                diagnostics: DiagnosticCache::default(),
                analysis: None,
            },
        );
    }
}

/// Rename a symbol defined in an external module (base or imported).
/// Edits the definition + references in the external file, and all matching
/// cross-module references in the current file.
/// Returns (WorkspaceEdit, new external source text).
fn rename_cross_file(
    current_uri: &Url,
    analysis: &crate::document::Analysis,
    module: &ExternalModuleInfo,
    word: &str,
    new_name: &str,
    current_file_ref_filter: impl Fn(&crate::document::Reference) -> bool,
) -> Option<(WorkspaceEdit, String)> {
    let external_uri = Url::from_file_path(&module.path).ok()?;

    // Collect edits in the external file: definition + all references.
    // Track both LSP TextEdits (for the WorkspaceEdit) and byte offsets
    // (to compute the new source text for the document_map).
    let mut external_edits = Vec::new();
    let mut byte_edits: Vec<(usize, usize)> = Vec::new();
    for def in &module.definitions {
        if def.name == word {
            external_edits.push(TextEdit {
                range: text::span_to_range(&module.rope, def.name_span),
                new_text: new_name.into(),
            });
            byte_edits.push((def.name_span.start as usize, def.name_span.end as usize));
        }
    }
    for reference in &module.references {
        let name_matches = match &reference.kind {
            RefKind::Rule(name) | RefKind::Variable(name) => name == word,
            _ => false,
        };
        if name_matches {
            external_edits.push(TextEdit {
                range: text::span_to_range(&module.rope, reference.span),
                new_text: new_name.into(),
            });
            byte_edits.push((reference.span.start as usize, reference.span.end as usize));
        }
    }

    // Collect edits in the current file: cross-module references.
    let mut current_edits = Vec::new();
    for reference in analysis.references.iter().flatten() {
        if current_file_ref_filter(reference) {
            current_edits.push(TextEdit {
                range: text::span_to_range(&analysis.rope, reference.span),
                new_text: new_name.into(),
            });
        }
    }

    if external_edits.is_empty() && current_edits.is_empty() {
        return None;
    }

    // Compute the new external source by applying byte edits back-to-front.
    let mut new_source = module.rope.to_string();
    byte_edits.sort_by(|a, b| b.0.cmp(&a.0));
    for (start, end) in &byte_edits {
        new_source.replace_range(*start..*end, new_name);
    }

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    if !external_edits.is_empty() {
        changes.insert(external_uri, external_edits);
    }
    if !current_edits.is_empty() {
        changes.insert(current_uri.clone(), current_edits);
    }
    Some((
        WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        },
        new_source,
    ))
}

/// Rename a locally-defined symbol within the current file.
fn rename_local(
    uri: &Url,
    analysis: &crate::document::Analysis,
    word: &str,
    new_name: &str,
    scope: Option<tree_sitter_generate::nativedsl::ast::Span>,
) -> Option<WorkspaceEdit> {
    let mut edits = Vec::new();

    // Rename the definition site(s).
    for def in analysis.definitions.iter().flatten() {
        if def.name == word && def.kind.visible_from(scope) {
            edits.push(TextEdit {
                range: text::span_to_range(&analysis.rope, def.name_span),
                new_text: new_name.into(),
            });
        }
    }

    // Rename all reference sites.
    for reference in analysis.references.iter().flatten() {
        // Skip cross-module references.
        if matches!(
            reference.kind,
            RefKind::BaseRule(_) | RefKind::ImportedMember { .. }
        ) {
            continue;
        }
        if reference.matches_word(word, &analysis.source, scope) {
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

/// Check if a string is a valid DSL identifier (alphanumeric + underscores,
/// not starting with a digit).
fn is_valid_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
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
