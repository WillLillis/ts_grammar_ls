use std::io::Read;
use std::path::Path;

use tree_sitter_generate::{
    ALLOC_HEADER, ARRAY_HEADER, PARSER_HEADER, generate_parser_for_grammar,
};

/// Subprocess entry point for the generate-check command.
///
/// Reads grammar JSON from stdin and runs the full
/// `generate_parser_for_grammar` pipeline. On error, prints to stdout and
/// exits 1. On success exits 0; if `write_to` is set, the generated
/// `parser.c`, the input `grammar.json`, and the tree-sitter headers
/// (`tree_sitter/{alloc,array,parser}.h`) are written under
/// `<write_to>/src/` first.
///
/// Runs as a child process so the LSP can kill it on cancellation.
///
/// # Panics
///
/// Panics if the grammar JSON cannot be read from stdin, or if the output
/// directory is not writable when `write_to` is provided.
pub fn run(write_to: Option<&Path>) {
    let mut json = String::new();
    std::io::stdin().read_to_string(&mut json).unwrap();

    let (_name, c_code) = match generate_parser_for_grammar(&json, None) {
        Ok(out) => out,
        Err(e) => {
            println!("{e}");
            std::process::exit(1);
        }
    };

    if let Some(dir) = write_to {
        write_artifacts(dir, &json, &c_code).expect("write generated artifacts");
    }
}

fn write_artifacts(dir: &Path, grammar_json: &str, parser_c: &str) -> std::io::Result<()> {
    let src = dir.join("src");
    let headers = src.join("tree_sitter");
    std::fs::create_dir_all(&headers)?;
    std::fs::write(src.join("grammar.json"), grammar_json)?;
    std::fs::write(src.join("parser.c"), parser_c)?;
    std::fs::write(headers.join("alloc.h"), ALLOC_HEADER)?;
    std::fs::write(headers.join("array.h"), ARRAY_HEADER)?;
    std::fs::write(headers.join("parser.h"), PARSER_HEADER)?;
    Ok(())
}
