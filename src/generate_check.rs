use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
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
    let (_name, parser_c) =
        generate_parser_for_grammar(grammar_json, None).map_err(GenerateToDirError::Codegen)?;
    let src = dir.join("src");
    let headers = src.join("tree_sitter");
    std::fs::create_dir_all(&headers)
        .map_err(|e| GenerateToDirError::Io(IoError::new(&e, Some(&headers))))?;
    for (path, content) in &[
        (src.join("grammar.json"), grammar_json),
        (src.join("parser.c"), &parser_c),
        (headers.join("alloc.h"), ALLOC_HEADER),
        (headers.join("array.h"), ARRAY_HEADER),
        (headers.join("parser.h"), PARSER_HEADER),
    ] {
        std::fs::write(path, content)
            .map_err(|e| GenerateToDirError::Io(IoError::new(&e, Some(path))))?;
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Error)]
#[error(transparent)]
pub enum GenerateToDirError {
    Codegen(#[from] tree_sitter_generate::GenerateError),
    Io(IoError),
}

#[derive(Debug, Error, Serialize, Deserialize)]
pub struct IoError {
    pub error: String,
    pub path: Option<String>,
}

impl IoError {
    fn new(error: &std::io::Error, path: Option<&Path>) -> Self {
        Self {
            error: error.to_string(),
            path: path.map(|p| p.to_string_lossy().to_string()),
        }
    }
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)?;
        if let Some(ref path) = self.path {
            write!(f, " ({path})")?;
        }
        Ok(())
    }
}

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
            .map_err(GenerateToDirError::Codegen),
    };

    if let Err(e) = result {
        println!("{}", serde_json::to_string(&e).unwrap());
        std::process::exit(1);
    }
}
