//! REPL workspace commands and per-buffer state plumbing.
//!
//! The REPL is a scratch input buffer bound to one (grammar, rule) pair.
//! The buffer's contents are pure parse input; configuration (which
//! grammar, which rule, output format) lives in the sibling
//! `<basename>.tsg-repl.json` metadata file and in the in-memory
//! `ReplSession`. The rule is surfaced to the user via a `CodeLens` on
//! line 0 (see `handlers::code_lens`) and changed via the
//! `tsg.setReplRule` command, which prompts through
//! `window/showMessageRequest`.
//!
//! Commands:
//! - `tsg.openRepl { uri, position? }` - creates (or reuses) the input +
//!   tree buffers + metadata for a grammar URI, then asks the client to
//!   open the buffers via `window/showDocument`. If `position` lands
//!   inside a `rule X { ... }` declaration, `X` is the start rule;
//!   otherwise the grammar's first rule.
//! - `tsg.setReplRule { uri }` - changes the start rule for the REPL
//!   bound to `uri`. Picks the new rule via `window/showMessageRequest`
//!   populated from the grammar's analysis.
//!
//! ## Process model
//!
//! `tsg.openRepl` always runs in the LSP process serving the grammar
//! buffer (call it process A): the action's code-action entry lives on
//! the grammar URI, which only that process owns. The REPL input +
//! tree buffers live under `$XDG_CACHE_HOME/ts_grammar_ls/repl/`,
//! whose `root_dir` is almost always different from the grammar's, so
//! lspconfig (and similar client frameworks) spawn a separate process
//! (call it process B) to serve them.
//!
//! That means every `did_open` / `did_change` / `codeLens` /
//! `tsg.setReplRule` on the REPL URI lands in process B, never A. So
//! all per-buffer state - the `ReplSession`, the in-flight compile,
//! the cached `Language` - lives in B's `Backend`. Process A only
//! writes the on-disk `ReplMeta` and the empty buffer files, then asks
//! the client to open them; it intentionally does NOT register a
//! session or kick a compile (doing so would race B's own compile on
//! the shared cache dir).
//!
//! Single-process configs (A == B) still work: B's `did_open` handler
//! reads the metadata file just like any other process would.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use rustc_hash::FxHasher;
use serde::Deserialize;
use tower_lsp::lsp_types::{
    ExecuteCommandParams, MessageActionItem, MessageType, Position, ShowDocumentParams, Url,
};

use crate::document::{DefKind, Module};
use crate::repl::{ReplMeta, ReplSession};
use crate::server::Backend;

pub const OPEN_REPL_COMMAND: &str = "tsg.openRepl";
pub const SET_REPL_RULE_COMMAND: &str = "tsg.setReplRule";
pub const TOGGLE_REPL_FORMAT_COMMAND: &str = "tsg.toggleReplFormat";

/// Top-level dispatch for `workspace/executeCommand`. Routes to the
/// per-command handler based on `params.command`; unknown commands
/// return `None`, which tower-lsp surfaces as an empty result.
pub async fn execute_command(
    backend: &Backend,
    params: &ExecuteCommandParams,
) -> Option<serde_json::Value> {
    match params.command.as_str() {
        OPEN_REPL_COMMAND => open_repl(backend, params),
        SET_REPL_RULE_COMMAND => set_repl_rule(backend, params).await,
        TOGGLE_REPL_FORMAT_COMMAND => toggle_repl_format(backend, params).await,
        _ => None,
    }
}

#[derive(Deserialize, Debug)]
struct OpenReplArgs {
    uri: Url,
    #[serde(default)]
    position: Option<Position>,
}

