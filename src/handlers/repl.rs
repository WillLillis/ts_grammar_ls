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
use crate::repl::{self, ReplMeta, ReplSession};
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
    write_initial_input(&repl_path, &rule_name);

    // Persist the grammar URI in a sibling metadata file so the LSP
    // process that eventually receives `did_open` on the REPL buffer
    // (often a different process than the one running this command -
    // see `ReplMeta`'s docstring) can rebuild the session without
    // requiring shared memory.
    let meta = ReplMeta {
        grammar_uri: args.uri.clone(),
    };
    let _ = meta.write_for(&repl_path);

    let repl_uri = Url::from_file_path(&repl_path).ok()?;

    // Register the session locally too. In single-process setups this
    // avoids a redundant disk read on the upcoming `did_open`.
    backend.repl_sessions.insert(
        repl_uri.clone(),
        std::sync::Mutex::new(ReplSession {
            grammar_uri: args.uri.clone(),
            current_rule: rule_name.clone(),
            last_key: None,
            language: None,
        }),
    );

    // Kick off the first compile asynchronously; the user can already
    // start typing while the parser is being built. Result lands on the
    // session.
    spawn_compile(backend, repl_uri.clone());
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
    repl_dir().join(format!("{stem}{}", crate::repl::REPL_INPUT_SUFFIX))
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

/// Called from `did_change`/`did_open` when the touched URI is a REPL
/// input buffer. Re-parses the header to detect rule changes and
/// triggers a recompile when the (grammar, rule) fingerprint differs
/// from the session's last-cached key.
///
/// Step 4 of the REPL feature: this just keeps the session's compiled
/// `Language` fresh. Actually parsing the REPL text + publishing
/// diagnostics + updating the tree-view buffer lands in step 5.
pub fn handle_repl_change(backend: &Backend, repl_uri: &Url, text: &str) {
    let new_rule = match repl::parse_rule_header(text) {
        Some(name) => name.to_owned(),
        None => {
            tracing::info!("repl: missing header on {repl_uri}; nothing to do");
            return; // Step #39 follow-up: fall back to grammar start.
        }
    };

    // Locate / rebuild the session. If this LSP process is fresh (e.g.
    // a second instance spawned by lspconfig for the REPL buffer's
    // root_dir), the in-memory map is empty. Reconstruct the session
    // by reading the sibling metadata file written by `tsg.openRepl`.
    if !backend.repl_sessions.contains_key(repl_uri) {
        let Ok(repl_path) = repl_uri.to_file_path() else {
            tracing::info!("repl: non-file repl uri {repl_uri}");
            return;
        };
        let Some(meta) = ReplMeta::read_for(&repl_path) else {
            tracing::info!(
                "repl: no sibling metadata for {repl_uri}; user opened a REPL buffer \
                 without going through tsg.openRepl"
            );
            return;
        };
        tracing::info!(
            "repl: rebuilding session for {repl_uri} (grammar={})",
            meta.grammar_uri
        );
        backend.repl_sessions.insert(
            repl_uri.clone(),
            std::sync::Mutex::new(ReplSession {
                grammar_uri: meta.grammar_uri,
                current_rule: new_rule.clone(),
                last_key: None,
                language: None,
            }),
        );
        spawn_compile(backend, repl_uri.clone());
        return;
    }

    let session_ref = backend.repl_sessions.get(repl_uri).expect("checked above");
    // Mutate the rule if needed under the session lock, then drop the
    // map ref before spawning so we don't hold a DashMap guard across
    // an await. The compile task takes its own clone of the session URI.
    {
        let mut session = session_ref.lock().unwrap();
        if session.current_rule == new_rule && session.language.is_some() {
            tracing::info!("repl: {repl_uri} rule={new_rule} unchanged, language cached");
            return;
        }
        session.current_rule = new_rule.clone();
    }
    drop(session_ref);
    tracing::info!("repl: {repl_uri} rule={new_rule}; spawning compile");
    spawn_compile(backend, repl_uri.clone());
}

/// Asynchronously prepare + compile a parser for the current state of
/// the session at `repl_uri`. On success, swaps the loaded `Language`
/// into the session. Failures are logged at `warn` level; in step 5
/// they'll become diagnostics on the REPL buffer.
fn spawn_compile(backend: &Backend, repl_uri: Url) {
    let cache = std::sync::Arc::clone(&backend.repl_cache);
    let sessions = std::sync::Arc::clone(&backend.repl_sessions);
    let grammar_uri;
    let rule_name;
    {
        let Some(session_ref) = sessions.get(&repl_uri) else {
            return;
        };
        let session = session_ref.lock().unwrap();
        grammar_uri = session.grammar_uri.clone();
        rule_name = session.current_rule.clone();
    }
    // The lowered InputGrammar comes from the loader, so we need to
    // re-run `analyze` against the current grammar text. Source for
    // that text:
    //   - in-memory `document_map` when this process has the grammar
    //     buffer open (single-process setup),
    //   - on-disk file otherwise (the common multi-process case where
    //     the REPL buffer lives under a different `root_dir` than the
    //     grammar and got spawned its own LSP instance).
    let grammar_text = backend
        .document_map
        .get(&grammar_uri)
        .map(|d| d.text.clone())
        .or_else(|| {
            grammar_uri
                .to_file_path()
                .ok()
                .and_then(|p| std::fs::read_to_string(p).ok())
        });
    let Some(grammar_text) = grammar_text else {
        tracing::warn!("repl: couldn't load grammar text for {grammar_uri}");
        return;
    };
    let Some(outcome) = crate::analysis::analyze(grammar_text, &grammar_uri) else {
        return;
    };
    let Some(Ok(grammar)) = outcome.pipeline else {
        tracing::warn!("repl: grammar didn't reach lower stage");
        return;
    };
    let prepared = match crate::repl::ReplCache::prepare(&grammar, &rule_name) {
        Ok(p) => p,
        Err(crate::repl::ReplCompileError::RuleNotFound) => {
            // Common transient state while the user is typing the rule
            // name (e.g. "i", "id", "ide" before settling on
            // "identifier"). Logging this at warn level produces noisy
            // bursts; demote to debug.
            tracing::debug!("repl: rule `{rule_name}` not in grammar");
            return;
        }
        Err(e) => {
            tracing::warn!("repl: prepare failed: {e}");
            return;
        }
    };
    let (json, key) = prepared;

    // Compare against last_key. If the session was just recompiled for
    // the same key (e.g. concurrent did_change events arrived), skip.
    {
        let Some(session_ref) = sessions.get(&repl_uri) else {
            return;
        };
        let session = session_ref.lock().unwrap();
        if session.last_key.as_ref() == Some(&key) && session.language.is_some() {
            return;
        }
    }

    tokio::spawn(async move {
        tracing::info!("repl: compile start key={}", key.as_str());
        match cache.get_or_compile(key.clone(), json).await {
            Ok(language) => {
                tracing::info!("repl: compile done key={}", key.as_str());
                if let Some(session_ref) = sessions.get(&repl_uri) {
                    let mut session = session_ref.lock().unwrap();
                    session.language = Some(language);
                    session.last_key = Some(key);
                }
            }
            Err(e) => {
                tracing::warn!("repl: compile failed: {e}");
            }
        }
    });
}

/// Reset the REPL input to just the header line. The REPL is intended
/// to be ephemeral: prior session content shouldn't bleed across
/// invocations, since the user's mental model is "this is a scratch
/// buffer that opens fresh each time".
fn write_initial_input(path: &std::path::Path, rule_name: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let initial = format!("# rule: {rule_name}\n");
    let _ = std::fs::write(path, initial);
}
