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
        format: crate::repl::TreeFormat::default(),
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
            format: meta.format,
        }),
    );

    // Create the tree side buffer (empty until the first parse lands).
    // Doing it up front means the editor finds a real file when we ask
    // it to open one, rather than racing the first compile.
    let tree_path = crate::repl::tree_path_for(&repl_path);
    let _ = std::fs::write(&tree_path, "");
    let tree_uri = Url::from_file_path(&tree_path).ok();

    // Kick off the first compile asynchronously; the user can already
    // start typing while the parser is being built. Result lands on the
    // session.
    spawn_compile(backend, repl_uri.clone());

    // Fire `window/showDocument` as detached tasks for both buffers.
    // The request awaits a client response, and we don't want our
    // `executeCommand` reply gated on that round trip. Open the tree
    // buffer first (so it's the unfocused split) then the input (so
    // it ends up focused).
    let client = backend.client.clone();
    let input_uri = repl_uri.clone();
    let tree_uri_clone = tree_uri.clone();
    tokio::spawn(async move {
        if let Some(tu) = tree_uri_clone {
            let _ = client
                .show_document(ShowDocumentParams {
                    uri: tu,
                    external: Some(false),
                    take_focus: Some(false),
                    selection: None,
                })
                .await;
        }
        let _ = client
            .show_document(ShowDocumentParams {
                uri: input_uri,
                external: Some(false),
                take_focus: Some(true),
                selection: None,
            })
            .await;
    });

    Some(serde_json::json!({
        "uri": repl_uri.to_string(),
        "tree_uri": tree_uri.map(|u| u.to_string()),
        "rule": rule_name,
    }))
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
                format: meta.format,
            }),
        );
        spawn_compile(backend, repl_uri.clone());
        return;
    }

    let session_ref = backend.repl_sessions.get(repl_uri).expect("checked above");
    // Mutate the rule if needed under the session lock, then drop the
    // map ref before spawning so we don't hold a DashMap guard across
    // an await. The compile task takes its own clone of the session URI.
    let cached = {
        let mut session = session_ref.lock().unwrap();
        if session.current_rule == new_rule {
            // Rule unchanged: just re-parse with the existing language
            // (if any). The user is editing the input region, not the
            // header.
            session.language.clone().map(|lang| (lang, session.format))
        } else {
            session.current_rule = new_rule.clone();
            None // Rule changed -> drop into recompile path below.
        }
    };
    drop(session_ref);

    if let Some((language, format)) = cached {
        // Fast path: parse the new text with the existing compiled
        // language and publish results. No subprocess needed.
        let client = backend.client.clone();
        let repl_uri = repl_uri.clone();
        let text = text.to_owned();
        tokio::spawn(async move {
            parse_and_publish(client, repl_uri, text, language, format).await;
        });
        return;
    }

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

    let client = backend.client.clone();
    tokio::spawn(async move {
        tracing::info!("repl: compile start key={}", key.as_str());
        match cache.get_or_compile(key.clone(), json).await {
            Ok(language) => {
                tracing::info!("repl: compile done key={}", key.as_str());
                let format = sessions
                    .get(&repl_uri)
                    .map(|s| s.lock().unwrap().format)
                    .unwrap_or_default();
                if let Some(session_ref) = sessions.get(&repl_uri) {
                    let mut session = session_ref.lock().unwrap();
                    session.language = Some(std::sync::Arc::clone(&language));
                    session.last_key = Some(key);
                }
                // Re-parse the input region with the fresh language so
                // the tree buffer + diagnostics catch up to the rule
                // change (the user may have been waiting seconds for
                // this compile and now expects to see results).
                if let Ok(repl_path) = repl_uri.to_file_path()
                    && let Ok(text) = std::fs::read_to_string(&repl_path)
                {
                    parse_and_publish(client, repl_uri, text, language, format).await;
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

/// Parse the input region (everything after the `# rule:` header) with
/// the session's cached `Language`. Renders the parse tree as a
/// pretty-printed s-expression to the sibling tree file, and publishes
/// LSP diagnostics for any ERROR / MISSING nodes back on the input
/// buffer.
///
/// Triggered from two places:
///   - `handle_repl_change` after the rule + compile bookkeeping, when
///     a language is already on the session.
///   - `spawn_compile`'s completion path, so the tree updates as soon
///     as a fresh compile lands (the user changed the rule, waited a
///     few seconds, now wants to see the parse).
async fn parse_and_publish(
    client: tower_lsp::Client,
    repl_uri: Url,
    text: String,
    language: std::sync::Arc<tree_sitter::Language>,
    format: crate::repl::TreeFormat,
) {
    // Trim trailing whitespace so the user's editor-supplied final
    // newline doesn't show up as an unparsed-tail ERROR for rules whose
    // pattern stops at `\n` (e.g. `comment` in tree-sitter-c).
    let input = match text.find('\n') {
        Some(header_end) => text[header_end + 1..].trim_end(),
        None => "",
    };

    // Empty input is a normal state (the user just opened the buffer
    // and hasn't typed anything yet). Parsing `""` produces an error
    // tree for most start rules, which would surface as a misleading
    // "syntax error" diagnostic. Treat empty as "nothing to report".
    if input.is_empty() {
        clear_repl_diagnostics(&client, &repl_uri).await;
        update_tree_buffer(&client, &repl_uri, "").await;
        return;
    }

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        tracing::warn!("repl: set_language failed");
        return;
    }
    let Some(tree) = parser.parse(input, None) else {
        tracing::warn!("repl: parse returned None");
        return;
    };

    let tree_str = render_tree(format, input.as_bytes(), &tree);
    update_tree_buffer(&client, &repl_uri, &tree_str).await;

    // Header is line 0; user input starts at line 1. Tree-sitter row
    // numbers count from 0 within the input slice, so add 1 to align
    // with LSP line numbers in the full buffer. Clamp end points to
    // the buffer's actual line count so neovim's diagnostic handler
    // doesn't read past EOF (which throws "Index out of bounds").
    let total_lines = u32::try_from(text.lines().count().max(1)).unwrap_or(u32::MAX);
    let diagnostics = collect_error_diagnostics(&tree, input, /* line_offset = */ 1, total_lines);
    tracing::info!(
        "repl: parsed {} bytes, {} diagnostics for {repl_uri}",
        input.len(),
        diagnostics.len()
    );
    client
        .publish_diagnostics(repl_uri, diagnostics, None)
        .await;
}

/// Push a fresh parse tree into the side buffer. Writes to disk for
/// the initial-open case, then sends a `workspace/applyEdit` so any
/// already-open instance of the buffer in the client refreshes in
/// place - clients (notably neovim) don't auto-reload on disk change
/// unless `autoread` + focus events fire, which is unreliable for a
/// buffer that's been sitting in an unfocused split.
async fn update_tree_buffer(client: &tower_lsp::Client, repl_uri: &Url, tree_str: &str) {
    let Ok(repl_path) = repl_uri.to_file_path() else {
        return;
    };
    let tree_path = crate::repl::tree_path_for(&repl_path);
    let _ = std::fs::write(&tree_path, tree_str);
    let Ok(tree_uri) = Url::from_file_path(&tree_path) else {
        return;
    };
    let edits = std::collections::HashMap::from([(
        tree_uri,
        vec![tower_lsp::lsp_types::TextEdit {
            // Replace the entire buffer. The client clamps `u32::MAX`
            // to the actual end-of-buffer line.
            range: tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: 0, character: 0 },
                end: tower_lsp::lsp_types::Position { line: u32::MAX, character: 0 },
            },
            new_text: tree_str.to_string(),
        }],
    )]);
    let _ = client
        .apply_edit(tower_lsp::lsp_types::WorkspaceEdit {
            changes: Some(edits),
            document_changes: None,
            change_annotations: None,
        })
        .await;
}

async fn clear_repl_diagnostics(client: &tower_lsp::Client, repl_uri: &Url) {
    client
        .publish_diagnostics(repl_uri.clone(), Vec::new(), None)
        .await;
}

/// Render a parse tree for display, picking the format per the
/// session's `TreeFormat` setting. Sexp is the compact one-line-per-
/// node structural view; CST is the verbose `tree-sitter parse
/// --output-cst` format with row ranges + literal text.
fn render_tree(format: crate::repl::TreeFormat, source: &[u8], tree: &tree_sitter::Tree) -> String {
    match format {
        crate::repl::TreeFormat::Sexp => {
            tree_sitter::format_sexp(&tree.root_node().to_sexp(), 0)
        }
        crate::repl::TreeFormat::Cst => crate::cst::render(source, tree),
    }
}

/// Collect a flat list of LSP `Diagnostic`s for every ERROR / MISSING
/// node in the tree. `line_offset` is added to each diagnostic line so
/// the ranges land in the full REPL buffer's coordinates (accounting
/// for the header line). `total_lines` is the line count of the full
/// REPL buffer; we clamp every end position to it so the client's
/// diagnostic handler doesn't try to look past the buffer.
fn collect_error_diagnostics(
    tree: &tree_sitter::Tree,
    input: &str,
    line_offset: u32,
    total_lines: u32,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    let max_line = total_lines.saturating_sub(1);
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        // Emit a diagnostic only for the deepest error site - if any
        // descendant of this node also has an error / missing, the
        // child's diagnostic is more specific and pointing at this
        // node would double-report the same problem. For tree-sitter
        // `/` parsed as `comment` the tree is `(ERROR (MISSING "//"))`
        // - we want one diagnostic on the MISSING, not two.
        let descendant_has_error = any_descendant_has_error(node);
        if (node.is_error() || node.is_missing()) && !descendant_has_error {
            let r = node.range();
            let start_line = (r.start_point.row as u32 + line_offset).min(max_line);
            let end_line = (r.end_point.row as u32 + line_offset).min(max_line);
            let range = Range {
                start: Position {
                    line: start_line,
                    character: r.start_point.column as u32,
                },
                end: Position {
                    line: end_line,
                    character: r.end_point.column as u32,
                },
            };
            // Match tree-sitter core's own diagnostic vocabulary
            // (`MISSING <symbol>` / `UNEXPECTED '<char>'` in the sexp,
            // see lib/src/subtree.c). For a MISSING node the symbol
            // kind is what's expected; for an ERROR with no children
            // and non-empty byte range the bytes themselves are the
            // unexpected content; for an ERROR at EOF (no input
            // consumed) it's an unexpected end-of-input.
            let message = if node.is_missing() {
                format!("missing `{}`", node.kind())
            } else {
                let span = input
                    .get(node.start_byte()..node.end_byte())
                    .unwrap_or("");
                if span.is_empty() {
                    "unexpected end of input".to_string()
                } else {
                    format!("unexpected `{}`", span.escape_default())
                }
            };
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("ts_grammar_ls (repl)".into()),
                message,
                ..Default::default()
            });
        }
        // Only descend if there's an error somewhere below.
        if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
}

/// `true` if any proper descendant of `node` is an ERROR or MISSING.
/// `Node::has_error()` includes the node itself, so we walk the
/// children directly to check "is there an error STRICTLY below?".
fn any_descendant_has_error(node: tree_sitter::Node<'_>) -> bool {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return false;
    }
    loop {
        let child = cursor.node();
        if child.is_error() || child.is_missing() || child.has_error() {
            return true;
        }
        if !cursor.goto_next_sibling() {
            return false;
        }
    }
}

