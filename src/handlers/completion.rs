use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse,
};
use tree_sitter_generate::nativedsl::lexer::{Token, TokenKind};

use crate::analysis;
use crate::document::DefKind;
use crate::server::Backend;
use crate::text;

const BUILTIN_COMBINATORS: &[(&str, &str)] = &[
    ("seq", "Match items in sequence"),
    ("choice", "Match one of the items"),
    ("repeat", "Match zero or more"),
    ("repeat1", "Match one or more"),
    ("optional", "Match zero or one"),
    ("blank", "Match nothing (epsilon)"),
    ("field", "Assign a field name"),
    ("alias", "Rename in syntax tree"),
    ("token", "Mark as single token"),
    ("token_immediate", "Token with no preceding whitespace"),
    ("prec", "Set precedence"),
    ("prec_left", "Left-associative precedence"),
    ("prec_right", "Right-associative precedence"),
    ("prec_dynamic", "Dynamic precedence"),
    ("regexp", "Match a regular expression"),
    ("concat", "Concatenate strings"),
    ("append", "Concatenate lists"),
    ("reserved", "Reserved word context"),
    ("inherit", "Inherit from base grammar"),
];

const KEYWORDS: &[(&str, &str)] = &[
    ("grammar", "Grammar configuration block"),
    ("rule", "Define a grammar rule"),
    ("override", "Override an inherited rule"),
    ("let", "Bind a value"),
    ("fn", "Define a function"),
    ("for", "Iterate over a list"),
    ("in", "For-loop iterable"),
    (
        "print",
        "Debug-print a value to stderr at grammar evaluation time",
    ),
];

const TYPE_KEYWORDS: &[(&str, &str)] = &[
    ("rule_t", "Rule expression type"),
    ("str_t", "String type"),
    ("int_t", "Integer type"),
    ("list_rule_t", "List of rules type"),
    ("list_str_t", "List of strings type"),
    ("list_int_t", "List of integers type"),
    ("list_list_rule_t", "List of lists of rules type"),
    ("list_list_str_t", "List of lists of strings type"),
    ("list_list_int_t", "List of lists of integers type"),
    (
        "void_t",
        "Internal type returned by print; not a user-writable annotation",
    ),
    (
        "spread_t",
        "Internal type produced by for-loop expansions; not a user-writable annotation",
    ),
];

