//! `textDocument/codeAction` handler.
//!
//! Provides three refactorings:
//!
//! - **Convert string to raw string** (`"..."` to `r#"..."#`) with the minimum
//!   number of `#` delimiters needed to disambiguate embedded `"` sequences.
//! - **Inline rule-set macro call**: cursor on a top-level `@NAME(args)`
//!   invocation that the loader expanded into `Node::ExpandedRule`s. The
//!   handler finds those `ExpandedRule`s (they share the original call's
//!   span) and replaces the call span with the rendered rule decls.
//! - **Inline expression macro call**: cursor on a nested `Node::Call` or
//!   `Node::QualifiedCall` (cross-module) whose name resolves to an
//!   expression-flavor macro. The handler renders the macro's body with
//!   the caller's args substituted (via `format_macro_expansion`) and
//!   replaces the call's span with the result.

use tower_lsp::lsp_types;
use tree_sitter_generate::nativedsl::ast::{
    IdentKind, MacroId, MacroKind, Node, NodeId, SharedAst, Span,
};
use tree_sitter_generate::nativedsl::lexer::TokenKind;

use crate::repl::{ReplInputUri, ReplMeta, TreeFormat};
use crate::{
    config::FormattingConfig,
    document::{DefKind, Module},
    formatter,
    server::Backend,
    text,
};

#[must_use]
pub fn code_action(
    backend: &Backend,
    params: &lsp_types::CodeActionParams,
) -> Option<lsp_types::CodeActionResponse> {
    let uri = &params.text_document.uri;

    // If the client filtered by `only`, skip if it didn't ask for refactors.
    if let Some(kinds) = &params.context.only
        && !kinds.iter().any(is_refactor_kind)
    {
        return Some(Vec::new());
    }

    // REPL input buffers aren't grammar source, so they bypass the
    // analysis-driven actions below. The only action we offer there is
    // the format toggle (the alternative tree-buffer CodeLens has
    // unfixable display issues on unfocused buffers in neovim).
    if let Some(input_uri) = crate::repl::ReplInputUri::try_from_uri(uri) {
        return build_toggle_repl_format_action(backend, &input_uri)
            .map(|a| vec![lsp_types::CodeActionOrCommand::CodeAction(a)]);
    }

    let (analysis, start_offset) = backend.resolve_position(uri, params.range.start)?;

    let mut actions: Vec<lsp_types::CodeActionOrCommand> = Vec::new();
    if let Some(a) = build_raw_string_action(&analysis, start_offset, uri) {
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(a));
    }
    if let Some(a) = build_inline_macro_action(&analysis, start_offset, uri) {
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(a));
    }
    if let Some(a) = build_inline_expression_macro_action(&analysis, start_offset, uri) {
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(a));
    }
    if let Some(a) = build_open_repl_action(&analysis, start_offset, uri, params.range.start) {
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(a));
    }
    if let Some(a) = build_open_grammar_repl_action(&analysis, uri) {
        actions.push(lsp_types::CodeActionOrCommand::CodeAction(a));
    }
    if actions.is_empty() {
        None
    } else {
        Some(actions)
    }
}

