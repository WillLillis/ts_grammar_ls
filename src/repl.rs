//! REPL compile + load pipeline.
//!
//! Given a parsed grammar and a chosen "start rule", produces a compiled
//! `tree_sitter::Language` that parses input as if `rule_name` were the
//! grammar's start. The transform is:
//!
//! 1. Clone the grammar.
//! 2. Swap the target rule into `variables[0]` (`InputGrammar::normalize`
//!    treats `variables.first()` as the implicit start).
//! 3. Re-run `normalize()` so the resulting grammar is well-formed and
//!    pruned to just the rules reachable from the chosen start + the
//!    usual roots (extras, externals, `word_token`).
//! 4. Serialize to JSON, hash → cache key.
//! 5. If cache miss: re-exec ourselves with `generate-check --write-to <dir>`
//!    to produce `<dir>/src/{parser.c, grammar.json, tree_sitter/*}`.
//! 6. Hand `<dir>/src/` to `tree_sitter_loader::Loader` to compile +
//!    `dlopen` into a `Language`. Cache the result.

use std::{
    collections::HashMap,
    ffi::OsStr,
    hash::{DefaultHasher, Hash, Hasher},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tower_lsp::lsp_types;
use tree_sitter::Language;
use tree_sitter_generate::nativedsl::{self, serialize::grammar_to_json};
use tree_sitter_loader::{CompileConfig, Loader};

use crate::generate_check::GenerateToDirError;

pub const REPL_INPUT_SUFFIX: &str = ".tsg-repl.tsg";
pub const REPL_META_SUFFIX: &str = ".tsg-repl.json";
pub const REPL_TREE_SUFFIX: &str = ".tsg-repl-tree.tsg";

/// A URI pointing to a REPL input buffer (`<basename>.tsg-repl.tsg`).
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ReplInputUri(lsp_types::Url);

impl ReplInputUri {
    #[must_use]
    pub fn try_from_uri(uri: &lsp_types::Url) -> Option<Self> {
        uri.as_str()
            .ends_with(REPL_INPUT_SUFFIX)
            .then(|| Self(uri.clone()))
    }

    #[must_use]
    pub const fn as_url(&self) -> &lsp_types::Url {
        &self.0
    }

    #[must_use]
    #[expect(clippy::missing_panics_doc)]
    pub fn input_path(&self) -> PathBuf {
        self.0.to_file_path().unwrap()
    }

    #[must_use]
    #[expect(clippy::missing_panics_doc)]
    pub fn tree_uri(&self) -> ReplTreeUri {
        let stem = self.0.as_str().strip_suffix(REPL_INPUT_SUFFIX).unwrap();
        let url = lsp_types::Url::parse(&format!("{stem}{REPL_TREE_SUFFIX}")).unwrap();
        ReplTreeUri(url)
    }

    #[must_use]
    pub fn tree_path(&self) -> PathBuf {
        sibling_with_suffix(&self.input_path(), REPL_TREE_SUFFIX)
    }

    #[must_use]
    pub fn meta_path(&self) -> PathBuf {
        sibling_with_suffix(&self.input_path(), REPL_META_SUFFIX)
    }
}

/// A URI confirmed to point at a REPL tree-side buffer
/// (`<basename>.tsg-repl-tree.tsg`).
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ReplTreeUri(lsp_types::Url);

impl ReplTreeUri {
    #[must_use]
    pub fn try_from_uri(uri: &lsp_types::Url) -> Option<Self> {
        uri.as_str()
            .ends_with(REPL_TREE_SUFFIX)
            .then(|| Self(uri.clone()))
    }

    #[must_use]
    pub const fn as_url(&self) -> &lsp_types::Url {
        &self.0
    }

    #[must_use]
    #[expect(clippy::missing_panics_doc)]
    pub fn tree_path(&self) -> PathBuf {
        self.0.to_file_path().unwrap()
    }

    #[must_use]
    #[expect(clippy::missing_panics_doc)]
    pub fn input_uri(&self) -> ReplInputUri {
        let stem = self.0.as_str().strip_suffix(REPL_TREE_SUFFIX).unwrap();
        let url = lsp_types::Url::parse(&format!("{stem}{REPL_INPUT_SUFFIX}")).unwrap();
        ReplInputUri(url)
    }
}

/// Replace `<input_path>`'s trailing [`REPL_INPUT_SUFFIX`] with
/// `new_suffix`. Pre-validation by [`ReplInputUri::try_from_uri`] is
/// what keeps the `unwrap()`s safe.
fn sibling_with_suffix(input_path: &Path, new_suffix: &str) -> PathBuf {
    let name = input_path.file_name().unwrap();
    let stem = OsStr::from_bytes(
        name.as_bytes()
            .strip_suffix(REPL_INPUT_SUFFIX.as_bytes())
            .unwrap(),
    );
    input_path.with_file_name(stem).with_extension(new_suffix)
}

/// Output format for the tree side buffer. Persisted in the metadata
/// file so the user's preference survives across LSP restarts.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TreeFormat {
    Sexp,
    #[default]
    Cst,
}

