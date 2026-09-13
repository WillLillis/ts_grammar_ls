use tree_sitter_generate::nativedsl::RulePool;
use tree_sitter_generate::nativedsl::ast::SharedAst;
use tree_sitter_generate::nativedsl::lexer::Lexer;
use tree_sitter_generate::nativedsl::parser::Parser;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/last_formatted.tsg".to_string());
    let src = std::fs::read_to_string(&path).unwrap();
    eprintln!("checking {path}");
    let parse_path = std::path::PathBuf::from(&path);
    let (documents, document_id) =
        ts_grammar_ls::analysis::document_map_for_source(&parse_path, &src);
    let document = documents.document(document_id);
    let tokens = match Lexer::new(document).tokenize() {
        Ok(t) => t,
        Err(e) => {
            println!("LEX ERR: {e:?}");
            return;
        }
    };
    let mut shared = SharedAst::new(0);
    let mut pool = RulePool::default();
    let result = Parser::new(&tokens, document, &mut shared, pool.strs_mut()).parse();
    match result {
        Ok(_) => println!("OK"),
        Err(e) => {
            println!("PARSE ERR: {e:?}");
            // Find the span
            let span = e.span.unwrap();
            let prefix = &src[..span.start as usize];
            let line = prefix.matches('\n').count() + 1;
            let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
            let col = span.start as usize - line_start;
            println!(
                "at line {} col {} bytes {}..{}",
                line, col, span.start, span.end
            );
            let line_text = src.lines().nth(line - 1).unwrap_or("");
            println!("  | {line_text}");
            println!("  | {}^", " ".repeat(col));
        }
    }
}