#[must_use]
#[expect(clippy::too_many_lines)]
pub fn completion(backend: &Backend, params: &CompletionParams) -> Option<CompletionResponse> {
    let uri = &params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;

    let doc = backend.document_map.get(uri)?;
    let offset = text::position_to_offset(&doc.rope, pos)?;

    let ctx = backend.analysis_context();
    let analysis = analysis::analyze(&doc.text, uri, Some(&ctx));
    let tokens = analysis.tokens.as_deref().unwrap_or_default();

    // Find the token just before the cursor position.
    let prev_token = tokens.iter().take_while(|t| t.span.end <= offset).last();

    // `IDENT.` -> complete object fields
    if let Some(Token {
        kind: TokenKind::Dot,
        span: dot_span,
    }) = prev_token
    {
        let ident = tokens
            .iter()
            .take_while(|t| t.span.end <= dot_span.start)
            .last();
        if let Some(ident) = ident.filter(|t| t.kind == TokenKind::Ident) {
            let obj_name = &doc.text[ident.span.start as usize..ident.span.end as usize];
            if let Some(defs) = &analysis.definitions {
                return Some(CompletionResponse::Array(object_field_completions(
                    defs, obj_name,
                )));
            }
            // Definitions unavailable (e.g. parse error). Fall back to
            // scanning tokens for `let OBJ = { KEY: ... }` patterns.
            return Some(CompletionResponse::Array(
                object_field_completions_from_tokens(tokens, &doc.text, obj_name),
            ));
        }
    }

    // `IDENT::` -> complete module members (import or base grammar rules).
    if let Some(cc_token) = prev_token.filter(|t| t.kind == TokenKind::ColonColon) {
        // Find the identifier immediately before the `::` token.
        let qualifier = tokens
            .iter()
            .take_while(|t| t.span.end <= cc_token.span.start)
            .last()
            .filter(|t| t.kind == TokenKind::Ident);
        let qualifier_name =
            qualifier.map(|t| &doc.text[t.span.start as usize..t.span.end as usize]);

        // Check if the qualifier is an import variable.
        if let Some(name) = qualifier_name
            && let Some(module_info) = analysis.import_modules.get(name)
        {
            return Some(CompletionResponse::Array(
                module_info
                    .definitions
                    .iter()
                    .filter_map(|d| {
                        let (kind, detail) = match &d.kind {
                            DefKind::Rule | DefKind::OverrideRule => {
                                (CompletionItemKind::CLASS, format!("rule {}", d.name))
                            }
                            DefKind::Function { signature } => {
                                (CompletionItemKind::FUNCTION, signature.clone())
                            }
                            DefKind::Let { .. } => {
                                (CompletionItemKind::VARIABLE, format!("let {}", d.name))
                            }
                            DefKind::Import => {
                                (CompletionItemKind::MODULE, format!("import {}", d.name))
                            }
                            DefKind::ObjectKey | DefKind::Parameter { .. } => return None,
                        };
                        Some(CompletionItem {
                            label: d.name.clone(),
                            kind: Some(kind),
                            detail: Some(detail),
                            ..Default::default()
                        })
                    })
                    .collect(),
            ));
        }

        // Fall back to base grammar rules.
        return Some(CompletionResponse::Array(
            analysis
                .base_definitions
                .iter()
                .flatten()
                .filter(|d| matches!(d.kind, DefKind::Rule))
                .map(|d| CompletionItem {
                    label: d.name.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some(format!("rule {} (base)", d.name)),
                    ..Default::default()
                })
                .collect(),
        ));
    }

    let mut items = Vec::new();

    // User-defined rules.
    for def in analysis.definitions.iter().flatten() {
        let (kind, detail) = match &def.kind {
            DefKind::Rule | DefKind::OverrideRule => {
                (CompletionItemKind::CLASS, format!("rule {}", def.name))
            }
            DefKind::Function { signature } => (CompletionItemKind::FUNCTION, signature.clone()),
            DefKind::Let { .. } => (CompletionItemKind::VARIABLE, format!("let {}", def.name)),
            DefKind::Import | DefKind::ObjectKey | DefKind::Parameter { .. } => continue,
        };
        items.push(CompletionItem {
            label: def.name.clone(),
            kind: Some(kind),
            detail: Some(detail),
            ..Default::default()
        });
    }
    drop(doc);

    // Builtin combinators.
    for &(name, detail) in BUILTIN_COMBINATORS {
        items.push(CompletionItem {
            label: name.into(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(detail.into()),
            ..Default::default()
        });
    }

    // Keywords.
    for &(name, detail) in KEYWORDS {
        items.push(CompletionItem {
            label: name.into(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(detail.into()),
            ..Default::default()
        });
    }

    // Type keywords.
    for &(name, detail) in TYPE_KEYWORDS {
        items.push(CompletionItem {
            label: name.into(),
            kind: Some(CompletionItemKind::TYPE_PARAMETER),
            detail: Some(detail.into()),
            ..Default::default()
        });
    }

    Some(CompletionResponse::Array(items))
}

/// Fallback for dot-completion when cached definitions are unavailable (e.g.
/// parse error). Scans the token stream for `let OBJ = { KEY: ..., KEY: ... }`
/// and extracts the KEY identifiers.
fn object_field_completions_from_tokens(
    tokens: &[Token],
    text: &str,
    obj_name: &str,
) -> Vec<CompletionItem> {
    // Find `KwLet Ident(obj_name) Eq LBrace` sequence.
    let mut i = 0;
    while i + 3 < tokens.len() {
        if tokens[i].kind == TokenKind::KwLet
            && tokens[i + 1].kind == TokenKind::Ident
            && &text[tokens[i + 1].span.start as usize..tokens[i + 1].span.end as usize] == obj_name
            && tokens[i + 2].kind == TokenKind::Eq
            && tokens[i + 3].kind == TokenKind::LBrace
        {
            // Scan inside the braces for `Ident Colon` pairs.
            let mut items = Vec::new();
            let mut j = i + 4;
            let mut depth = 1u32;
            while j < tokens.len() && depth > 0 {
                match tokens[j].kind {
                    TokenKind::LBrace => depth += 1,
                    TokenKind::RBrace => depth -= 1,
                    TokenKind::Ident if depth == 1 => {
                        if tokens
                            .get(j + 1)
                            .is_some_and(|t| t.kind == TokenKind::Colon)
                        {
                            let key =
                                &text[tokens[j].span.start as usize..tokens[j].span.end as usize];
                            items.push(CompletionItem {
                                label: key.into(),
                                kind: Some(CompletionItemKind::FIELD),
                                detail: Some(format!("{obj_name}.{key}")),
                                ..Default::default()
                            });
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            return items;
        }
        i += 1;
    }
    Vec::new()
}

/// Find object field completions for `obj_name.` by finding the Let definition
/// for `obj_name` and returning its `ObjectKey` children.
fn object_field_completions(
    definitions: &[crate::document::Definition],
    obj_name: &str,
) -> Vec<CompletionItem> {
    // Find the Let binding's full_span.
    let Some(let_def) = definitions
        .iter()
        .find(|d| d.name == obj_name && matches!(d.kind, DefKind::Let { .. }))
    else {
        return Vec::new();
    };
    let let_span = let_def.full_span;

    // Collect ObjectKeys whose span falls within this Let binding.
    definitions
        .iter()
        .filter(|d| {
            d.kind == DefKind::ObjectKey
                && d.name_span.start >= let_span.start
                && d.name_span.end <= let_span.end
        })
        .map(|d| CompletionItem {
            label: d.name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(format!("{obj_name}.{}", d.name)),
            ..Default::default()
        })
        .collect()
}
