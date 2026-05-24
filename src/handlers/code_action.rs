//! `textDocument/codeAction` handler.
//!
//! Provides two refactorings:
//!
//! - **Convert string to raw string** (`"..."` to `r#"..."#`) with the minimum
//!   number of `#` delimiters needed to disambiguate embedded `"` sequences.
//! - **Inline rule-set macro call**: when the cursor is on a top-level
//!   `Node::Call` to a rule-set macro (one that generates rule decls), expand
//!   it in place to the rules it produces. The loader's `expand_macro_calls`
//!   already replaced the call in `root_items` with `Node::ExpandedRule`s
//!   carrying the original call's span; we find those and render them.

use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
    TextEdit, WorkspaceEdit, Url,
};
use tree_sitter_generate::nativedsl::ast::{Node, Span};
use tree_sitter_generate::nativedsl::lexer::TokenKind;

use crate::config::FormattingConfig;
use crate::document::Module;
use crate::formatter;
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn code_action(backend: &Backend, params: &CodeActionParams) -> Option<CodeActionResponse> {
    let uri = &params.text_document.uri;

    // If the client filtered by `only`, skip if it didn't ask for refactors.
    if let Some(kinds) = &params.context.only
        && !kinds.iter().any(is_refactor_kind)
    {
        return Some(Vec::new());
    }

    let (analysis, start_offset) = backend.resolve_position(uri, params.range.start)?;

    let mut actions: Vec<CodeActionOrCommand> = Vec::new();
    if let Some(a) = build_raw_string_action(&analysis, start_offset, uri) {
        actions.push(CodeActionOrCommand::CodeAction(a));
    }
    if let Some(a) = build_inline_macro_action(&analysis, start_offset, uri) {
        actions.push(CodeActionOrCommand::CodeAction(a));
    }
    if actions.is_empty() {
        None
    } else {
        Some(actions)
    }
}

