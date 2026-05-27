use std::io::Read;
use std::path::Path;

use tree_sitter_generate::{
    ALLOC_HEADER, ARRAY_HEADER, PARSER_HEADER, generate_parser_for_grammar,
};

/// Run the upstream codegen pipeline on `grammar_json` and lay out the
/// resulting artifacts (`grammar.json`, `parser.c`, the tree-sitter
/// headers) under `<dir>/src/`. The directory matches what
/// `tree_sitter generate` produces, so it's directly consumable by
/// `tree_sitter_loader::Loader::load_language_at_path`.
///
/// In-process. The LSP's REPL pipeline uses this both directly (from
/// tests, where the subprocess hop is unreachable from the test binary)
/// and indirectly via the `generate-check` subprocess (production, where
/// the subprocess gives us cancellation isolation).
///
/// # Errors
///
/// `Err` payload is a human-readable message from the codegen pipeline
/// (`Codegen`) or an IO failure on the artifact writes (`Io`).
pub fn generate_to_dir(dir: &Path, grammar_json: &str) -> Result<(), GenerateToDirError> {
    let (_name, parser_c) = generate_parser_for_grammar(grammar_json, None)
        .map_err(|e| GenerateToDirError::Codegen(e.to_string()))?;
    let src = dir.join("src");
    let headers = src.join("tree_sitter");
    std::fs::create_dir_all(&headers).map_err(GenerateToDirError::Io)?;
    std::fs::write(src.join("grammar.json"), grammar_json).map_err(GenerateToDirError::Io)?;
    std::fs::write(src.join("parser.c"), parser_c).map_err(GenerateToDirError::Io)?;
    std::fs::write(headers.join("alloc.h"), ALLOC_HEADER).map_err(GenerateToDirError::Io)?;
    std::fs::write(headers.join("array.h"), ARRAY_HEADER).map_err(GenerateToDirError::Io)?;
    std::fs::write(headers.join("parser.h"), PARSER_HEADER).map_err(GenerateToDirError::Io)?;
    Ok(())
}

#[derive(Debug)]
pub enum GenerateToDirError {
    Codegen(String),
    Io(std::io::Error),
}

impl std::fmt::Display for GenerateToDirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codegen(msg) => f.write_str(msg),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for GenerateToDirError {}

/// Subprocess entry point for the generate-check command.
///
/// Reads grammar JSON from stdin and runs the codegen pipeline. On
/// failure, prints to stdout and exits 1. On success exits 0; if
/// `write_to` is set, artifacts are laid out via [`generate_to_dir`].
///
/// Runs as a child process so the LSP can kill it on cancellation.
///
/// # Panics
///
/// Panics if the grammar JSON cannot be read from stdin.
pub fn run(write_to: Option<&Path>) {
    let mut json = String::new();
    std::io::stdin().read_to_string(&mut json).unwrap();

    let result = match write_to {
        Some(dir) => generate_to_dir(dir, &json),
        // Validation-only path: still produce artifacts (cheap) but throw
        // them away. Routes through the same error surface.
        None => generate_parser_for_grammar(&json, None)
            .map(|_| ())
            .map_err(|e| GenerateToDirError::Codegen(e.to_string())),
    };

    if let Err(e) = result {
        println!("{e}");
        std::process::exit(1);
    }
}
