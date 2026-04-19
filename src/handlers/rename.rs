use std::collections::HashMap;

use tower_lsp::lsp_types::{
    PrepareRenameResponse, RenameParams, TextDocumentPositionParams, TextEdit, WorkspaceEdit,
};

use crate::document::{CursorContext, DefKind, RefKind};
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
        // Only regular identifiers are renameable within the current file.
        // Config fields, base rule accesses, and import member accesses
        // would require cross-file edits.
        CursorContext::GrammarConfigField
        | CursorContext::BaseRuleAccess
        | CursorContext::ImportModuleAccess { .. } => return None,
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

/// Rename a symbol and all its references within the current file.
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

    // Only rename regular identifiers (not base/import accesses).
    let CursorContext::Identifier { scope } = analysis.cursor_context(offset, &analysis.source)
    else {
        return None;
    };

    let mut edits = Vec::new();

    // Rename the definition site(s).
    for def in analysis.definitions.iter().flatten() {
        if def.name == word && def.kind.visible_from(scope) {
            edits.push(TextEdit {
                range: text::span_to_range(&analysis.rope, def.name_span),
                new_text: new_name.clone(),
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
                new_text: new_name.clone(),
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
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
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
