use std::io::Read;

use tree_sitter_generate::generate_parser_for_grammar;

/// Subprocess entry point for the generate-check command.
///
/// Reads grammar JSON from stdin, runs the full `generate_parser_for_grammar`
/// pipeline, and prints any error to stdout. Exits 0 on success, 1 on error.
/// This runs as a child process so the LSP can kill it on cancellation.
///
/// # Panics
///
/// Panics if the `grammar.json` contents cannot be read from stdin.
pub fn run() {
    let mut json = String::new();
    std::io::stdin().read_to_string(&mut json).unwrap();

    match generate_parser_for_grammar(&json, None) {
        Ok(_) => {}
        Err(e) => {
            println!("{e}");
            std::process::exit(1);
        }
    }
}