/// Handle `workspace/executeCommand` for `tsg.openRepl`. Writes the
/// empty input + tree buffers, persists the `(grammar, rule, format)`
/// binding to the sibling metadata file, then asks the client to open
/// both buffers via `window/showDocument`.
///
/// Intentionally does NOT register a `ReplSession` or kick a compile:
/// this handler runs in the grammar's LSP process, which never serves
/// the REPL buffer's events. The process that DOES serve them (B) will
/// build its session from the metadata file on `did_open`. Doing
/// either here would just produce a write-only session and a compile
/// that races B's compile on the same on-disk cache dir.
fn open_repl(backend: &Backend, params: &ExecuteCommandParams) -> Option<serde_json::Value> {
    let args = params.arguments.first()?;
    let args: OpenReplArgs = serde_json::from_value(args.clone()).ok()?;

    let rule_name = resolve_default_rule(backend, &args)?;
    let input_uri = repl_input_uri(&args.uri);
    let tree_uri = input_uri.tree_uri();
    ensure_input_buffer(&input_uri.input_path());

    let meta = ReplMeta {
        grammar_uri: args.uri,
        current_rule: rule_name.clone(),
        format: crate::repl::TreeFormat::default(),
    };
    meta.write_for(&input_uri);

    // Stand up the tree side buffer (empty until the first parse
    // lands). Doing it up front means the editor finds a real file
    // when we ask it to open one, rather than racing the first compile.
    let _ = std::fs::write(tree_uri.tree_path(), "");

    // Fire `window/showDocument` as detached tasks for both buffers.
    // The request awaits a client response, and we don't want our
    // `executeCommand` reply gated on that round trip. Open the tree
    // buffer first (so it's the unfocused split) then the input (so
    // it ends up focused).
    let client = backend.client.clone();
    let input_url = input_uri.as_url().clone();
    let tree_url = tree_uri.as_url().clone();
    tokio::spawn(async move {
        let _ = client
            .show_document(ShowDocumentParams {
                uri: tree_url,
                external: Some(false),
                take_focus: Some(false),
                selection: None,
            })
            .await;
        let _ = client
            .show_document(ShowDocumentParams {
                uri: input_url,
                external: Some(false),
                take_focus: Some(true),
                selection: None,
            })
            .await;
    });

    Some(serde_json::json!({
        "uri": input_uri.as_url().to_string(),
        "tree_uri": tree_uri.as_url().to_string(),
        "rule": rule_name,
    }))
}

/// A hidden rule (leading `_`) can never be a start rule: codegen rejects it
/// with `InternSymbolsError::HiddenStartRule`, so the REPL can't compile one.
fn is_hidden_rule(name: &str) -> bool {
    name.starts_with('_')
}

/// First rule in `module` that could legally serve as a start rule.
fn first_start_candidate(module: &Module) -> Option<String> {
    module
        .definitions
        .as_ref()?
        .iter()
        .find(|d| {
            matches!(d.kind, DefKind::Rule | DefKind::OverrideRule) && !is_hidden_rule(&d.name)
        })
        .map(|d| d.name.clone())
}

/// Cursor-aware rule pick: if `position` falls inside a `rule X { ... }`
/// declaration, return `X`. Otherwise fall back to the grammar's implicit
/// start rule.
///
/// The fallback deliberately walks to the root of the inherit chain rather
/// than using this file's first declaration. `InputGrammar::normalize` treats
/// `variables[0]` as the implicit start, and variables are ordered base-first,
/// so for an inheriting grammar the start lives in the *base*. tree-sitter-cpp
/// is the motivating case: it never declares `translation_unit` (it inherits
/// it from C), and its own first declaration is `override rule _top_level_item`
/// - hidden, so codegen refuses it and the REPL used to fail to compile with
/// nothing but a `warn!` to show for it.
///
/// Hidden rules are skipped in both paths for the same reason: picking one
/// can only ever produce a compile failure.
fn resolve_default_rule(backend: &Backend, args: &OpenReplArgs) -> Option<String> {
    let analysis = backend.get_analysis(&args.uri)?;

    if let Some(pos) = args.position
        && let Some(offset) = crate::text::position_to_offset(&analysis.rope, pos)
        && let Some(def) = analysis.definitions.as_ref().and_then(|defs| {
            defs.iter().find(|d| {
                matches!(d.kind, DefKind::Rule | DefKind::OverrideRule)
                    && d.full_span.start <= offset
                    && offset < d.full_span.end
            })
        })
        && !is_hidden_rule(&def.name)
    {
        return Some(def.name.clone());
    }

    // Root of the inherit chain first (that's where `variables[0]` comes
    // from), then this file as a fallback for non-inheriting grammars.
    let mut root = &*analysis;
    while let Some(base) = root.base_module.as_deref() {
        root = base;
    }
    first_start_candidate(root).or_else(|| first_start_candidate(&analysis))
}

