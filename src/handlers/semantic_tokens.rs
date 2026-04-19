use ropey::Rope;
use rustc_hash::FxHashMap;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensLegend,
    SemanticTokensParams, SemanticTokensResult,
};

use tree_sitter_generate::nativedsl::lexer::TokenKind;

use crate::document::{Analysis, DefKind, RefKind};
use crate::server::Backend;
use crate::text;

// Semantic token types - indices into the legend.
// We only emit tokens for identifiers that the tree-sitter highlighting
// grammar can't classify (it doesn't know what names resolve to).
const TYPE_FUNCTION: u32 = 0;
const TYPE_VARIABLE: u32 = 1;
const TYPE_TYPE: u32 = 2;
const TYPE_CLASS: u32 = 3; // rule names

// Modifier bits.
const MOD_DECLARATION: u32 = 1 << 0;

#[must_use]
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::FUNCTION, // 0
            SemanticTokenType::VARIABLE, // 1
            SemanticTokenType::TYPE,     // 2
            SemanticTokenType::CLASS,    // 3
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION, // bit 0
        ],
    }
}

#[must_use]
pub fn semantic_tokens_full(
    backend: &Backend,
    params: &SemanticTokensParams,
) -> Option<SemanticTokensResult> {
    let uri = &params.text_document.uri;
    let analysis = backend.get_analysis(uri)?;
    let tokens = compute_semantic_tokens(&analysis.source, &analysis.rope, &analysis);

    Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data: tokens,
    }))
}

fn compute_semantic_tokens(text: &str, rope: &Rope, analysis: &Analysis) -> Vec<SemanticToken> {
    let Some(lex_tokens) = analysis.tokens.as_deref() else {
        return Vec::new();
    };

    // Build a span-keyed classification index once. Defs take priority over
    // refs at the same offset (definition site).
    let mut index: FxHashMap<u32, (u32, u32)> = FxHashMap::default();
    for reference in analysis.references.iter().flatten() {
        let value = match &reference.kind {
            RefKind::Rule(_) | RefKind::BaseRule(_) => (TYPE_CLASS, 0),
            RefKind::Variable(_) => (TYPE_VARIABLE, 0),
            RefKind::ObjectField { .. }
            | RefKind::InheritPath
            | RefKind::ImportPath
            | RefKind::ImportedMember { .. }
            | RefKind::Builtin => continue,
        };
        index.insert(reference.span.start, value);
    }
    for def in analysis.definitions.iter().flatten() {
        let value = match def.kind {
            DefKind::Rule | DefKind::OverrideRule => (TYPE_CLASS, MOD_DECLARATION),
            DefKind::Function { .. } => (TYPE_FUNCTION, MOD_DECLARATION),
            DefKind::Let { .. } | DefKind::Import | DefKind::Inherit => {
                (TYPE_VARIABLE, MOD_DECLARATION)
            }
            DefKind::ObjectKey | DefKind::Parameter { .. } => continue,
        };
        // Defs override refs (e.g. parameter declaration site).
        index.insert(def.name_span.start, value);
    }

    let mut result = Vec::new();
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;

    for token in lex_tokens {
        if token.kind != TokenKind::Ident {
            continue;
        }

        let name = &text[token.span.start as usize..token.span.end as usize];
        let classification = match name {
            "rule_t" | "str_t" | "int_t" | "list_rule_t" | "list_str_t" | "list_int_t"
            | "list_list_rule_t" | "list_list_str_t" | "list_list_int_t" | "void_t"
            | "spread_t" => Some((TYPE_TYPE, 0)),
            _ => index.get(&token.span.start).copied(),
        };
        let Some((token_type, modifiers)) = classification else {
            continue;
        };

        let pos = text::offset_to_position(rope, token.span.start);
        let length = token.span.end - token.span.start;

        let delta_line = pos.line - prev_line;
        let delta_start = if delta_line == 0 {
            pos.character - prev_col
        } else {
            pos.character
        };

        result.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: modifiers,
        });

        prev_line = pos.line;
        prev_col = pos.character;
    }

    result
}
