use ropey::Rope;
use rustc_hash::FxHashMap;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensLegend,
    SemanticTokensParams, SemanticTokensResult,
};

use tree_sitter_generate::nativedsl::lexer::TokenKind;

use crate::cst::ColorKind;
use crate::document::{DefKind, Module, RefKind};
use crate::server::Backend;
use crate::text;

// Semantic token types - indices into the legend.
// 0..=3 classify identifiers in grammar source; 4..=9 classify regions
// of the rendered CST in REPL tree buffers (see `tree_token_type`).
const TYPE_FUNCTION: u32 = 0;
const TYPE_VARIABLE: u32 = 1;
const TYPE_TYPE: u32 = 2;
const TYPE_CLASS: u32 = 3; // rule names
const TYPE_STRING: u32 = 4;
const TYPE_PROPERTY: u32 = 5;
const TYPE_NUMBER: u32 = 6;
const TYPE_COMMENT: u32 = 7;
const TYPE_KEYWORD: u32 = 8;
const TYPE_OPERATOR: u32 = 9;

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
            SemanticTokenType::STRING,   // 4
            SemanticTokenType::PROPERTY, // 5
            SemanticTokenType::NUMBER,   // 6
            SemanticTokenType::COMMENT,  // 7
            SemanticTokenType::KEYWORD,  // 8
            SemanticTokenType::OPERATOR, // 9
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

    // Tree-side buffers: serve colored spans stashed by the most
    // recent parse_and_publish on the paired session. The tree
    // buffer's text is server-driven, so the spans index into the
    // session's `last_tree_text`.
    if let Some(tree_uri) = crate::repl::ReplTreeUri::try_from_uri(uri) {
        let input_uri = tree_uri.input_uri();
        let session = backend.repl_sessions.get(&input_uri)?;
        let g = session.lock().unwrap();
        let data = tree_tokens(&g.last_tree_text, &g.last_tree_spans);
        return Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        }));
    }

    let analysis = backend.get_analysis(uri)?;
    let tokens = compute_semantic_tokens(&analysis.source, &analysis.rope, &analysis);

    Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data: tokens,
    }))
}

/// Map a `ColorKind` from the CST renderer to one of the legend slots.
/// Picks the closest-meaning standard LSP token type so the highlight
/// renders sensibly under any colorscheme rather than needing a custom
/// theme.
const fn tree_token_type(kind: ColorKind) -> u32 {
    match kind {
        ColorKind::NodeKind => TYPE_TYPE,
        ColorKind::Field => TYPE_PROPERTY,
        ColorKind::RowColor | ColorKind::RowColorNamed => TYPE_NUMBER,
        ColorKind::Extra => TYPE_COMMENT,
        ColorKind::Error | ColorKind::Missing => TYPE_KEYWORD,
        ColorKind::Backtick => TYPE_OPERATOR,
        ColorKind::NodeText | ColorKind::LineFeed | ColorKind::Literal => TYPE_STRING,
    }
}

/// Convert byte-offset spans (into `text`) into LSP semantic-token
/// deltas. Walks `text` line-by-line so each span can resolve to a
/// (line, character) pair; spans are assumed sorted by start byte.
fn tree_tokens(text: &str, spans: &[crate::cst::CstSpan]) -> Vec<SemanticToken> {
    if spans.is_empty() {
        return Vec::new();
    }
    // Precompute (line, line_start_byte) so a span's line lookup is
    // a binary search and its column is just `start - line_start`.
    // LSP requires column counts in UTF-16, but the CST output is
    // ASCII + escape sequences, so `byte == utf16_unit` everywhere
    // we emit a span. (If the rendered text ever included multi-byte
    // characters mid-span this would need a code-unit conversion.)
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }

    let mut out = Vec::with_capacity(spans.len());
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;
    for span in spans {
        let line_idx = line_starts.partition_point(|&s| s <= span.start) - 1;
        let line_start = line_starts[line_idx];
        let line = line_idx as u32;
        let col = (span.start - line_start) as u32;
        let length = (span.end - span.start) as u32;
        // Skip zero-length or cross-line spans. The renderer doesn't
        // emit cross-line styled regions today (newlines are pushed
        // plain by render_node), so this is a defensive guard.
        if length == 0
            || span.end
                > line_start
                    + (text[line_start..]
                        .find('\n')
                        .unwrap_or(text.len() - line_start))
        {
            continue;
        }
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 { col - prev_col } else { col };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: tree_token_type(span.kind),
            token_modifiers_bitset: 0,
        });
        prev_line = line;
        prev_col = col;
    }
    out
}

fn compute_semantic_tokens(text: &str, rope: &Rope, analysis: &Module) -> Vec<SemanticToken> {
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
            // For `mod::foo`, resolve through the import chain to the target
            // def and pick the color from its kind, so qualified access reads
            // the same way as bare-name access does.
            RefKind::ImportedMember { path, member } => {
                let Some(module) = analysis.resolve_import_chain(path) else {
                    continue;
                };
                let Some(def) = module
                    .definitions
                    .iter()
                    .flatten()
                    .find(|d| &d.name == member)
                else {
                    continue;
                };
                match def.kind {
                    DefKind::Rule | DefKind::OverrideRule => (TYPE_CLASS, 0),
                    DefKind::Function { .. } => (TYPE_FUNCTION, 0),
                    _ => (TYPE_VARIABLE, 0),
                }
            }
            RefKind::ObjectField { .. }
            | RefKind::InheritPath(_)
            | RefKind::ImportPath(_)
            | RefKind::Builtin => continue,
        };
        index.insert(reference.span.start, value);
    }
    for def in analysis.definitions.iter().flatten() {
        let value = match def.kind {
            DefKind::Rule | DefKind::OverrideRule => (TYPE_CLASS, MOD_DECLARATION),
            DefKind::Function { .. } => (TYPE_FUNCTION, MOD_DECLARATION),
            DefKind::Let { .. }
            | DefKind::Import
            | DefKind::Inherit
            | DefKind::Forward
            | DefKind::Parameter { .. } => (TYPE_VARIABLE, MOD_DECLARATION),
            DefKind::ObjectKey { .. } => continue,
        };
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
        let classification = if text::DSL_TYPE_NAMES.contains(&name) {
            Some((TYPE_TYPE, 0))
        } else {
            index.get(&token.span.start).copied()
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