/// Stable input-buffer URI for a given grammar URI. Hashing the
/// grammar URI keeps the path deterministic across LSP restarts, so
/// reopening the REPL for the same grammar reuses the same buffer.
fn repl_input_uri(grammar_uri: &Url) -> crate::repl::ReplInputUri {
    let mut hasher = FxHasher::default();
    grammar_uri.as_str().hash(&mut hasher);
    let path = repl_dir().join(format!(
        "{:016x}{}",
        hasher.finish(),
        crate::repl::REPL_INPUT_SUFFIX
    ));
    let url = Url::from_file_path(&path).expect("repl_dir is absolute");
    crate::repl::ReplInputUri::try_from_uri(&url).expect("path ends with REPL_INPUT_SUFFIX")
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

/// Ensure a `ReplSession` exists in `backend.repl_sessions` for
/// `repl_uri`, reconstructing it from the sibling metadata file if
/// this process hasn't touched the URI yet (the typical first-event
/// path - whoever ran `tsg.openRepl` was a different process, so our
/// session map is empty).
///
/// Returns `true` if a session is present after the call. `false`
/// means there's no metadata file either (e.g., the user manually
/// opened a `*.tsg-repl.tsg` file the LSP never wrote), so callers
/// should bail.
fn ensure_session(backend: &Backend, input_uri: &crate::repl::ReplInputUri) -> bool {
    if backend.repl_sessions.contains_key(input_uri) {
        return true;
    }
    let Some(meta) = ReplMeta::read_for(input_uri) else {
        tracing::info!(
            "repl: no sibling metadata for {}; user opened a REPL buffer \
             without going through tsg.openRepl",
            input_uri.as_url(),
        );
        return false;
    };
    tracing::info!(
        "repl: rebuilding session for {} (grammar={}, rule={})",
        input_uri.as_url(),
        meta.grammar_uri,
        meta.current_rule,
    );
    backend.repl_sessions.insert(
        input_uri.clone(),
        std::sync::Mutex::new(ReplSession {
            grammar_uri: meta.grammar_uri,
            current_rule: meta.current_rule,
            last_key: None,
            language: None,
            format: meta.format,
            current_text: None,
            last_tree_text: String::new(),
            last_tree_spans: Vec::new(),
        }),
    );
    true
}

/// Called from `did_change`/`did_open` when the touched URI is a REPL
/// input buffer. Caches the live buffer text on the session and either
/// re-parses with the cached `Language` (fast path) or kicks a compile
/// if no language is loaded yet.
///
/// Rule binding never changes here - it's owned by the session +
/// metadata file and only mutated via `tsg.setReplRule`. The buffer
/// text is pure parse input.
pub fn handle_repl_change(backend: &Backend, input_uri: &crate::repl::ReplInputUri, text: &str) {
    if !ensure_session(backend, input_uri) {
        return;
    }

    let session_ref = backend.repl_sessions.get(input_uri).expect("ensured above");
    // Cache the live buffer text and grab the current language under
    // the lock; drop the DashMap ref before spawning so we don't hold
    // a guard across an await.
    let cached = {
        let mut session = session_ref.lock().unwrap();
        // Stash the live buffer text so the async compile-completion
        // path can re-parse against the current state without going
        // through disk (neovim doesn't flush to disk until `:w`).
        session.current_text = Some(text.to_owned());
        session.language.clone().map(|lang| (lang, session.format))
    };
    drop(session_ref);

    if let Some((language, format)) = cached {
        let client = backend.client.clone();
        let sessions = std::sync::Arc::clone(&backend.repl_sessions);
        let input_uri = input_uri.clone();
        let text = text.to_owned();
        tokio::spawn(async move {
            parse_and_publish(sessions, client, input_uri, text, language, format).await;
        });
        return;
    }

    tracing::info!(
        "repl: {} no language cached; spawning compile",
        input_uri.as_url()
    );
    spawn_compile(backend, input_uri.clone());
}

/// Handle `workspace/executeCommand` for `tsg.setReplRule`. Picks a new
/// rule via `window/showMessageRequest` populated from the grammar's
/// analysis, then updates the session + metadata file, kicks a
/// recompile, and asks the client to refresh code lenses so the
/// displayed rule name updates.
async fn set_repl_rule(
    backend: &Backend,
    params: &ExecuteCommandParams,
) -> Option<serde_json::Value> {
    #[derive(Deserialize)]
    struct Args {
        uri: Url,
    }
    let args = params.arguments.first()?;
    let Args { uri } = serde_json::from_value(args.clone()).ok()?;
    let input_uri = crate::repl::ReplInputUri::try_from_uri(&uri)?;

    // Make sure the session exists locally - if the user opened the
    // REPL and immediately clicked the lens without typing, no
    // did_change has fired yet and our map is empty. Without this,
    // the post-pick spawn_compile would be a no-op (it bails on
    // missing session) and the new rule wouldn't take effect until
    // the next keystroke.
    if !ensure_session(backend, &input_uri) {
        return None;
    }
    let (grammar_uri, current_rule) = {
        let s = backend.repl_sessions.get(&input_uri)?;
        let g = s.lock().unwrap();
        (g.grammar_uri.clone(), g.current_rule.clone())
    };

    // List the grammar's rules. We need the live analysis (which may
    // include rules contributed by inherits/imports), not just whatever
    // is in the buffer.
    let analysis = backend.analysis_for_uri(&grammar_uri)?;
    let defs = analysis.definitions.as_ref()?;
    let rules: Vec<String> = defs
        .iter()
        .filter(|d| matches!(d.kind, DefKind::Rule | DefKind::OverrideRule))
        .map(|d| d.name.clone())
        .collect();
    if rules.is_empty() {
        return None;
    }

    let actions: Vec<MessageActionItem> = rules
        .into_iter()
        .map(|r| MessageActionItem {
            title: r,
            properties: Default::default(),
        })
        .collect();
    let prompt = format!("REPL rule (current: {current_rule})");
    let picked = backend
        .client
        .show_message_request(MessageType::INFO, prompt, Some(actions))
        .await
        .ok()
        .flatten()?;
    let new_rule = picked.title;
    if new_rule == current_rule {
        return None;
    }

    // Update session under lock; drop guard before any awaits.
    {
        let s = backend.repl_sessions.get(&input_uri)?;
        let mut g = s.lock().unwrap();
        g.current_rule = new_rule.clone();
        g.language = None;
        g.last_key = None;
    }

    // Persist to disk so a future LSP process / restart picks up the
    // new rule.
    if let Some(mut meta) = ReplMeta::read_for(&input_uri) {
        meta.current_rule = new_rule.clone();
        meta.write_for(&input_uri);
    }

    spawn_compile(backend, input_uri);
    // Best-effort: ask the client to refresh code lenses so the title
    // ("Rule: <name>") updates immediately. Clients that don't support
    // refresh just leave the stale lens until the next natural refresh.
    let _ = backend.client.code_lens_refresh().await;

    Some(serde_json::json!({ "rule": new_rule }))
}

/// Handle `workspace/executeCommand` for `tsg.toggleReplFormat`. Flips
/// the session's `format` between `Cst` and `Sexp`, persists to the
/// metadata file, re-renders the tree using the cached `Language` (no
/// recompile - format choice doesn't affect parsing), and asks the
/// client to refresh the code lenses so the `Tree:` lens title
/// updates.
///
/// Accepts the REPL input URI in `arguments[0].uri`.
async fn toggle_repl_format(
    backend: &Backend,
    params: &ExecuteCommandParams,
) -> Option<serde_json::Value> {
    #[derive(Deserialize)]
    struct Args {
        uri: Url,
    }
    let args = params.arguments.first()?;
    let Args { uri } = serde_json::from_value(args.clone()).ok()?;
    let input_uri = crate::repl::ReplInputUri::try_from_uri(&uri)?;

    if !ensure_session(backend, &input_uri) {
        return None;
    }

    // Flip the format and grab a snapshot of the bits we need for the
    // re-render outside the lock.
    let (new_format, language, current_text) = {
        let s = backend.repl_sessions.get(&input_uri)?;
        let mut g = s.lock().unwrap();
        g.format = match g.format {
            crate::repl::TreeFormat::Cst => crate::repl::TreeFormat::Sexp,
            crate::repl::TreeFormat::Sexp => crate::repl::TreeFormat::Cst,
        };
        (g.format, g.language.clone(), g.current_text.clone())
    };

    // Persist so the choice survives an LSP restart.
    if let Some(mut meta) = ReplMeta::read_for(&input_uri) {
        meta.format = new_format;
        meta.write_for(&input_uri);
    }

    // Re-render whatever the user has parsed so far in the new format.
    // If no language/text yet (REPL just opened, nothing typed), the
    // next did_change will pick up the new format naturally.
    if let (Some(lang), Some(text)) = (language, current_text) {
        let sessions = std::sync::Arc::clone(&backend.repl_sessions);
        let client = backend.client.clone();
        let uri = input_uri.clone();
        tokio::spawn(async move {
            parse_and_publish(sessions, client, uri, text, lang, new_format).await;
        });
    }

    let _ = backend.client.code_lens_refresh().await;
    Some(serde_json::json!({
        "format": match new_format {
            crate::repl::TreeFormat::Cst => "cst",
            crate::repl::TreeFormat::Sexp => "sexp",
        }
    }))
}

/// Asynchronously prepare + compile a parser for the current state of
/// the session at `repl_uri`. On success, swaps the loaded `Language`
/// into the session and re-parses the cached live text.
///
/// Callers must run `ensure_session` first - we early-return silently
/// if the session is missing rather than reconstruct it here, since
/// reconstruction is a sync I/O step and `spawn_compile` is only
/// supposed to dispatch async work.
fn spawn_compile(backend: &Backend, input_uri: crate::repl::ReplInputUri) {
    let cache = std::sync::Arc::clone(&backend.repl_cache);
    let sessions = std::sync::Arc::clone(&backend.repl_sessions);
    let grammar_uri;
    let rule_name;
    {
        let Some(session_ref) = sessions.get(&input_uri) else {
            return;
        };
        let session = session_ref.lock().unwrap();
        grammar_uri = session.grammar_uri.clone();
        rule_name = session.current_rule.clone();
    }
    // Re-run `analyze` against the current grammar text. The grammar
    // buffer is normally owned by a different LSP process (see the
    // module docstring), so the on-disk read is the usual path; the
    // `document_map` hit only fires in single-process configs where
    // the same instance happens to serve both buffers.
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
    let prepared = match crate::repl::ReplCache::prepare(grammar, &rule_name) {
        Ok(p) => p,
        Err(crate::repl::ReplCompileError::RuleNotFound(name)) => {
            // Common transient state while the user is typing the rule
            // name (e.g. "i", "id", "ide" before settling on
            // "identifier"). Logging this at warn level produces noisy
            // bursts; demote to debug.
            tracing::debug!(rule = %name, "repl: rule not in grammar");
            return;
        }
        Err(e) => {
            tracing::warn!("repl: prepare failed: {e}");
            return;
        }
    };
    let (json, key) = prepared;

    // A grammar with `externals:` backed by a custom scanner generates a
    // `parser.c` that references `tree_sitter_<lang>_external_scanner_*`.
    // Those symbols live in the grammar's own `src/scanner.c`, which the
    // REPL's generated build dir doesn't otherwise contain.
    let scanner = grammar_uri
        .to_file_path()
        .ok()
        .and_then(|p| crate::repl::find_scanner(&p));

    // Compare against last_key. If the session was just recompiled for
    // the same key (e.g. concurrent did_change events arrived), skip.
    {
        let Some(session_ref) = sessions.get(&input_uri) else {
            return;
        };
        let session = session_ref.lock().unwrap();
        if session.last_key.as_ref() == Some(&key) && session.language.is_some() {
            return;
        }
    }

    let client = backend.client.clone();
    tokio::spawn(async move {
        tracing::info!("repl: compile start key={key}");
        match cache.get_or_compile(key.clone(), json, scanner).await {
            Ok(language) => {
                tracing::info!("repl: compile done key={key}");
                // Stash the freshly compiled language on the session
                // and grab the live buffer text + format under one
                // lock (the text comes from did_change, not disk -
                // neovim doesn't flush until `:w`).
                let (text, format) = {
                    let Some(session_ref) = sessions.get(&input_uri) else {
                        return;
                    };
                    let mut session = session_ref.lock().unwrap();
                    session.language = Some(std::sync::Arc::clone(&language));
                    session.last_key = Some(key);
                    (session.current_text.clone(), session.format)
                };
                // Re-parse the input region with the fresh language so
                // the tree buffer + diagnostics catch up to the rule
                // change (the user may have been waiting seconds for
                // this compile and now expects to see results).
                if let Some(text) = text {
                    parse_and_publish(sessions, client, input_uri, text, language, format).await;
                }
            }
            Err(e) => {
                tracing::warn!("repl: compile failed: {e}");
                // Surface it on the input buffer too. A compile failure is
                // otherwise completely silent: the REPL just sits there with
                // a stale (or empty) tree and no indication why, which is
                // exactly what made the hidden-start-rule case so hard to
                // diagnose.
                publish_repl_error(
                    &client,
                    &input_uri,
                    &format!("REPL failed to build a parser for rule `{rule_name}`: {e}"),
                )
                .await;
            }
        }
    });
}

/// Stand up an empty input buffer at `path`. The REPL is intended to
/// be ephemeral: prior session content shouldn't bleed across
/// invocations, since the user's mental model is "this is a scratch
/// buffer that opens fresh each time".
fn ensure_input_buffer(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, "");
}