/// "Switch to S-expression view" / "Switch to CST view"
fn build_toggle_repl_format_action(
    backend: &Backend,
    input_uri: &ReplInputUri,
) -> Option<lsp_types::CodeAction> {
    let format = backend
        .repl_sessions
        .get(input_uri)
        .map(|s| s.lock().unwrap().format)
        .or_else(|| ReplMeta::read_for(input_uri).map(|m| m.format))?;
    let title = match format {
        TreeFormat::Cst => "Switch to S-expression view",
        TreeFormat::Sexp => "Switch to CST view",
    };
    Some(lsp_types::CodeAction {
        title: title.into(),
        kind: Some(lsp_types::CodeActionKind::EMPTY),
        command: Some(lsp_types::Command {
            title: title.into(),
            command: crate::handlers::repl::TOGGLE_REPL_FORMAT_COMMAND.into(),
            arguments: Some(vec![
                serde_json::json!({ "uri": input_uri.as_url().to_string() }),
            ]),
        }),
        edit: None,
        diagnostics: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

/// "Open REPL for rule `X`" when the cursor sits on the rule's *name*
/// identifier in its declaration. Narrower than checking `full_span`
/// (which covers the entire `rule X { ... }` body) so the action is
/// scoped to "I clicked on the rule name", matching how users actually
/// invoke it. Surfaces the `tsg.openRepl` workspace command as a
/// discoverable refactor, rather than requiring the user to type out
/// `vim.lsp.buf.execute_command(...)`.
fn build_open_repl_action(
    analysis: &Module,
    start_offset: u32,
    uri: &lsp_types::Url,
    position: lsp_types::Position,
) -> Option<lsp_types::CodeAction> {
    let defs = analysis.definitions.as_ref()?;
    let def = defs.iter().find(|d| {
        matches!(d.kind, DefKind::Rule | DefKind::OverrideRule)
            && d.name_span.start <= start_offset
            && start_offset < d.name_span.end
    })?;
    let arguments = vec![serde_json::json!({
        "uri": uri.to_string(),
        "position": { "line": position.line, "character": position.character }
    })];
    Some(lsp_types::CodeAction {
        title: format!("Open REPL for rule `{}`", def.name),
        kind: Some(lsp_types::CodeActionKind::EMPTY),
        command: Some(lsp_types::Command {
            title: "Open REPL".into(),
            command: crate::handlers::repl::OPEN_REPL_COMMAND.into(),
            arguments: Some(arguments),
        }),
        edit: None,
        diagnostics: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

/// "Open grammar REPL" - always available in any `.tsg` buffer that
/// has at least one rule. Opens a REPL bound to the grammar's first
/// rule (which `InputGrammar::normalize` treats as the implicit
/// start).
fn build_open_grammar_repl_action(
    analysis: &Module,
    uri: &lsp_types::Url,
) -> Option<lsp_types::CodeAction> {
    // Only offer the action when there's actually a rule to default to.
    let defs = analysis.definitions.as_ref()?;
    defs.iter()
        .find(|d| matches!(d.kind, DefKind::Rule | DefKind::OverrideRule))?;
    let arguments = vec![serde_json::json!({ "uri": uri.to_string() })];
    Some(lsp_types::CodeAction {
        title: "Open grammar REPL".into(),
        kind: Some(lsp_types::CodeActionKind::EMPTY),
        command: Some(lsp_types::Command {
            title: "Open grammar REPL".into(),
            command: crate::handlers::repl::OPEN_REPL_COMMAND.into(),
            arguments: Some(arguments),
        }),
        edit: None,
        diagnostics: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

fn build_raw_string_action(
    analysis: &Module,
    start_offset: u32,
    uri: &lsp_types::Url,
) -> Option<lsp_types::CodeAction> {
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
        vec![lsp_types::TextEdit {
            range: edit_range,
            new_text,
        }],
    )]);

    Some(lsp_types::CodeAction {
        title: "Convert to raw string".into(),
        kind: Some(lsp_types::CodeActionKind::REFACTOR_REWRITE),
        edit: Some(lsp_types::WorkspaceEdit {
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
    uri: &lsp_types::Url,
) -> Option<lsp_types::CodeAction> {
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
        let rendered = formatter::format_macro_expansion(rid, &[], analysis, analysis, &config);
        parts.push(rendered);
    }
    let new_text = parts.join("\n\n");
    let edit_range = text::span_to_range(&analysis.rope, call_span);
    let edits = std::collections::HashMap::from([(
        uri.clone(),
        vec![lsp_types::TextEdit {
            range: edit_range,
            new_text,
        }],
    )]);

    Some(lsp_types::CodeAction {
        title: "Inline macro call".into(),
        kind: Some(lsp_types::CodeActionKind::REFACTOR_REWRITE),
        edit: Some(lsp_types::WorkspaceEdit {
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

/// "Inline macro call" for an expression-flavor macro. Walks the AST
/// top-down from `root_items`, finds the innermost `Node::Call` /
/// `Node::QualifiedCall` whose name resolves to an expression-flavor
/// macro and whose span contains the cursor, then replaces the call
/// span with the rendered expansion.
fn build_inline_expression_macro_action(
    analysis: &Module,
    start_offset: u32,
    uri: &lsp_types::Url,
) -> Option<lsp_types::CodeAction> {
    if !analysis.loader_succeeded {
        return None;
    }
    let shared = &analysis.shared;
    let call_id = analysis
        .root_items
        .iter()
        .find_map(|&id| find_macro_call_at(shared, start_offset, id))?;
    let (macro_id, body_module, args) = resolve_call_target(analysis, call_id)?;
    let macro_cfg = shared.pools.get_macro(macro_id);
    // Only expression-flavor macros are inlinable in expression position.
    // Rule-set macros are handled by `build_inline_macro_action`.
    if !matches!(macro_cfg.kind, MacroKind::Expression(_)) {
        return None;
    }

    let config = FormattingConfig::default();
    let new_text =
        formatter::format_macro_expansion(macro_cfg.body, &args, body_module, analysis, &config);
    let call_span = shared.arena.span(call_id);
    let edit_range = text::span_to_range(&analysis.rope, call_span);
    let edits = std::collections::HashMap::from([(
        uri.clone(),
        vec![lsp_types::TextEdit {
            range: edit_range,
            new_text,
        }],
    )]);
    Some(lsp_types::CodeAction {
        title: "Inline macro call".into(),
        kind: Some(lsp_types::CodeActionKind::REFACTOR_REWRITE),
        edit: Some(lsp_types::WorkspaceEdit {
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

/// Top-down search: smallest-span `Call`/`QualifiedCall` to a macro that
/// contains `cursor`. Descend through children first so an inner call
/// wins over an enclosing one.
fn find_macro_call_at(shared: &SharedAst, cursor: u32, id: NodeId) -> Option<NodeId> {
    let span = shared.arena.span(id);
    if !(span.start <= cursor && cursor < span.end) {
        return None;
    }
    let children = collect_children(shared, id);
    for c in children {
        if let Some(hit) = find_macro_call_at(shared, cursor, c) {
            return Some(hit);
        }
    }
    match *shared.arena.get(id) {
        Node::Call { name, .. } => {
            if matches!(shared.arena.get(name), Node::Ident(IdentKind::Macro(_))) {
                return Some(id);
            }
        }
        Node::QualifiedCall(range) => {
            let (_, name, _) = shared.pools.get_qualified_call(range);
            if matches!(shared.arena.get(name), Node::Ident(IdentKind::Macro(_))) {
                return Some(id);
            }
        }
        _ => {}
    }
    None
}

/// Enumerate `id`'s direct children. Used by `find_macro_call_at`'s
/// recursive descent; leaves return an empty list.
fn collect_children(shared: &SharedAst, id: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    match *shared.arena.get(id) {
        #[rustfmt::skip]
        Node::Rule { body: id, .. } | Node::Let { value: id, .. } | Node::Cfg { child: id, .. }
        | Node::Field { content: id, .. } | Node::Reserved { content: id, .. } | Node::Repeat { inner: id, .. }
        | Node::Token { inner: id, .. } | Node::Neg(id) | Node::GrammarConfig { module: id, .. }
        | Node::FieldAccess { obj: id, .. } | Node::QualifiedAccess { obj: id, .. }
        | Node::SymRef { expr: id } | Node::ExpandedRule { body: id, .. } => out.push(id),
        #[rustfmt::skip]
        Node::RuleSet(r) | Node::SeqOrChoice { range: r, .. } | Node::Concat(r)
        | Node::List(r) | Node::Tuple(r) | Node::QualifiedCall(r) => {
            out.extend(shared.pools.child_slice(r).iter().copied());
        }
        #[rustfmt::skip]
        Node::ComputedRule { name_expr: a, body: b, .. } | Node::Alias { content: a, target: b, }
        | Node::Prec { value: a, content: b, .. } | Node::Append { left: a, right: b }
        | Node::BinOp { lhs: a, rhs: b, .. } => {
            out.push(a);
            out.push(b);
        }
        Node::Macro(macro_id) => out.push(shared.pools.get_macro(macro_id).body),
        Node::DynRegex { pattern, flags } => {
            out.push(pattern);
            if let Some(f) = flags {
                out.push(f);
            }
        }
        Node::For { for_id, body } => {
            let cfg = shared.pools.get_for(for_id);
            out.push(cfg.iterable);
            out.push(body);
        }
        Node::Call { name, args } => {
            out.push(name);
            out.extend(shared.pools.child_slice(args).iter().copied());
        }
        Node::Object(range) => {
            for &(_, v) in shared.pools.get_object(range) {
                out.push(v);
            }
        }
        // Leaves.
        Node::Grammar
        | Node::External { .. }
        | Node::StringLit
        | Node::RawStringLit { .. }
        | Node::IntLit(_)
        | Node::Ident(_)
        | Node::Blank
        | Node::SynthRef { .. }
        | Node::MacroParam { .. }
        | Node::ForBinding { .. }
        | Node::ModuleRef { .. }
        | Node::Unreachable => {}
    }
    out
}

/// Resolve a Call or `QualifiedCall` `NodeId` to:
///   - the `MacroId` it invokes,
///   - the `Module` whose `StringTable` / source the macro body lives in,
///   - the argument `NodeId`s.
///
/// For a local `Node::Call`, `body_module` is `caller`. For a
/// `Node::QualifiedCall`, the obj resolves to a `let h = import("...")`
/// binding and `body_module` is the imported module.
fn resolve_call_target(
    caller: &Module,
    call_id: NodeId,
) -> Option<(MacroId, &Module, Vec<NodeId>)> {
    let shared = &caller.shared;
    match *shared.arena.get(call_id) {
        Node::Call { name, args } => {
            let macro_id = match shared.arena.get(name) {
                Node::Ident(IdentKind::Macro(m)) => *m,
                _ => return None,
            };
            let args = shared.pools.child_slice(args).to_vec();
            Some((macro_id, caller, args))
        }
        Node::QualifiedCall(range) => {
            let (obj, name, args) = shared.pools.get_qualified_call(range);
            let macro_id = match shared.arena.get(name) {
                Node::Ident(IdentKind::Macro(m)) => *m,
                _ => return None,
            };
            let body_module = resolve_import_obj(caller, obj)?;
            Some((macro_id, body_module, args.to_vec()))
        }
        _ => None,
    }
}

/// Follow `obj`'s `IdentKind::Var(let_id)` to its `Node::Let`, take the
/// let's binding name, and look it up in `caller.import_modules`.
fn resolve_import_obj(caller: &Module, obj: NodeId) -> Option<&Module> {
    let shared = &caller.shared;
    let Node::Ident(IdentKind::Var(let_id)) = shared.arena.get(obj) else {
        return None;
    };
    let Node::Let { name, .. } = shared.arena.get(*let_id) else {
        return None;
    };
    let binding_name = &caller.source[name.start as usize..name.end as usize];
    caller
        .import_modules
        .iter()
        .find(|(n, _)| n == binding_name)
        .map(|(_, m)| m)
}

fn is_refactor_kind(k: &lsp_types::CodeActionKind) -> bool {
    *k == lsp_types::CodeActionKind::REFACTOR_REWRITE
        || *k == lsp_types::CodeActionKind::REFACTOR
        || *k == lsp_types::CodeActionKind::EMPTY
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
