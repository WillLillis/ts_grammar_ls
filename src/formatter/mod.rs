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

use crate::config::FormattingConfig;

mod doc;
mod print;

/// Format a `.tsg` source file. Returns `None` if the source can't be parsed
/// (the AST is required - we won't emit guesses from a partial parse).
#[must_use]
pub fn format(_source: &str, _path: &Path, _config: &FormattingConfig) -> Option<String> {
    // TODO: lex+parse, collect trivia, AST -> Doc, render.
    None
}