/// Server-owned metadata persisted next to each REPL input buffer.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ReplMeta {
    pub grammar_uri: lsp_types::Url,
    pub current_rule: String,
    pub format: TreeFormat,
}

impl ReplMeta {
    /// Write `self` to the metadata file paired with `input_uri`.
    /// Best-effort: failures are logged and swallowed. The file is
    /// server-internal state used to rebuild a session on a future
    /// `did_open`; a failed write degrades to the "no session, rebuild
    /// from scratch" path the LSP already handles.
    pub fn write_for(&self, input_uri: &ReplInputUri) {
        let path = input_uri.meta_path();
        let json = match serde_json::to_string(self) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(?path, error = %e, "repl: failed to serialize meta");
                return;
            }
        };
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!(?path, error = %e, "repl: failed to write meta");
        }
    }

    /// Read the metadata file paired with `input_uri`. Returns `None`
    /// if the file is absent or unparseable; both are logged.
    #[must_use]
    pub fn read_for(input_uri: &ReplInputUri) -> Option<Self> {
        let path = input_uri.meta_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(?path, error = %e, "repl: failed to read meta");
                }
                return None;
            }
        };
        match serde_json::from_str(&raw) {
            Ok(meta) => Some(meta),
            Err(e) => {
                tracing::warn!(?path, error = %e, "repl: failed to parse meta");
                None
            }
        }
    }
}

#[must_use]
pub fn default_cache_root() -> PathBuf {
    use etcetera::BaseStrategy as _;
    let base = etcetera::choose_base_strategy()
        .ok()
        .map_or_else(std::env::temp_dir, |s| s.cache_dir());
    base.join("ts_grammar_ls").join("repl")
}

/// Per-REPL-buffer state. Tracks which grammar/rule the REPL is bound
/// to and the most recently compiled `Language` so per-keystroke parses
/// don't need to re-compile.
pub struct ReplSession {
    /// The grammar this REPL parses against.
    pub grammar_uri: lsp_types::Url,
    /// Current start rule. Owned by the server: only `tsg.openRepl` and
    /// `tsg.setReplRule` change it. Mirrored to `ReplMeta` on disk.
    pub current_rule: String,
    /// Cache key for the most recently compiled language. `None` if no
    /// compile has completed yet. Compared against the next attempt's
    /// key to detect "needs recompile" without re-running the loader.
    pub last_key: Option<CacheKey>,
    /// Most recent loaded language. Used to parse REPL input. `None`
    /// until the first compile completes.
    pub language: Option<Arc<Language>>,
    /// User-selected output format for the tree side buffer.
    pub format: TreeFormat,
    /// Live buffer contents as of the most recent `did_change`. The
    /// disk file is stale until the user saves; we hold the live text
    /// here so the async compile-completion path can re-parse against
    /// the current state without going through disk.
    pub current_text: Option<String>,
    /// Most recently rendered tree-buffer text. Stashed here (vs. read
    /// off disk in the semantic-tokens handler) because the tree file
    /// is the editor's view,  driven by `applyEdit`.
    pub last_tree_text: String,
    /// Colored byte ranges from the most recent CST render, in source
    /// order. Indices into `last_tree_text`. Empty when the current
    /// `TreeFormat` is sexp (sexp output isn't colored).
    pub last_tree_spans: Vec<crate::cst::CstSpan>,
}

/// Stable on-disk + in-memory identifier for one compiled REPL parser.
/// Hex-encoded hash of the post-swap, post-normalize grammar JSON.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey(u64);

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Failure modes for `prepare` / `get_or_compile`. Callers branch on
/// `RuleNotFound` (transient typing state, logged at debug); the rest
/// are logged at warn and otherwise lumped together.
#[derive(Debug, Error)]
pub enum ReplCompileError {
    /// The chosen start rule isn't declared in the grammar's
    /// `variables`. Common transient state while the user types a
    /// rule name.
    #[error("rule `{0}` not found in grammar")]
    RuleNotFound(String),