/// Parse the buffer text with the session's cached `Language`. Renders
/// the parse tree to the sibling tree file and publishes LSP
/// diagnostics for any ERROR / MISSING nodes.
///
/// Triggered from two places:
///   - `handle_repl_change` after the bookkeeping, when a language is
///     already on the session.
///   - `spawn_compile`'s completion path, so the tree updates as soon
///     as a fresh compile lands (the user changed the rule, waited a
///     few seconds, now wants to see the parse).
async fn parse_and_publish(
    backend_sessions: std::sync::Arc<
        dashmap::DashMap<crate::repl::ReplInputUri, std::sync::Mutex<ReplSession>>,
    >,
    client: tower_lsp::Client,
    input_uri: crate::repl::ReplInputUri,
    text: String,
    language: std::sync::Arc<tree_sitter::Language>,
    format: crate::repl::TreeFormat,
) {
    // Trim trailing whitespace so the user's editor-supplied final
    // newline doesn't show up as an unparsed-tail ERROR for rules whose
    // pattern stops at `\n` (e.g. `comment` in tree-sitter-c).
    let input = text.trim_end();

    // Empty input is a normal state (the user just opened the buffer
    // and hasn't typed anything yet). Parsing `""` produces an error
    // tree for most start rules, which would surface as a misleading
    // "syntax error" diagnostic. Treat empty as "nothing to report".
    if input.is_empty() {
        stash_tree_render(&backend_sessions, &input_uri, String::new(), Vec::new());
        clear_repl_diagnostics(&client, &input_uri).await;
        update_tree_buffer(&client, &input_uri, "").await;
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

    let (tree_str, tree_spans) = render_tree(format, input.as_bytes(), &tree);
    stash_tree_render(&backend_sessions, &input_uri, tree_str.clone(), tree_spans);
    update_tree_buffer(&client, &input_uri, &tree_str).await;
    // The stashed spans drive `textDocument/semanticTokens/full` for
    // the tree buffer. Most clients refetch on buffer-change events
    // (driven by the applyEdit above), but a server-side refresh
    // request is the explicit signal and works across clients.
    let _ = client.semantic_tokens_refresh().await;

    // Clamp end positions to the buffer's actual line count so neovim's
    // diagnostic handler doesn't read past EOF.
    let total_lines = u32::try_from(text.lines().count().max(1)).unwrap_or(u32::MAX);
    let diagnostics = collect_error_diagnostics(&tree, input, total_lines);
    tracing::info!(
        "repl: parsed {} bytes, {} diagnostics for {}",
        input.len(),
        diagnostics.len(),
        input_uri.as_url(),
    );
    client
        .publish_diagnostics(input_uri.as_url().clone(), diagnostics, None)
        .await;
}

/// Push a fresh parse tree into the side buffer via `workspace/applyEdit`.
/// The on-disk file is only stamped once at `tsg.openRepl` time (so the
/// editor has something to open); after that, the buffer's contents
/// are exclusively server-driven. Skipping the per-keystroke disk
/// write lets the user set the buffer `nomodifiable` in their editor
/// without us racing the file perms, and avoids cache-dir thrash.
async fn update_tree_buffer(
    client: &tower_lsp::Client,
    input_uri: &crate::repl::ReplInputUri,
    tree_str: &str,
) {
    let edits = std::collections::HashMap::from([(
        input_uri.tree_uri().as_url().clone(),
        vec![tower_lsp::lsp_types::TextEdit {
            // Replace the entire buffer. The client clamps `u32::MAX`
            // to the actual end-of-buffer line.
            range: tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: tower_lsp::lsp_types::Position {
                    line: u32::MAX,
                    character: 0,
                },
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

/// Report a REPL-level failure (compile, codegen) on the input buffer itself.
/// Anchored at the top of the buffer since the fault is in the grammar/rule
/// selection, not in anything the user typed here.
async fn publish_repl_error(
    client: &tower_lsp::Client,
    input_uri: &crate::repl::ReplInputUri,
    message: &str,
) {
    let zero = tower_lsp::lsp_types::Position::new(0, 0);
    client
        .publish_diagnostics(
            input_uri.as_url().clone(),
            vec![tower_lsp::lsp_types::Diagnostic {
                range: tower_lsp::lsp_types::Range::new(zero, zero),
                severity: Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
                source: Some("ts_grammar_ls".into()),
                message: message.to_owned(),
                ..Default::default()
            }],
            None,
        )
        .await;
}

async fn clear_repl_diagnostics(client: &tower_lsp::Client, input_uri: &crate::repl::ReplInputUri) {
    client
        .publish_diagnostics(input_uri.as_url().clone(), Vec::new(), None)
        .await;
}

/// Persist the latest rendered tree text + spans on the session so the
/// semantic-tokens handler (which serves the tree URI, a different
/// buffer) can return styling without re-rendering.
fn stash_tree_render(
    sessions: &dashmap::DashMap<crate::repl::ReplInputUri, std::sync::Mutex<ReplSession>>,
    input_uri: &crate::repl::ReplInputUri,
    text: String,
    spans: Vec<crate::cst::CstSpan>,
) {
    if let Some(s) = sessions.get(input_uri) {
        let mut g = s.lock().unwrap();
        g.last_tree_text = text;
        g.last_tree_spans = spans;
    }
}

/// Render a parse tree for display, picking the format per the
/// session's `TreeFormat` setting. Sexp is the compact one-line-per-
/// node structural view; CST is the verbose `tree-sitter parse
/// --output-cst` format with row ranges + literal text.
///
/// Returns the rendered text plus, for CST output, the colored span
/// list so the LSP semantic-tokens handler can turn them into client
/// highlighting. Sexp output gets no spans (we just format the bare
/// parenthesized form).
fn render_tree(
    format: crate::repl::TreeFormat,
    source: &[u8],
    tree: &tree_sitter::Tree,
) -> (String, Vec<crate::cst::CstSpan>) {
    match format {
        crate::repl::TreeFormat::Sexp => (
            tree_sitter::format_sexp(&tree.root_node().to_sexp(), 0),
            Vec::new(),
        ),
        crate::repl::TreeFormat::Cst => {
            let r = crate::cst::render(source, tree);
            (r.text, r.spans)
        }
    }
}

/// Collect a flat list of LSP `Diagnostic`s for every ERROR / MISSING
/// node in the tree. `total_lines` is the line count of the buffer;
/// we clamp every end position to it so the client's diagnostic
/// handler doesn't try to look past the buffer.
fn collect_error_diagnostics(
    tree: &tree_sitter::Tree,
    input: &str,
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
            let start_line = (r.start_point.row as u32).min(max_line);
            let end_line = (r.end_point.row as u32).min(max_line);
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
                let span = input.get(node.start_byte()..node.end_byte()).unwrap_or("");
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
