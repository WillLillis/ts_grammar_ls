//! Deterministic, comment-preserving formatter for `.tsg` source files.
//!
//! Pipeline:
//! 1. Lex + parse to a `ModuleContext` (we need a valid AST; on parse failure
//!    we return `None`).
//! 2. Collect trivia (line comments, blank-line markers, `// tsg-format: off/on`
//!    regions) from the token stream.
//! 3. Walk the AST emitting a `Doc` IR (see `doc.rs`).
//! 4. Render the `Doc` to a `String` with width-aware group breaks.
//!
//! The formatter is opinionated: client `FormattingOptions` are ignored,
//! layout is determined entirely by `FormattingConfig` + the input AST +
//! comment positions. Output is idempotent: `format(format(x)) == format(x)`.

use std::path::Path;

use tree_sitter_generate::nativedsl::ast::SharedAst;
use tree_sitter_generate::nativedsl::lexer::Lexer;
use tree_sitter_generate::nativedsl::parser::Parser;

use crate::config::FormattingConfig;

mod doc;
mod print;
mod trivia;
mod visit;

/// Format a `.tsg` source file. Returns `None` if the source can't be parsed
/// (the AST is required - we won't emit guesses from a partial parse).
#[must_use]
pub fn format(source: &str, path: &Path, config: &FormattingConfig) -> Option<String> {
    let tokens = Lexer::new(source).tokenize().ok()?;
    let mut shared = SharedAst::new(source.len() / 30);
    let ctx = Parser::new(&tokens, source.to_owned(), path.to_path_buf(), &mut shared)
        .parse()
        .ok()?;
    let trivia = trivia::TriviaMap::build(&tokens, source);
    let mut arena = doc::DocArena::new();
    let root = {
        let mut printer = visit::Printer::new(&mut arena, &shared, &ctx, &trivia);
        printer.module()
    };
    Some(print::render_with_opts(
        &arena,
        root,
        config.indent_width,
        config.max_line_width,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        format(src, Path::new("/tmp/x.tsg"), &FormattingConfig::default())
            .expect("format failed")
    }

    #[test]
    fn minimal_grammar() {
        let out = fmt("grammar { language: \"test\" }\nrule x { \"a\" }\n");
        let expected = "grammar {\n    language: \"test\",\n}\n\nrule x { \"a\" }\n";
        assert_eq!(out, expected);
    }
}