fn build_raw_string_action(
    analysis: &Module,
    start_offset: u32,
    uri: &Url,
) -> Option<CodeAction> {
    // Find a StringLit token covering the cursor in the cached tokens. If lex
    // never produced tokens (very early state), bail.
    let tokens = analysis.tokens.as_deref()?;
    let tok = tokens
        .iter()
        .find(|t| t.span.start <= start_offset && start_offset < t.span.end)?;
    if tok.kind != TokenKind::StringLit {
        return None;
    }
    let span = tok.span;

    // Extract content (without surrounding quotes). Only offer the action if
    // the string has escapes that are faithfully representable in raw form -
    // that's `\\` and `\"`. Semantic escapes (`\n`, `\t`, `\r`, `\0`) change
    // meaning in a raw string (they'd become literal two-char sequences), so
    // we skip those to avoid silent semantic changes.
    let raw_body = analysis
        .source
        .get((span.start + 1) as usize..(span.end - 1) as usize)?;
    if !has_only_raw_safe_escapes(raw_body) {
        return None;
    }
    let decoded = decode_escapes(raw_body);
    let hashes = hashes_needed(&decoded);
    let mut new_text = String::with_capacity(decoded.len() + 4 + usize::from(hashes) * 2);
    new_text.push('r');
    for _ in 0..hashes {
        new_text.push('#');
    }
    new_text.push('"');
    new_text.push_str(&decoded);
    new_text.push('"');
    for _ in 0..hashes {
        new_text.push('#');
    }

    let edit_range = text::span_to_range(&analysis.rope, span);
    let edits = std::collections::HashMap::from([(
        uri.clone(),
        vec![TextEdit {
            range: edit_range,
            new_text,
        }],
    )]);

    Some(CodeAction {
        title: "Convert to raw string".into(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        edit: Some(WorkspaceEdit {
            changes: Some(edits),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: None,
        command: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

/// "Inline rule-set macro call": when the cursor is inside a top-level
/// macro call that the loader expanded into `Node::ExpandedRule`s, replace
/// the call span with the rendered rule decls.
fn build_inline_macro_action(
    analysis: &Module,
    start_offset: u32,
    uri: &Url,
) -> Option<CodeAction> {
    // Only meaningful when the loader actually ran expansion. The manual
    // parse fallback never produces `ExpandedRule` nodes.
    if !analysis.loader_succeeded {
        return None;
    }
    // Collect the `ExpandedRule`s synthesized for one call. They all share
    // the original call's source span - so the first hit anchors the span
    // and we gather every sibling carrying the same span. Sibling rules
    // appear contiguously in `root_items` (expand_macro_calls writes them
    // in order).
    let arena = &analysis.shared.arena;
    let mut hit_span: Option<Span> = None;
    let mut rule_ids: Vec<tree_sitter_generate::nativedsl::ast::NodeId> = Vec::new();
    for &id in &analysis.root_items {
        if !matches!(arena.get(id), Node::ExpandedRule { .. }) {
            continue;
        }
        let span = arena.span(id);
        match hit_span {
            Some(s) if s == span => rule_ids.push(id),
            Some(_) => {
                // Past the run sharing the cursor's call span.
                if !rule_ids.is_empty() {
                    break;
                }
            }
            None => {
                if span.start <= start_offset && start_offset < span.end {
                    hit_span = Some(span);
                    rule_ids.push(id);
                }
            }
        }
    }
    let call_span = hit_span?;

    // Render each ExpandedRule as `[override ]rule NAME { BODY }` via the
    // formatter's expansion mode (so `Node::SynthRef` lookups against the
    // strings table work). Empty `args` slice: the loader has already
    // substituted the call's args structurally, so no MacroParams remain
    // in the bodies.
    let config = FormattingConfig::default();
    let mut parts: Vec<String> = Vec::with_capacity(rule_ids.len());
    for rid in rule_ids {
        let rendered = formatter::format_macro_expansion(
            rid,
            &[],
            analysis,
            analysis,
            &config,
        );
        parts.push(rendered);
    }
    let new_text = parts.join("\n\n");
    let edit_range = text::span_to_range(&analysis.rope, call_span);
    let edits = std::collections::HashMap::from([(
        uri.clone(),
        vec![TextEdit {
            range: edit_range,
            new_text,
        }],
    )]);

    Some(CodeAction {
        title: "Inline macro call".into(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        edit: Some(WorkspaceEdit {
            changes: Some(edits),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: None,
        command: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

fn is_refactor_kind(k: &CodeActionKind) -> bool {
    *k == CodeActionKind::REFACTOR_REWRITE
        || *k == CodeActionKind::REFACTOR
        || *k == CodeActionKind::EMPTY
}

/// Returns `true` if `s` has at least one escape sequence and all escapes are
/// `\\` or `\"` (the only escapes whose meaning is preserved in a raw string).
/// Returns `false` if there are no escapes, or if any semantic escape (`\n`,
/// `\t`, `\r`, `\0`) is present.
fn has_only_raw_safe_escapes(s: &str) -> bool {
    let mut found_escape = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"' | '\\') => found_escape = true,
                _ => return false,
            }
        }
    }
    found_escape
}

/// Decode the DSL's supported string escapes (`\"`, `\\`, `\n`, `\t`, `\r`,
/// `\0`) into their literal characters. Any other escape (post-lexer they
/// shouldn't exist) is passed through verbatim.
fn decode_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                // `None` here should be unreachable, purely defensive
                Some('\\') | None => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Minimum number of `#` delimiters needed so `r<#s>"content"<#s>` is
/// unambiguously terminated. Returns 0 if `content` has no `"`.
fn hashes_needed(content: &str) -> u8 {
    let bytes = content.as_bytes();
    let mut max_run_after_quote: u8 = 0;
    let mut any_quote = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            any_quote = true;
            let mut count: u8 = 0;
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'#' {
                count = count.saturating_add(1);
                j += 1;
            }
            if count > max_run_after_quote {
                max_run_after_quote = count;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    if any_quote {
        max_run_after_quote.saturating_add(1)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_safe_escapes() {
        // Only \\ and \" are safe for raw conversion.
        assert!(has_only_raw_safe_escapes(r"\\"));
        assert!(has_only_raw_safe_escapes(r#"\""#));
        assert!(has_only_raw_safe_escapes(r#"foo\\bar\""#));
        // Semantic escapes are not safe.
        assert!(!has_only_raw_safe_escapes(r"\n"));
        assert!(!has_only_raw_safe_escapes(r"\t"));
        assert!(!has_only_raw_safe_escapes(r"\r"));
        assert!(!has_only_raw_safe_escapes(r"\0"));
        // Mixed: one safe + one semantic -> not safe.
        assert!(!has_only_raw_safe_escapes(r"\\foo\n"));
        // No escapes at all -> not offered (nothing to simplify).
        assert!(!has_only_raw_safe_escapes("hello"));
        assert!(!has_only_raw_safe_escapes(""));
    }

    #[test]
    fn decode_escapes_basic() {
        assert_eq!(decode_escapes("hello"), "hello");
        assert_eq!(decode_escapes(r#"he\"llo"#), r#"he"llo"#);
        assert_eq!(decode_escapes(r"a\nb"), "a\nb");
        assert_eq!(decode_escapes(r"a\tb"), "a\tb");
        assert_eq!(decode_escapes(r"a\rb"), "a\rb");
        assert_eq!(decode_escapes(r"a\0b"), "a\0b");
        assert_eq!(decode_escapes(r"\\"), "\\");
    }

    #[test]
    fn decode_escapes_mixed() {
        assert_eq!(
            decode_escapes(r#"he said \"hi\nthere\""#),
            "he said \"hi\nthere\""
        );
    }

    #[test]
    fn hashes_needed_no_quotes() {
        assert_eq!(hashes_needed("hello"), 0);
        assert_eq!(hashes_needed(""), 0);
        assert_eq!(hashes_needed("# not after a quote"), 0);
    }

    #[test]
    fn hashes_needed_bare_quote() {
        assert_eq!(hashes_needed(r#"he said "hi""#), 1);
    }

    #[test]
    fn hashes_needed_quote_hash() {
        // content has "#, so we need ##
        assert_eq!(hashes_needed(r##"foo "# bar"##), 2);
    }

    #[test]
    fn hashes_needed_quote_many_hashes() {
        // content has "###, so we need ####
        assert_eq!(hashes_needed(r####"foo "### bar"####), 4);
    }

    #[test]
    fn hashes_needed_picks_max_run() {
        // content has "# somewhere and "## elsewhere: need 3
        assert_eq!(hashes_needed(r###"foo "# mid "## end"###), 3);
    }
}
