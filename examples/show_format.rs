use std::path::PathBuf;
use ts_grammar_ls::config::FormattingConfig;
use ts_grammar_ls::formatter;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let src = std::fs::read_to_string(&path).unwrap();
    let cfg = FormattingConfig::default();
    let out = formatter::format(&src, &PathBuf::from(&path), &cfg).unwrap();
    print!("{out}");
}
