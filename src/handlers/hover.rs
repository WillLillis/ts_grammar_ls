use tower_lsp::lsp_types::{Hover, HoverContents, HoverParams, MarkupContent, MarkupKind};

use crate::document::{BindingLocation, CursorContext, DefKind, RefKind};
use crate::hover_docs;
use crate::server::Backend;
use crate::text;

#[must_use]
pub fn hover(backend: &Backend, params: &HoverParams) -> Option<Hover> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let (analysis, offset) = backend.resolve_position(uri, pos)?;
    let word = text::word_at_offset(&analysis.source, offset)?.to_owned();

    match analysis.cursor_context(offset, &analysis.source)? {
        // On `reserved:`, show the config field docs, not the builtin combinator docs.
        CursorContext::GrammarConfigField => {
            hover_docs::grammar_field_hover(&word).map(|info| make_hover(info.to_string()))
        }
        // On the rule part of `base::rule_name`, show the base grammar's definition.
        CursorContext::BaseRuleAccess => analysis
            .base_module
            .as_deref()
            .and_then(|m| m.definitions.iter().flatten().find(|d| d.name == word))
            .map(|def| make_hover(format!("```\n{} {}\n```", def.kind.label(), def.name))),
        // On a member accessed through an imported module (`mod::fn_name`).
        CursorContext::ImportModuleAccess { .. } => imported_member_hover(&analysis, &word, offset),
        CursorContext::Identifier { .. } => {
            identifier_hover(&analysis, &analysis.source, &word, offset)
        }
    }
}

fn identifier_hover(
    analysis: &crate::document::Module,
    text: &str,
    word: &str,
    offset: u32,
) -> Option<Hover> {
    // grammar_config(x).field - show the field type from the known config schema.
    if let Some(tokens) = analysis.tokens.as_deref()
        && let Some(content) = grammar_config_field_hover(tokens, word, offset)
    {
        return Some(make_hover(content));
    }

    // `#[cfg(NAME)]` - show the flag's enabled/disabled state.
    if let Some(tokens) = analysis.tokens.as_deref()
        && let Some((_, name)) = text::cfg_flag_at_offset(tokens, &analysis.source, offset)
    {
        let state = analysis
            .declared_cfg_flags
            .iter()
            .find(|f| f.name == name)
            .map_or(
                "undeclared",
                |f| if f.enabled { "enabled" } else { "disabled" },
            );
        return Some(make_hover(format!("```\ncfg flag `{name}` ({state})\n```")));
    }

    // inherit("path") / import("path") - show the resolved canonical path.
    if let Some(content) = path_string_hover(analysis, offset) {
        return Some(make_hover(content));
    }

    // Object field access (e.g. `CALL` in `PREC.CALL`) - show the field's value.
    if let Some(RefKind::ObjectField { field, object }) =
        analysis.reference_at(offset).map(|r| r.kind.clone())
        && let Some(def) = find_object_field(analysis, &object, &field)
        && let DefKind::ObjectKey { value_span } = def.kind
    {
        let value_text = &text[value_span.start as usize..value_span.end as usize];
        return Some(make_hover(format!(
            "```\n{object}.{field} = {value_text}\n```"
        )));
    }

    // User-defined names: local bindings first, then helper-rule and external
    // bindings reachable by bare name from imports / inherits.
    let scope = analysis.scope_at(offset);
    if let Some(binding) = analysis.resolve_bare_name(word, scope) {
        let content = match binding {
            BindingLocation::Local(def) => match &def.kind {
                DefKind::Function { signature } => format!("```\n{signature}\n```"),
                DefKind::Let { ty, .. } => ty.map_or_else(
                    || format!("```\nlet {}\n```", def.name),
                    |ty| format!("```\nlet {}: {ty}\n```", def.name),
                ),
                DefKind::Parameter { ty, .. } => {
                    format!("```\nparameter {}: {ty}\n```", def.name)
                }
                _ => format!("```\n{} {}\n```", def.kind.label(), def.name),
            },
            BindingLocation::External { def, .. } => match &def.kind {
                DefKind::Function { signature } => format!("```\n{signature}\n```"),
                _ => format!("```\n{} {}\n```", def.kind.label(), def.name),
            },
        };
        return Some(make_hover(content));
    }

    // Builtins and keywords.
    hover_docs::builtin_hover(word).map(|info| make_hover(info.to_string()))
}

/// Hover for the path string inside `inherit("...")` / `import("...")`:
/// show the resolved canonical path.
fn path_string_hover(analysis: &crate::document::Module, offset: u32) -> Option<String> {
    let reference = analysis.reference_at(offset)?;
    match &reference.kind {
        RefKind::InheritPath(_) => {
            let base = analysis.base_module.as_deref()?;
            Some(format!("```\ninherit: {}\n```", base.path.display()))
        }
        RefKind::ImportPath(binding) => {
            let module = analysis.get_module(binding)?;
            Some(format!("```\nimport: {}\n```", module.path.display()))
        }
        _ => None,
    }
}

/// Find the `DefKind::ObjectKey` definition matching `obj_name.field_name`,
/// resolved via the existing top-level `Let` binding for `obj_name`.
fn find_object_field<'a>(
    analysis: &'a crate::document::Module,
    obj_name: &str,
    field_name: &str,
) -> Option<&'a crate::document::Definition> {
    let definitions = analysis.definitions.as_ref()?;
    let let_def = definitions
        .iter()
        .find(|d| d.name == obj_name && matches!(d.kind, DefKind::Let { .. }))?;
    let let_span = let_def.full_span;
    definitions.iter().find(|d| {
        d.name == field_name
            && matches!(d.kind, DefKind::ObjectKey { .. })
            && d.name_span.start >= let_span.start
            && d.name_span.end <= let_span.end
    })
}

/// Show hover info for a member accessed through an imported module.
fn imported_member_hover(
    analysis: &crate::document::Module,
    word: &str,
    offset: u32,
) -> Option<Hover> {
    let module_info = analysis.qualified_member_module(offset)?;
    let def = module_info
        .definitions
        .iter()
        .flatten()
        .find(|d| d.name == word)?;
    let content = match &def.kind {
        DefKind::Function { signature } => format!("```\n{signature}\n```"),
        _ => format!("```\n{} {}\n```", def.kind.label(), def.name),
    };
    Some(make_hover(content))
}

/// Check if the identifier at `offset` is the field-name argument of a
/// `grammar_config(module, field)` call. If so, return a hover string with
/// the field's type.
fn grammar_config_field_hover(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    word: &str,
    offset: u32,
) -> Option<String> {
    use tree_sitter_generate::nativedsl::lexer::TokenKind;

    let idx = tokens.iter().position(|t| {
        t.kind == TokenKind::Ident && offset >= t.span.start && offset < t.span.end
    })?;
    text::at_grammar_config_field_arg(tokens, idx)
        .then(|| hover_docs::grammar_field_hover(word).map(str::to_owned))
        .flatten()
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