    /// Failed to prepare the on-disk cache directory for this key.
    #[error("creating cache dir {path}: {source}")]
    CacheDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// I/O around the codegen subprocess: locating our own binary,
    /// spawning it, piping stdin, awaiting it. Distinct from
    /// [`Self::Generate`] - this means we couldn't run the pipeline;
    /// that one means the pipeline ran and reported errors.
    #[error("codegen subprocess: {0}")]
    Subprocess(#[source] std::io::Error),

    /// The codegen subprocess exited non-zero. The structured error
    /// crosses the process boundary as JSON on the subprocess's
    /// stdout; we deserialize it back into the original variant
    /// (so callers can branch on which generate-stage failed if they
    /// care to).
    #[error("tree-sitter generate failed: {0}")]
    Generate(#[from] GenerateToDirError),

    /// The codegen subprocess exited non-zero but its stdout wasn't
    /// parseable as a [`GenerateToDirError`] - shouldn't happen in
    /// practice (our subprocess emits one of those by contract), but
    /// recorded distinctly so we don't silently drop the message.
    #[error("codegen subprocess exited with unparseable error: {0}")]
    GenerateUnparseable(String),

    /// `tree_sitter_loader::Loader` couldn't compile the generated C
    /// or `dlopen` the resulting shared library.
    #[error("loading compiled parser: {0}")]
    LoadLanguage(#[from] tree_sitter_loader::LoaderError),
}

/// Thread-safe cache of compiled REPL parsers, plus the on-disk root for
/// their codegen artifacts. One cache per LSP process.
pub struct ReplCache {
    /// Parent of every per-key codegen dir, e.g.
    /// `$XDG_CACHE_HOME/ts_grammar_ls/repl/`.
    cache_root: PathBuf,
    /// `key -> Language`. Held behind a tokio Mutex so concurrent REPLs
    /// can share results without racing.
    entries: Mutex<HashMap<CacheKey, Arc<Language>>>,
    /// Per-key serialization gate. While a `(json, key)` compile is in
    /// flight, holds an `Arc<Mutex<()>>` keyed by that key. Subsequent
    /// `get_or_compile` calls for the same key acquire the same mutex
    /// and wait for the in-flight work to finish before re-checking
    /// `entries`. Without this, multiple `did_change` events fired
    /// before the first compile completes each spawn their own
    /// codegen+cc invocation, all writing to the same cache dir and
    /// racing on the `.so` output.
    in_flight: Mutex<HashMap<CacheKey, Arc<Mutex<()>>>>,
}

impl ReplCache {
    #[must_use]
    pub fn new(cache_root: PathBuf) -> Self {
        Self {
            cache_root,
            entries: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    /// Drop every in-memory entry. The on-disk dirs survive; the next
    /// `get_or_compile` for a still-valid key will skip the subprocess
    /// (artifacts on disk) and just reload via `Loader`. Called when the
    /// grammar source changes - we don't know which entries are still
    /// valid, so we conservatively clear all of them.
    pub async fn invalidate_all(&self) {
        self.entries.lock().await.clear();
    }

    /// Pure preparation: swap → normalize → serialize → hash. No IO.
    /// Returns the JSON payload (to be handed to the codegen subprocess
    /// later) and the cache key derived from it.
    ///
    /// # Errors
    ///
    ///
    #[expect(clippy::missing_panics_doc)]
    pub fn prepare(
        mut grammar: nativedsl::InputGrammar,
        rule_name: &str,
    ) -> Result<(String, CacheKey), ReplCompileError> {
        let idx = grammar
            .variables
            .iter()
            .position(|v| v.name == rule_name)
            .ok_or_else(|| ReplCompileError::RuleNotFound(rule_name.to_owned()))?;
        grammar.variables.swap(0, idx);

        // Strip configuration that would conflict with the chosen rule
        // being the start. The REPL is a "what does this rule match?"
        // tool; the user picked the rule explicitly and expects it to
        // parse their input directly, not be eaten as background.
        //
        // Specific conflicts:
        //   - `word_token`: codegen rejects making it also the start.
        //   - `extras`: a top-level `NamedSymbol(rule_name)` in extras
        //     means "treat matches of this rule as background between
        //     tokens" - if rule_name is also start, the parser eats
        //     all input as extras and leaves nothing to match.
        if grammar.word_token.as_deref() == Some(rule_name) {
            grammar.word_token = None;
        }
        grammar.extra_symbols.retain(|r| match r {
            tree_sitter_generate::rules::Rule::NamedSymbol(n) => n != rule_name,
            _ => true,
        });
        let grammar = grammar.normalize();

        let json_value = grammar_to_json(&grammar);
        let json = serde_json::to_string(&json_value)
            .expect("grammar_to_json produces valid serde_json::Value");

        let mut hasher = DefaultHasher::new();
        json.hash(&mut hasher);
        let key = CacheKey(hasher.finish());
        Ok((json, key))
    }

    /// Return the cached `Language` for `key`, or compile + load it from
    /// `grammar_json` if not cached. Re-execs ourselves with
    /// `generate-check --write-to` to produce the codegen artifacts, then
    /// hands the resulting dir to `tree_sitter_loader::Loader` to build
    /// and `dlopen` the shared library.
    ///
    /// Concurrent calls for the same `key` serialize on a per-key
    /// mutex (see [`Self::in_flight`]): the second caller waits for
    /// the first to finish and then sees the populated `entries` entry
    /// on its post-lock re-check, so we never run codegen + cc twice
    /// for the same key. Different keys still proceed in parallel.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the language needs to be compiled and said
    /// recompilation fails.
    pub async fn get_or_compile(
        &self,
        key: CacheKey,
        grammar_json: String,
    ) -> Result<Arc<Language>, ReplCompileError> {
        // Fast path: result already cached, no coordination needed.
        if let Some(lang) = self.entries.lock().await.get(&key) {
            return Ok(Arc::clone(lang));
        }

        // Get-or-insert a per-key serialization gate. Holding the
        // outer `in_flight` lock briefly is fine; the slow work
        // (codegen, cc, dlopen) runs under the inner per-key lock so
        // sibling keys' compiles aren't blocked.
        let per_key_lock = {
            let mut in_flight = self.in_flight.lock().await;
            Arc::clone(
                in_flight
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let guard = per_key_lock.lock().await;

        // Re-check after taking the per-key lock: an earlier caller
        // for this key may have just finished and populated `entries`
        // while we were waiting on the lock.
        if let Some(lang) = self.entries.lock().await.get(&key) {
            // Even though we're returning early, leave `in_flight`
            // cleanup to the caller that originally inserted - they
            // do it below after dropping their guard.
            return Ok(Arc::clone(lang));
        }

        let dir = self.cache_root.join(format!("{key}"));
        let src_dir = dir.join("src");

        // Skip codegen if the artifacts are already on disk (LSP restart
        // case: in-memory cache empty but disk cache populated).
        let needs_codegen =
            !src_dir.join("parser.c").exists() || !src_dir.join("grammar.json").exists();
        if needs_codegen {
            std::fs::create_dir_all(&dir).map_err(|source| ReplCompileError::CacheDir {
                path: dir.clone(),
                source,
            })?;
            run_codegen_subprocess(&dir, &grammar_json).await?;
        }

        // Put the compiled `.so` inside this cache entry's own dir so
        // it can't collide with sibling entries that share the same
        // grammar `name` (every rule-swap of one grammar carries the
        // same `name`). See `load_language`'s docstring.
        let lib_path = dir.join(format!("parser.{}", std::env::consts::DLL_EXTENSION));
        let language = load_language(&src_dir, lib_path)?;
        let arc = Arc::new(language);
        self.entries
            .lock()
            .await
            .insert(key.clone(), Arc::clone(&arc));

        // Release the per-key lock and remove its `in_flight` entry
        // now that the result is cached. Any waiters that grabbed an
        // `Arc` before removal still hold it and will re-check
        // `entries` on the fast path above; future callers find
        // nothing in `in_flight` and skip the gate entirely.
        drop(guard);
        self.in_flight.lock().await.remove(&key);
        Ok(arc)
    }
}

/// Re-exec the current binary with `generate-check --write-to <dir>` and
/// feed `grammar_json` on stdin. Returns the captured stdout (an error
/// message from the codegen pipeline) when the subprocess exits non-zero.
async fn run_codegen_subprocess(dir: &Path, grammar_json: &str) -> Result<(), ReplCompileError> {
    let exe = std::env::current_exe().map_err(ReplCompileError::Subprocess)?;
    let mut child = tokio::process::Command::new(exe)
        .arg("generate-check")
        .arg("--write-to")
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(ReplCompileError::Subprocess)?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(grammar_json.as_bytes())
            .await
            .map_err(ReplCompileError::Subprocess)?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(ReplCompileError::Subprocess)?;
    if output.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&output.stdout);
        match serde_json::from_str::<GenerateToDirError>(msg.trim()) {
            Ok(err) => Err(ReplCompileError::Generate(err)),
            Err(_) => Err(ReplCompileError::GenerateUnparseable(msg.into_owned())),
        }
    }
}

/// Compile `<src_dir>/parser.c` (and any sibling `scanner.c`) into a
/// shared library and load it as a `tree_sitter::Language`.
pub fn load_language(src_dir: &Path, output_path: PathBuf) -> Result<Language, ReplCompileError> {
    let loader = Loader::new()?;
    let config = CompileConfig::new(src_dir, None, Some(output_path));
    Ok(loader.load_language_at_path(config)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter_generate::parse_grammar::parse_grammar;

    /// Two rules: `start` references `helper` (so both survive the initial
    /// normalize). After swapping `helper` to position 0 and normalizing,
    /// `start` should be pruned (unreachable from the new start) and the
    /// JSON should differ from the swap-of-start version.
    #[test]
    fn prepare_swaps_target_rule_to_first() {
        let json = r#"{
            "name":"tiny",
            "rules":{
                "start":{"type":"SYMBOL","name":"helper"},
                "helper":{"type":"STRING","value":"hi"}
            }
        }"#;
        let grammar = parse_grammar(json).unwrap();

        let (out_json_start, key_start) =
            ReplCache::prepare(grammar.clone(), "start").expect("start exists");
        let (out_json_helper, key_helper) =
            ReplCache::prepare(grammar, "helper").expect("helper exists");

        // Distinct rules produce distinct cache keys and distinct JSON.
        assert_ne!(key_start, key_helper);
        assert_ne!(out_json_start, out_json_helper);

        // The "helper"-as-start JSON includes helper but not start
        // (start is unreachable from helper, normalize prunes it).
        assert!(out_json_helper.contains("\"helper\""));
        assert!(!out_json_helper.contains("\"start\""));
    }

    #[test]
    fn prepare_rejects_unknown_rule() {
        let json = r#"{"name":"tiny","rules":{"start":{"type":"STRING","value":"hi"}}}"#;
        let grammar = parse_grammar(json).unwrap();
        assert!(matches!(
            ReplCache::prepare(grammar, "no_such_rule"),
            Err(ReplCompileError::RuleNotFound(name)) if name == "no_such_rule"
        ));
    }

    #[test]
    fn prepare_strips_word_token_when_used_as_start() {
        // `identifier` is declared as the grammar's `word`. The REPL
        // strips it so codegen accepts the rule as a start.
        let json = r#"{
            "name":"tiny",
            "rules":{
                "source_file":{"type":"SYMBOL","name":"identifier"},
                "identifier":{"type":"PATTERN","value":"[a-z]+"}
            },
            "word":"identifier"
        }"#;
        let grammar = parse_grammar(json).unwrap();
        let (prepared_json, _) =
            ReplCache::prepare(grammar, "identifier").expect("prepare succeeds");
        assert!(
            !prepared_json.contains("\"word\""),
            "word_token should be stripped: {prepared_json}"
        );
    }

    #[test]
    fn prepare_strips_extras_referencing_start() {
        // `comment` is in extras. Picking it as start would make the
        // parser eat all input as extras, leaving the start rule
        // nothing to match. REPL prepare strips it from extras.
        let json = r#"{
            "name":"tiny",
            "rules":{
                "source_file":{"type":"SYMBOL","name":"word"},
                "word":{"type":"PATTERN","value":"[a-z]+"},
                "comment":{"type":"PATTERN","value":"//[^\\n]*"}
            },
            "extras":[
                {"type":"PATTERN","value":"\\s"},
                {"type":"SYMBOL","name":"comment"}
            ]
        }"#;
        let grammar = parse_grammar(json).unwrap();
        let (prepared_json, _) = ReplCache::prepare(grammar, "comment").expect("prepare succeeds");
        // Extras still has the whitespace pattern but not the comment
        // symbol reference.
        let extras_pos = prepared_json.find("\"extras\"").expect("extras present");
        let after_extras = &prepared_json[extras_pos..];
        let extras_end = after_extras.find(']').expect("closing bracket");
        let extras_slice = &after_extras[..=extras_end];
        assert!(
            !extras_slice.contains("\"comment\""),
            "comment symbol should not be in extras: {extras_slice}"
        );
    }

    #[test]
    fn prepare_is_deterministic() {
        // Same input → same key + same JSON.
        let json = r#"{"name":"tiny","rules":{"start":{"type":"STRING","value":"hi"}}}"#;
        let grammar = parse_grammar(json).unwrap();
        let (j1, k1) = ReplCache::prepare(grammar.clone(), "start").unwrap();
        let (j2, k2) = ReplCache::prepare(grammar, "start").unwrap();
        assert_eq!(j1, j2);
        assert_eq!(k1, k2);
    }

    /// Repro for the "(ERROR (comment))" issue: comment is a token-only
    /// rule, in extras, with a regex pattern that *should* match
    /// `// this is a comment` cleanly. Run with
    /// `cargo test --lib --ignored repl::tests::comment_as_start_rule`.
    #[test]
    #[ignore = "requires cc; run with --ignored"]
    fn comment_as_start_rule() {
        // Minimal grammar mirroring tree-sitter-c's shape, including:
        //   - comment is in extras AND defined as a token-wrapped rule.
        //   - comment's regex is escape-aware like the real C grammar.
        //   - source_file is the original start, references everything.
        let json = r#"{
            "name":"tinyc",
            "rules":{
                "source_file":{"type":"REPEAT","content":{"type":"SYMBOL","name":"statement"}},
                "statement":{"type":"SEQ","members":[
                    {"type":"STRING","value":"x"},
                    {"type":"STRING","value":";"}
                ]},
                "comment":{"type":"TOKEN","content":{"type":"CHOICE","members":[
                    {"type":"SEQ","members":[
                        {"type":"STRING","value":"//"},
                        {"type":"PATTERN","value":"(\\\\+(.|\\r?\\n)|[^\\\\\\n])*"}
                    ]},
                    {"type":"SEQ","members":[
                        {"type":"STRING","value":"/*"},
                        {"type":"PATTERN","value":"[^*]*\\*+([^/*][^*]*\\*+)*"},
                        {"type":"STRING","value":"/"}
                    ]}
                ]}}
            },
            "extras":[
                {"type":"PATTERN","value":"\\s|\\\\\\r?\\n"},
                {"type":"SYMBOL","name":"comment"}
            ]
        }"#;
        let grammar = parse_grammar(json).unwrap();

        let (prepared_json, _) = ReplCache::prepare(grammar, "comment").expect("prepare succeeds");

        // Compile in a temp dir + load.
        let tmp = tempfile::TempDir::new().unwrap();
        crate::generate_check::generate_to_dir(tmp.path(), &prepared_json).expect("generate");
        let language = load_language(
            &tmp.path().join("src"),
            tmp.path()
                .join(format!("parser.{}", std::env::consts::DLL_EXTENSION)),
        )
        .expect("loader");

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse("// this is a comment", None).unwrap();
        let sexp = tree.root_node().to_sexp();
        eprintln!("PARSED SEXP: {sexp}");
        // The whole input should parse as a `comment` with no error
        // wrapping. Failure here matches what the user reports in their
        // REPL session.
        assert!(
            !tree.root_node().has_error(),
            "tree contains an ERROR despite matching the start rule: {sexp}"
        );
        assert_eq!(sexp, "(comment)");
    }

    /// End-to-end loader exercise: produce artifacts in-process (skipping
    /// the `generate-check` subprocess hop, which the test binary can't
    /// re-enter), then drive `load_language` + `tree_sitter::Parser`.
    /// Shells out to `cc`, so marked `#[ignore]`. Run with
    /// `cargo test --lib --ignored repl::tests::load_language_compiles_and_parses`.
    #[test]
    #[ignore = "requires cc; run with --ignored"]
    fn load_language_compiles_and_parses() {
        let json = r#"{
            "name":"tiny",
            "rules":{
                "source_file":{"type":"REPEAT","content":{"type":"SYMBOL","name":"word"}},
                "word":{"type":"PATTERN","value":"[a-z]+"}
            },
            "extras":[{"type":"PATTERN","value":"\\s"}]
        }"#;
        let tmp = tempfile::TempDir::new().unwrap();
        crate::generate_check::generate_to_dir(tmp.path(), json).expect("generate");
        let language = load_language(
            &tmp.path().join("src"),
            tmp.path()
                .join(format!("parser.{}", std::env::consts::DLL_EXTENSION)),
        )
        .expect("loader compile + dlopen");

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse("hello world", None).unwrap();
        assert_eq!(tree.root_node().to_sexp(), "(source_file (word) (word))");
    }
}
