use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Command, FromArgMatches as _, Subcommand};
use tower_lsp::{LspService, Server};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use ts_grammar_ls::server::Backend;

const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Subcommand)]
#[command(about = "LSP for tree-sitter grammar DSL (.tsg) files")]
enum Commands {
    /// Validate a grammar by running the full parser generation pipeline.
    /// Reads grammar JSON from stdin, prints errors to stdout.
    GenerateCheck(GenerateCheck),
    /// Format one or more .tsg files.
    Format(Format),
    /// Run lints against a grammar file (or grammar.tsg in a directory).
    /// Helpers reached through inherit / import are linted via the same
    /// pass - no need to list them separately.
    Lint(Lint),
}

#[derive(Args)]
struct GenerateCheck {
    /// Also write `grammar.json`, `src/parser.c`, and the tree-sitter
    /// headers (`src/tree_sitter/*.h`) under this directory, in addition
    /// to validating the grammar. Used by the LSP to drive the REPL
    /// compile pipeline without a second subprocess hop.
    #[arg(long)]
    write_to: Option<PathBuf>,
}

#[derive(Args)]
struct Lint {
    /// Grammar file to lint, or a directory containing `grammar.tsg`.
    path: PathBuf,
    /// Disable a lint by name (e.g. `alias-over-supertype`). Repeatable.
    #[arg(long, value_name = "NAME")]
    allow: Vec<String>,
    /// Apply each finding's suggested fix in place, rewriting the file.
    #[arg(long)]
    fix: bool,
}

#[derive(Args)]
struct Format {
    /// Files or directories to format. Directories are searched recursively for .tsg files.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    /// Check formatting without writing. Exits non-zero if changes needed.
    #[arg(long, short)]
    check: bool,
    /// Path to a configuration file.
    #[arg(long, short = 'C')]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Command::new("ts_grammar_ls")
        .version(BUILD_VERSION)
        .about("LSP for tree-sitter grammar DSL (.tsg) files");
    let cli = Commands::augment_subcommands(cli);

    let matches = cli.get_matches();

    if let Ok(command) = Commands::from_arg_matches(&matches) {
        match command {
            Commands::GenerateCheck(args) => {
                ts_grammar_ls::generate_check::run(args.write_to.as_deref());
            }
            Commands::Format(fmt) => {
                std::process::exit(ts_grammar_ls::cli::format::run(
                    &fmt.paths,
                    fmt.check,
                    fmt.config.as_deref(),
                ));
            }
            Commands::Lint(args) => {
                std::process::exit(ts_grammar_ls::cli::lint::run(
                    &args.path,
                    &args.allow,
                    args.fix,
                ));
            }
        }
        return;
    }

    // No subcommand: run the LSP server.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ts_grammar_ls=info".into()),
        )
        // No ANSI: stderr goes to the client's LSP log file, where escape
        // sequences just become noise (`\27[2m...`) rather than coloring.
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(|client| Backend {
        client,
        document_map: Arc::new(dashmap::DashMap::new()),
        publish_handle: Arc::new(dashmap::DashMap::new()),
        generate_child: Arc::default(),
        dependents: Arc::new(dashmap::DashMap::new()),
        closed_file_deps: Arc::new(dashmap::DashMap::new()),
        workspace_roots: Arc::default(),
        config: Arc::default(),
        repl_cache: Arc::new(ts_grammar_ls::repl::ReplCache::new(
            ts_grammar_ls::repl::default_cache_root(),
        )),
        repl_sessions: Arc::new(dashmap::DashMap::new()),
    })
    .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}
