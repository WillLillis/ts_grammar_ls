use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse,
};
use tree_sitter_generate::nativedsl::lexer::{Token, TokenKind};

use crate::document::{DefKind, Definition, Module};
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
    ("import", "Import a helper module"),
    ("grammar_config", "Access a module's grammar configuration"),
];

const KEYWORDS: &[(&str, &str)] = &[
    ("grammar", "Grammar configuration block"),
    ("rule", "Define a grammar rule"),
    ("override", "Override an inherited rule"),
    ("let", "Bind a value"),
    ("macro", "Define a macro"),
    (
        "external",
        "Forward-declare an externally-provided (typically scanner-emitted) symbol",
    ),
    ("for", "Iterate over a list"),
    ("in", "For-loop iterable"),
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

/// Grammar config fields with their types, matching the typecheck module's field access.
const GRAMMAR_CONFIG_FIELDS: &[(&str, &str)] = &[
    ("language", "str_t"),
    ("inherits", "grammar"),
    ("start", "rule_t"),
    ("extras", "list_rule_t"),
    ("externals", "list_rule_t"),
    ("inline", "list_rule_t"),
    ("supertypes", "list_rule_t"),
    ("conflicts", "list_list_rule_t"),
    ("precedences", "list_list_rule_t"),
    ("word", "rule_t"),
    ("reserved", "{ [context]: list_rule_t }"),
];

#[must_use]
pub fn completion(backend: &Backend, params: &CompletionParams) -> Option<CompletionResponse> {
    let uri = &params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;

    // Use the current buffer text for token scanning (to detect what the
    // user just typed: `::`, `.`, etc.) but the cached analysis for
    // definitions/modules (which may be from a prior successful parse).
    let (source, offset) = {
        let doc = backend.document_map.get(uri)?;
        let offset = text::position_to_offset(&doc.rope, pos)?;
        (doc.text.clone(), offset)
    };
    let analysis = backend.get_analysis(uri)?;
    let current_tokens = tree_sitter_generate::nativedsl::lexer::Lexer::new(&source)
        .tokenize()
        .ok();
    let tokens = current_tokens.as_deref().unwrap_or_default();

    // Find the token just before the cursor position.
    let prev_token = tokens.iter().take_while(|t| t.span.end <= offset).last();

    // Inside `grammar_config(module, |` - complete field names.
    let cursor_token_idx = tokens
        .iter()
        .rposition(|t| t.span.end <= offset)
        .map_or(0, |i| i + 1);
    if text::at_grammar_config_field_arg(tokens, cursor_token_idx) {
        return Some(CompletionResponse::Array(grammar_config_field_items()));
    }

    // `IDENT.` -> complete object fields
    if let Some(Token {
        kind: TokenKind::Dot,
        span: dot_span,
    }) = prev_token
    {
        let before_dot = tokens
            .iter()
            .take_while(|t| t.span.end <= dot_span.start)
            .last();
        if let Some(ident) = before_dot.filter(|t| t.kind == TokenKind::Ident) {
            let obj_name = &source[ident.span.start as usize..ident.span.end as usize];
            // Prefer cached definitions; on parse failure (no previous good
            // snapshot either) scan tokens for `let OBJ = { K: V, ... }` so
            // mid-keystroke `OBJ.|` still surfaces field names.
            return Some(CompletionResponse::Array(
                analysis
                    .definitions
                    .as_deref()
                    .map(|defs| complete_object_field(defs, obj_name))
                    .filter(|items| !items.is_empty())
                    .unwrap_or_else(|| {
                        complete_object_field_from_tokens(tokens, &source, obj_name)
                    }),
            ));
        }
    }

    // `IDENT::` -> complete module members (import or base grammar rules).
    if let Some(cc_token) = prev_token.filter(|t| t.kind == TokenKind::ColonColon) {
        let qualifier = tokens
            .iter()
            .take_while(|t| t.span.end <= cc_token.span.start)
            .last()
            .filter(|t| t.kind == TokenKind::Ident)
            .map(|t| &source[t.span.start as usize..t.span.end as usize]);
        return Some(CompletionResponse::Array(complete_qualified(
            &analysis, qualifier,
        )));
    }

    Some(CompletionResponse::Array(complete_global(&analysis)))
}

/// Completions for `module::` qualified access. When `qualifier` names an
/// imported/inherited module, list that module's definitions; otherwise fall
/// back to base grammar rules.
fn complete_qualified(analysis: &Module, qualifier: Option<&str>) -> Vec<CompletionItem> {
    if let Some(name) = qualifier
        && let Some(module_info) = analysis.get_module(name)
    {
        return module_info
            .definitions
            .iter()
            .flatten()
            .filter_map(|d| qualified_member_item(d, name))
            .collect();
    }
    // Fall back to base grammar rules.
    analysis
        .base_module
        .as_deref()
        .into_iter()
        .flat_map(|m| m.definitions.iter().flatten())
        .filter(|d| matches!(d.kind, DefKind::Rule))
        .map(|d| CompletionItem {
            label: d.name.clone(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(format!("rule {} (base)", d.name)),
            ..Default::default()
        })
        .collect()
}

fn qualified_member_item(d: &Definition, module_name: &str) -> Option<CompletionItem> {
    let (kind, detail) = match &d.kind {
        DefKind::Rule | DefKind::OverrideRule => (
            CompletionItemKind::CLASS,
            format!("rule {} ({module_name})", d.name),
        ),
        DefKind::Function { signature } => (
            CompletionItemKind::FUNCTION,
            format!("{signature} ({module_name})"),
        ),
        DefKind::Let { .. } => (
            CompletionItemKind::VARIABLE,
            format!("let {} ({module_name})", d.name),
        ),
        DefKind::Import | DefKind::Inherit => (
            CompletionItemKind::MODULE,
            format!("{} {} ({module_name})", d.kind.label(), d.name),
        ),
        DefKind::External => (
            CompletionItemKind::CLASS,
            format!("external {} ({module_name})", d.name),
        ),
        DefKind::ObjectKey { .. } | DefKind::Parameter { .. } => return None,
    };
    Some(CompletionItem {
        label: d.name.clone(),
        kind: Some(kind),
        detail: Some(detail),
        ..Default::default()
    })
}

/// Global completions: local defs + bare-name reachable from inherits and
/// transitive imports, plus builtin combinators, keywords, and type names.
fn complete_global(analysis: &Module) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen_names = rustc_hash::FxHashSet::default();

    let mut emit = |def: &Definition| {
        let Some(item) = global_def_item(def) else {
            return;
        };
        if seen_names.insert(def.name.clone()) {
            items.push(item);
        }
    };
    for def in analysis.definitions.iter().flatten() {
        emit(def);
    }
    if let Some(base) = analysis.base_module.as_deref() {
        walk_module(base, &mut emit);
    }
    for (_, info) in &analysis.import_modules {
        walk_module(info, &mut emit);
    }

    for &(name, detail) in BUILTIN_COMBINATORS {
        items.push(CompletionItem {
            label: name.into(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(detail.into()),
            ..Default::default()
        });
    }
    for &(name, detail) in KEYWORDS {
        items.push(CompletionItem {
            label: name.into(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(detail.into()),
            ..Default::default()
        });
    }
    for &(name, detail) in TYPE_KEYWORDS {
        items.push(CompletionItem {
            label: name.into(),
            kind: Some(CompletionItemKind::TYPE_PARAMETER),
            detail: Some(detail.into()),
            ..Default::default()
        });
    }
    items
}

fn global_def_item(def: &Definition) -> Option<CompletionItem> {
    let (kind, detail) = match &def.kind {
        DefKind::Rule | DefKind::OverrideRule => {
            (CompletionItemKind::CLASS, format!("rule {}", def.name))
        }
        DefKind::Function { signature } => (CompletionItemKind::FUNCTION, signature.clone()),
        DefKind::Let { .. } => (CompletionItemKind::VARIABLE, format!("let {}", def.name)),
        DefKind::External => (CompletionItemKind::CLASS, format!("external {}", def.name)),
        DefKind::Import
        | DefKind::Inherit
        | DefKind::ObjectKey { .. }
        | DefKind::Parameter { .. } => return None,
    };
    Some(CompletionItem {
        label: def.name.clone(),
        kind: Some(kind),
        detail: Some(detail),
        ..Default::default()
    })
}

fn walk_module(info: &Module, emit: &mut impl FnMut(&Definition)) {
    for def in info.definitions.iter().flatten() {
        emit(def);
    }
    for (_, sub) in &info.import_modules {
        walk_module(sub, emit);
    }
}

/// Object field completions for `obj_name.` by finding the Let binding for
/// `obj_name` and returning its `ObjectKey` children.
fn complete_object_field(definitions: &[Definition], obj_name: &str) -> Vec<CompletionItem> {
    let Some(let_def) = definitions
        .iter()
        .find(|d| d.name == obj_name && matches!(d.kind, DefKind::Let { .. }))
    else {
        return Vec::new();
    };
    let let_span = let_def.full_span;
    definitions
        .iter()
        .filter(|d| {
            matches!(d.kind, DefKind::ObjectKey { .. })
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

/// Fallback for dot-completion when no parser-derived definitions are
/// available (parse failed mid-keystroke and no previous good snapshot
/// exists). Scans the token stream for `let OBJ = { KEY: ..., KEY: ... }`
/// and extracts the KEY identifiers.
fn complete_object_field_from_tokens(
    tokens: &[Token],
    text: &str,
    obj_name: &str,
) -> Vec<CompletionItem> {
    let mut i = 0;
    while i + 3 < tokens.len() {
        if tokens[i].kind == TokenKind::KwLet
            && tokens[i + 1].kind == TokenKind::Ident
            && &text[tokens[i + 1].span.start as usize..tokens[i + 1].span.end as usize] == obj_name
            && tokens[i + 2].kind == TokenKind::Eq
            && tokens[i + 3].kind == TokenKind::LBrace
        {
            let mut items = Vec::new();
            let mut j = i + 4;
            let mut depth = 1u32;
            while j < tokens.len() && depth > 0 {
                match tokens[j].kind {
                    TokenKind::LBrace => depth += 1,
                    TokenKind::RBrace => depth -= 1,
                    TokenKind::Ident
                        if depth == 1
                            && tokens
                                .get(j + 1)
                                .is_some_and(|t| t.kind == TokenKind::Colon) =>
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

fn grammar_config_field_items() -> Vec<CompletionItem> {
    GRAMMAR_CONFIG_FIELDS
        .iter()
        .map(|&(name, ty)| CompletionItem {
            label: name.into(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(format!("{name}: {ty}")),
            ..Default::default()
        })
        .collect()
}
