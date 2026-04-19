use tower_lsp::lsp_types::{Hover, HoverContents, HoverParams, MarkupContent, MarkupKind};

use crate::analysis;
use crate::document::{CursorContext, DefKind, RefKind};
use crate::hover_docs;
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn hover(backend: &Backend, params: &HoverParams) -> Option<Hover> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    // Snapshot what we need from the document and drop the DashMap guard
    // before calling get_analysis, which also accesses document_map
    // internally (for base grammar lookups). Holding both would deadlock.
    let (source, offset, word) = {
        let doc = backend.document_map.get(uri)?;
        let offset = text::position_to_offset(&doc.rope, pos)?;
        let word = text::word_at_offset(&doc.text, offset)?.to_owned();
        (doc.text.clone(), offset, word)
    };

    let analysis = backend.get_analysis(uri)?;

    match analysis.cursor_context(offset, &source) {
        // On `reserved:`, show the config field docs, not the builtin combinator docs.
        CursorContext::GrammarConfigField => {
            hover_docs::grammar_field_hover(&word).map(|info| make_hover(info.to_string()))
        }
        // On the rule part of `base::rule_name`, show the base grammar's definition.
        CursorContext::BaseRuleAccess => analysis
            .base_module
            .as_ref()
            .and_then(|m| m.definitions.iter().find(|d| d.name == word))
            .map(|def| make_hover(format!("```\n{} {}\n```", def.kind.label(), def.name))),
        // On a member accessed through an imported module (`mod::fn_name`).
        CursorContext::ImportModuleAccess { .. } => {
            imported_member_hover(&analysis, &source, &word, offset)
        }
        CursorContext::Identifier { .. } => {
            identifier_hover(&analysis, &source, uri, &word, offset)
        }
    }
}

fn identifier_hover(
    analysis: &crate::document::Analysis,
    text: &str,
    uri: &tower_lsp::lsp_types::Url,
    word: &str,
    offset: u32,
) -> Option<Hover> {
    // grammar_config(x).field - show the field type from the known config schema.
    if let Some(tokens) = analysis.tokens.as_deref()
        && let Some(content) = grammar_config_field_hover(tokens, word, offset)
    {
        return Some(make_hover(content));
    }

    // Object field access (e.g. `CALL` in `PREC.CALL`) - show the field's value.
    if let Some(RefKind::ObjectField { field, object }) = analysis
        .references
        .iter()
        .flatten()
        .find(|r| offset >= r.span.start && offset < r.span.end)
        .map(|r| r.kind.clone())
        && let Some(content) = field_value_hover(text, uri, &object, &field)
    {
        return Some(make_hover(content));
    }

    // User-defined rules, functions, let bindings.
    if let Some(def) = analysis
        .definitions
        .iter()
        .flatten()
        .find(|d| d.name == word)
    {
        let content = match &def.kind {
            DefKind::Function { signature } => format!("```\n{signature}\n```"),
            DefKind::Let { .. } => {
                // Run the pipeline to get the type from the type environment.
                let ty =
                    analysis::with_type_env(text, uri, |_ast, env| env.vars.get(word)).flatten();
                ty.map_or_else(
                    || format!("```\nlet {}\n```", def.name),
                    |ty| format!("```\nlet {}: {ty}\n```", def.name),
                )
            }
            _ => format!("```\n{} {}\n```", def.kind.label(), def.name),
        };
        return Some(make_hover(content));
    }

    // Builtins and keywords.
    hover_docs::builtin_hover(word).map(|info| make_hover(info.to_string()))
}

/// Find the value of `object.field` and render it as a hover string.
fn field_value_hover(
    text: &str,
    uri: &tower_lsp::lsp_types::Url,
    object_name: &str,
    field_name: &str,
) -> Option<String> {
    analysis::with_ast(text, uri, |parsed_ast| {
        let (_, value_id) = analysis::find_object_field(parsed_ast, object_name, field_name)?;
        let value_text = parsed_ast.text(parsed_ast.span(value_id));
        Some(format!(
            "```\n{object_name}.{field_name} = {value_text}\n```"
        ))
    })?
}

/// Show hover info for a member accessed through an imported module.
fn imported_member_hover(
    analysis: &crate::document::Analysis,
    source: &str,
    word: &str,
    offset: u32,
) -> Option<Hover> {
    // Find which import module this access belongs to.
    let tokens = analysis.tokens.as_deref()?;
    let module_name = crate::text::qualified_access_module(tokens, source, offset)?;
    let module_info = analysis.get_module(module_name)?;
    let def = module_info.definitions.iter().find(|d| d.name == word)?;
    let content = match &def.kind {
        DefKind::Function { signature } => format!("```\n{signature}\n```"),
        _ => format!("```\n{} {}\n```", def.kind.label(), def.name),
    };
    Some(make_hover(content))
}

/// Check if the identifier at `offset` is a field access on `grammar_config(...)`.
/// If so, return a hover string with the field's type.
fn grammar_config_field_hover(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    word: &str,
    offset: u32,
) -> Option<String> {
    use tree_sitter_generate::nativedsl::lexer::TokenKind;

    // Find the ident token at the cursor.
    let idx = tokens
        .iter()
        .position(|t| t.kind == TokenKind::Ident && offset >= t.span.start && offset < t.span.end)?;

    // Must be preceded by `.`
    if idx < 2 || tokens[idx - 1].kind != TokenKind::Dot {
        return None;
    }

    // The token before `.` must be `)` closing a `grammar_config(...)` call.
    let before_dot = &tokens[idx - 2];
    if before_dot.kind != TokenKind::RParen {
        return None;
    }

    // Walk back through parens to find `grammar_config`.
    let dot_start = tokens[idx - 1].span.start;
    if !crate::handlers::completion::is_grammar_config_call(tokens, dot_start) {
        return None;
    }

    // Show the same docs as hovering over the field inside the grammar block.
    Some(hover_docs::grammar_field_hover(word)?.to_owned())
}

const fn make_hover(value: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: None,
    }
}
