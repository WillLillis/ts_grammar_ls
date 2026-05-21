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

use tree_sitter_generate::nativedsl::ast::{NodeId, SharedAst};
use tree_sitter_generate::nativedsl::lexer::Lexer;
use tree_sitter_generate::nativedsl::parser::Parser;

use crate::config::FormattingConfig;
use crate::document::Module;

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

/// Render a macro body with caller-supplied args substituted for each
/// `MacroParam`. Used by the "inline macro call" code action: the LSP
/// resolves the call's macro to a body NodeId and passes the call's args
/// + caller module; this returns the source text that should replace the
/// call's span.
///
/// `body_module` is the module the macro is defined in (provides the
/// source for body spans and the import lookup for any nested
/// `QualifiedCall`s inside the body). `caller_module` is where the call
/// expression itself lives (provides the source for the args' spans).
/// For a local `Node::Call`, both are the same module; for a
/// `Node::QualifiedCall`, `body_module` is the imported one.
///
/// Trivia is built from the caller module's source. Comments inside an
/// imported macro's body have spans in `body_module.source` and won't be
/// emitted - acceptable for V1; revisit by building per-module trivia
/// if it becomes a real problem.
#[must_use]
pub fn format_macro_expansion(
    body: NodeId,
    args: &[NodeId],
    body_module: &Module,
    caller_module: &Module,
    config: &FormattingConfig,
) -> String {
    let shared: &SharedAst = &body_module.shared;
    let tokens: Vec<_> = Lexer::new(&caller_module.source).tokenize().unwrap_or_default();
    let trivia = trivia::TriviaMap::build(&tokens, &caller_module.source);
    let mut arena = doc::DocArena::new();
    let root = visit::Printer::with_expansion(&mut arena, shared, &trivia, body_module)
        .expand(body, args, caller_module);
    let rendered = print::render_with_opts(
        &arena,
        root,
        config.indent_width,
        config.max_line_width,
    );
    // `render_with_opts` appends a trailing newline (whole-file shape);
    // strip it for an inline replacement.
    rendered.trim_end().to_owned()
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

