//! Probe the current document-backed lexer/parser ownership model.
//!
//! Core owns source text in a `DocumentMap`; lexer tokens are trivia-free and
//! both lexer and parser borrow the same `DocumentRef`. The LSP recovers line
//! comments from gaps between tokens.
//!
//! Run: `cargo run --example lexed_ownership_probe`

use std::path::Path;

use tree_sitter_generate::nativedsl::RulePool;
use tree_sitter_generate::nativedsl::ast::{SharedAst, Span};
use tree_sitter_generate::nativedsl::lexer::Lexer;
use tree_sitter_generate::nativedsl::parser::Parser;

fn comment_ranges(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    source: &str,
) -> Vec<Span> {
    let mut comments = Vec::new();
    let mut previous_end = 0usize;
    for token in tokens {
        let gap_end = token.span.start as usize;
        let gap = &source[previous_end..gap_end];
        let mut cursor = 0usize;
        while let Some(relative) = gap[cursor..].find("//") {
            let start = cursor + relative;
            let end = gap[start..]
                .find('\n')
                .map_or(gap.len(), |newline| start + newline);
            comments.push(Span::new(
                (previous_end + start) as u32,
                (previous_end + end) as u32,
            ));
            cursor = end.saturating_add(1);
            if cursor >= gap.len() {
                break;
            }
        }
        previous_end = token.span.end as usize;
    }
    comments
}

fn main() {
    let source = r#"grammar {
    language: "probe", // trailing on the language value
    extras: [regexp(r"\s")],
}

// leading on the rule
rule program { repeat("x") } // trailing on the rule
"#;
    let path = Path::new("/tmp/lexed-ownership-probe.tsg");
    let (documents, document_id) = ts_grammar_ls::analysis::document_map_for_source(path, source);
    let document = documents.document(document_id);
    let tokens = Lexer::new(document).tokenize().expect("lexes");
    let comments = comment_ranges(&tokens, source);

    let mut shared = SharedAst::new(source.len() / 30);
    let mut pool = RulePool::default();
    let context = Parser::new(&tokens, document, &mut shared, pool.strs_mut())
        .parse()
        .expect("parses");

    println!(
        "document-backed roundtrip OK: {} tokens, {} comments, {} root items",
        tokens.len(),
        comments.len(),
        context.root_items.len()
    );
    for span in comments {
        println!("  {}", &source[span.start as usize..span.end as usize]);
    }
}
