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

    let doc = backend.document_map.get(uri)?;
    let offset = text::position_to_offset(&doc.rope, pos)?;
    let word = text::word_at_offset(&doc.text, offset)?;

    let ctx = backend.analysis_context();
    let analysis = analysis::analyze(&doc.text, uri, Some(&ctx));

    match analysis.cursor_context(offset, &doc.text) {
        // On `reserved:`, show the config field docs, not the builtin combinator docs.
        CursorContext::GrammarConfigField => {
            hover_docs::grammar_field_hover(word).map(|info| make_hover(info.to_string()))
        }
        // On the rule part of `base::rule_name`, show the base grammar's definition.
        CursorContext::BaseRuleAccess => analysis
            .base_definitions
            .iter()
            .flatten()
            .find(|d| d.name == word)
            .map(|def| make_hover(format!("```\n{} {}\n```", def.kind.label(), def.name))),
        // On a member accessed through an imported module (`mod::fn_name`).
        CursorContext::ImportModuleAccess { .. } => {
            imported_member_hover(&analysis, &doc.text, word, offset)
        }
        CursorContext::Identifier { .. } => {
            identifier_hover(&analysis, &doc.text, uri, word, offset)
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
    let module_info = analysis.import_modules.get(module_name)?;
    let def = module_info.definitions.iter().find(|d| d.name == word)?;
    let content = match &def.kind {
        DefKind::Function { signature } => format!("```\n{signature}\n```"),
        _ => format!("```\n{} {}\n```", def.kind.label(), def.name),
    };
    Some(make_hover(content))
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
