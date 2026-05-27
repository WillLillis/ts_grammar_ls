//! `tsg.openRepl` workspace command.
//!
//! Creates (or reuses) a REPL "input" buffer for the user's grammar:
//! a real file under `$XDG_CACHE_HOME/ts_grammar_ls/repl/` with a stable
//! per-grammar path. The file's first line is `# rule: <name>` (header);
//! subsequent lines are the parse input. Compilation and per-keystroke
//! parsing land in later commits - this step only stands the buffer up
//! and points the client at it via `window/showDocument`.
//!
//! Arguments (single JSON object in `arguments[0]`):
//! - `uri` (string, required): grammar URI to open a REPL session for.
//! - `position` ({line, character}, optional): cursor in the grammar. If
//!   provided and inside a `rule X { ... }` declaration, `X` is chosen
//!   as the start rule. Otherwise the grammar's first rule is used.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use serde::Deserialize;
use tower_lsp::lsp_types::{
    ExecuteCommandParams, Position, ShowDocumentParams, Url,
};

use crate::document::DefKind;
use crate::server::Backend;

pub const OPEN_REPL_COMMAND: &str = "tsg.openRepl";

#[derive(Deserialize, Debug)]
struct OpenReplArgs {
    uri: Url,
    #[serde(default)]
    position: Option<Position>,
}

/// Handle `workspace/executeCommand` for `tsg.openRepl`. Returns the
/// REPL input URI as JSON on success so the client knows where the
/// session landed; opening the file is also pushed via `window/showDocument`.
pub async fn open_repl(backend: &Backend, params: &ExecuteCommandParams) -> Option<serde_json::Value> {
    if params.command != OPEN_REPL_COMMAND {
        return None;
    }
    let args = params.arguments.first()?;
    let args: OpenReplArgs = serde_json::from_value(args.clone()).ok()?;

    let rule_name = resolve_default_rule(backend, &args)?;
    let repl_path = repl_input_path(&args.uri);
    write_initial_input_if_absent(&repl_path, &rule_name);

    let repl_uri = Url::from_file_path(&repl_path).ok()?;
    // Fire `window/showDocument` as a detached task. The request awaits
    // a client response, and we don't want our `executeCommand` reply
    // gated on that round trip (the URI is in our return value anyway).
    let client = backend.client.clone();
    let show_uri = repl_uri.clone();
    tokio::spawn(async move {
        let _ = client
            .show_document(ShowDocumentParams {
                uri: show_uri,
                external: Some(false),
                take_focus: Some(true),
                selection: None,
            })
            .await;
    });

    Some(serde_json::json!({ "uri": repl_uri.to_string(), "rule": rule_name }))
}

/// Cursor-aware rule pick: if `position` falls inside a `rule X { ... }`
/// declaration in the grammar's analysis, return `X`. Otherwise return
/// the first `Rule` / `OverrideRule` definition in source order, which
/// is what `InputGrammar::normalize` treats as the implicit start.
fn resolve_default_rule(backend: &Backend, args: &OpenReplArgs) -> Option<String> {
    let analysis = backend.get_analysis(&args.uri)?;
    let defs = analysis.definitions.as_ref()?;

    if let Some(pos) = args.position {
        let offset = crate::text::position_to_offset(&analysis.rope, pos)?;
        if let Some(def) = defs.iter().find(|d| {
            matches!(d.kind, DefKind::Rule | DefKind::OverrideRule)
                && d.full_span.start <= offset
                && offset < d.full_span.end
        }) {
            return Some(def.name.clone());
        }
    }
    defs.iter()
        .find(|d| matches!(d.kind, DefKind::Rule | DefKind::OverrideRule))
        .map(|d| d.name.clone())
}

/// Stable on-disk path for a grammar's REPL input buffer. Hash of the
/// grammar URI keeps the path deterministic across LSP restarts, so
/// reopening the REPL for the same grammar reuses the same buffer.
fn repl_input_path(grammar_uri: &Url) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    grammar_uri.as_str().hash(&mut hasher);
    let stem = format!("{:016x}", hasher.finish());
    repl_dir().join(format!("{stem}.tsg-repl-input.txt"))
}

/// `$XDG_CACHE_HOME/ts_grammar_ls/repl/`. Falls back to `/tmp/` if the
/// cache base can't be determined.
fn repl_dir() -> PathBuf {
    use etcetera::BaseStrategy as _;
    let base = etcetera::choose_base_strategy()
        .ok()
        .map(|s| s.cache_dir())
        .unwrap_or_else(std::env::temp_dir);
    base.join("ts_grammar_ls").join("repl")
}

/// Write the header line on first open. If the file already exists,
/// leave it alone so the user's prior session content survives.
fn write_initial_input_if_absent(path: &std::path::Path, rule_name: &str) {
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let initial = format!("# rule: {rule_name}\n");
    let _ = std::fs::write(path, initial);
}
