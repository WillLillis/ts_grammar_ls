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
//! 5. If cache miss: re-exec ourselves with `generate-check --write-to
//!    <dir>` to produce `<dir>/src/{parser.c, grammar.json, tree_sitter/*}`.
//! 6. Hand `<dir>/src/` to `tree_sitter_loader::Loader` to compile +
//!    `dlopen` into a `Language`. Cache the result.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tree_sitter::Language;
use tree_sitter_generate::nativedsl;
use tree_sitter_generate::nativedsl::serialize::grammar_to_json;
use tree_sitter_loader::{CompileConfig, Loader};

/// File-name suffix for the REPL input buffer. Used by `is_repl_uri` to
/// recognize URIs the LSP should treat as REPL state instead of `.tsg`
/// source.
///
/// Ends in `.tsg` (rather than `.txt`) so neovim's default filetype
/// detection picks up the buffer as `tsg`, which lspconfig setups
/// register against - without this the client never attaches the LSP
/// to the REPL buffer and `did_open` never fires. The `.tsg-repl.`
/// segment is what `is_repl_uri` actually keys on, so the LSP doesn't
/// confuse REPL buffers with real grammar files.
pub const REPL_INPUT_SUFFIX: &str = ".tsg-repl.tsg";

/// File-name suffix for the sibling metadata file. Each REPL input
/// buffer has a paired `<basename>.tsg-repl.json` that stores
/// server-owned state (the grammar URI) so any LSP process can rebuild
/// its session by reading the buffer + sibling, without depending on
/// in-memory state surviving a process boundary.
///
/// Why this is needed: client frameworks (lspconfig, etc.) compute
/// `root_dir` per buffer and spawn one LSP process per
/// `(filetype, root_dir)` pair. The grammar and the REPL buffer have
/// different roots, so two processes get involved. The process that
/// handled `tsg.openRepl` registered the session in memory; the process
/// that receives `did_open` on the REPL buffer is a different one and
/// has no in-memory state. The sibling file bridges them.
pub const REPL_META_SUFFIX: &str = ".tsg-repl.json";

/// File-name suffix for the parse-tree side buffer. Plain `.txt` keeps
/// editors from running our LSP against it (no `.tsg` extension) and
/// suppresses syntax highlighting; the contents are just a rendered
/// s-expression refreshed on every keystroke of the input buffer.
pub const REPL_TREE_SUFFIX: &str = ".tsg-repl-tree.txt";

/// On-disk path of the tree side buffer paired with a given REPL input
/// path. Mirrors `ReplMeta::sibling_path`'s naming.
#[must_use]
pub fn tree_path_for(repl_input_path: &std::path::Path) -> PathBuf {
    let parent = repl_input_path.parent().unwrap_or(std::path::Path::new(""));
    let name = repl_input_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    let stem = name.strip_suffix(REPL_INPUT_SUFFIX).unwrap_or(name);
    parent.join(format!("{stem}{REPL_TREE_SUFFIX}"))
}

/// `true` when `uri` points at a REPL input buffer (created by
/// `tsg.openRepl`). The naming convention is private to the LSP so
/// false positives on user-owned files are vanishingly unlikely.
#[must_use]
pub fn is_repl_uri(uri: &tower_lsp::lsp_types::Url) -> bool {
    uri.path().ends_with(REPL_INPUT_SUFFIX)
}

/// Server-owned metadata persisted next to each REPL input buffer.
/// Read on `did_open`/`did_change` when no in-memory session exists,
/// which happens whenever a client framework spawned a fresh LSP
/// process for the REPL buffer's `root_dir`.
/// Output format for the tree side buffer. Persisted in the metadata
/// file so the user's preference survives across LSP restarts.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TreeFormat {
    /// The structural s-expression. Compact; one line per node when
    /// pretty-printed. Good for an overview of the parse shape.
    Sexp,
    /// The full CST (`tree-sitter parse --output-cst` format) with row
    /// ranges and literal text for each node. More informative; one
    /// line per node, vertically larger. The default - matches what
    /// the upstream `tree-sitter` CLI produces.
    #[default]
    Cst,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ReplMeta {
    pub grammar_uri: tower_lsp::lsp_types::Url,
    #[serde(default)]
    pub format: TreeFormat,
}

impl ReplMeta {
    /// Path of the sibling metadata file for a given REPL input path.
    /// E.g. `/...d.tsg-repl.tsg` -> `/...d.tsg-repl.json`.
    #[must_use]
    pub fn sibling_path(repl_input_path: &std::path::Path) -> PathBuf {
        let parent = repl_input_path.parent().unwrap_or(std::path::Path::new(""));
        let name = repl_input_path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default();
        let stem = name.strip_suffix(REPL_INPUT_SUFFIX).unwrap_or(name);
        parent.join(format!("{stem}{REPL_META_SUFFIX}"))
    }

    /// Write `self` to its sibling path next to `repl_input_path`. No-op
    /// on serialization failure (the metadata is best-effort: a missing
    /// or corrupt file just degrades to the same "no session" path the
    /// LSP already handles).
    pub fn write_for(&self, repl_input_path: &std::path::Path) -> std::io::Result<()> {
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::sibling_path(repl_input_path), json)
    }

    /// Read the metadata file paired with `repl_input_path`. Returns
    /// `None` if the file is absent or unparseable.
    #[must_use]
    pub fn read_for(repl_input_path: &std::path::Path) -> Option<Self> {
        let raw = std::fs::read_to_string(Self::sibling_path(repl_input_path)).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

/// Extract the rule name from a REPL input buffer's header line.
/// Format: `# rule: <name>` on line 0. Returns `None` for buffers
/// missing the header (e.g. the user blew it away).
#[must_use]
pub fn parse_rule_header(text: &str) -> Option<&str> {
    let first_line = text.lines().next()?;
    let rest = first_line.strip_prefix("# rule:")?;
    let name = rest.trim();
    (!name.is_empty()).then_some(name)
}

/// Production cache root for REPL artifacts. Matches the path
/// `handlers::repl::open_repl` writes input buffers to, so the
/// compiled `.so` and the input buffer live as siblings under
/// `$XDG_CACHE_HOME/ts_grammar_ls/repl/`.
#[must_use]
pub fn default_cache_root() -> PathBuf {
    use etcetera::BaseStrategy as _;
    let base = etcetera::choose_base_strategy()
        .ok()
        .map(|s| s.cache_dir())
        .unwrap_or_else(std::env::temp_dir);
    base.join("ts_grammar_ls").join("repl")
}

/// Per-REPL-buffer state. Tracks which grammar/rule the REPL is bound
/// to and the most recently compiled `Language` so per-keystroke parses
/// don't need to re-compile.
pub struct ReplSession {
    /// The grammar this REPL parses against.
    pub grammar_uri: tower_lsp::lsp_types::Url,
    /// Last rule name parsed from the header. Re-parsed on every change.
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
}

/// Stable on-disk + in-memory identifier for one compiled REPL parser.
/// Hex-encoded hash of the post-swap, post-normalize grammar JSON.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey(String);

impl CacheKey {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Failure modes for `prepare` / `get_or_compile`.
#[derive(Debug)]
pub enum ReplCompileError {
    /// `rule_name` wasn't found in `grammar.variables`.
    RuleNotFound,
    /// `tree-sitter generate` reported a pipeline error.
    Codegen(String),
    /// `cc` / `dlopen` failed in the loader step.
    Compile(String),
    /// Couldn't spawn the codegen subprocess.
    Spawn(std::io::Error),
}

impl std::fmt::Display for ReplCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuleNotFound => write!(f, "rule not found in grammar"),
            Self::Codegen(msg) => write!(f, "codegen failed: {msg}"),
            Self::Compile(msg) => write!(f, "compile/load failed: {msg}"),
            Self::Spawn(e) => write!(f, "spawn failed: {e}"),
        }
    }
}

impl std::error::Error for ReplCompileError {}

/// Thread-safe cache of compiled REPL parsers, plus the on-disk root for
/// their codegen artifacts. One cache per LSP process.
pub struct ReplCache {
    /// Parent of every per-key codegen dir, e.g.
    /// `$XDG_CACHE_HOME/ts_grammar_ls/repl/`.
    cache_root: PathBuf,
    /// `key -> Language`. Held behind a tokio Mutex so concurrent REPLs
    /// can share results without racing.
    entries: Mutex<std::collections::HashMap<CacheKey, Arc<Language>>>,
}

impl ReplCache {
    #[must_use]
    pub fn new(cache_root: PathBuf) -> Self {
        Self {
            cache_root,
            entries: Mutex::new(std::collections::HashMap::new()),
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
    pub fn prepare(
        grammar: &nativedsl::InputGrammar,
        rule_name: &str,
    ) -> Result<(String, CacheKey), ReplCompileError> {
        let idx = grammar
            .variables
            .iter()
            .position(|v| v.name == rule_name)
            .ok_or(ReplCompileError::RuleNotFound)?;
        let mut g = grammar.clone();
        g.variables.swap(0, idx);

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
        //   - `supertype_symbols`: harmless to leave alone (only affects
        //     node-type inheritance, not parsing), but cheap to strip.
        if g.word_token.as_deref() == Some(rule_name) {
            g.word_token = None;
        }
        g.extra_symbols.retain(|r| match r {
            tree_sitter_generate::rules::Rule::NamedSymbol(n) => n != rule_name,
            _ => true,
        });
        g.supertype_symbols.retain(|s| s != rule_name);

        let g = g.normalize();
        let json_value = grammar_to_json(&g);
        let json = serde_json::to_string(&json_value)
            .expect("grammar_to_json produces valid serde_json::Value");

        let mut hasher = DefaultHasher::new();
        json.hash(&mut hasher);
        let key = CacheKey(format!("{:016x}", hasher.finish()));
        Ok((json, key))
    }

    /// Return the cached `Language` for `key`, or compile + load it from
    /// `grammar_json` if not cached. Re-execs ourselves with
    /// `generate-check --write-to` to produce the codegen artifacts, then
    /// hands the resulting dir to `tree_sitter_loader::Loader` to build
    /// and `dlopen` the shared library.
    pub async fn get_or_compile(
        &self,
        key: CacheKey,
        grammar_json: String,
    ) -> Result<Arc<Language>, ReplCompileError> {
        if let Some(lang) = self.entries.lock().await.get(&key) {
            return Ok(Arc::clone(lang));
        }

        let dir = self.cache_root.join(key.as_str());
        let src_dir = dir.join("src");

        // Skip codegen if the artifacts are already on disk (LSP restart
        // case: in-memory cache empty but disk cache populated).
        let needs_codegen = !src_dir.join("parser.c").exists()
            || !src_dir.join("grammar.json").exists();
        if needs_codegen {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return Err(ReplCompileError::Spawn(e));
            }
            run_codegen_subprocess(&dir, &grammar_json).await?;
        }

        let language = load_language(&src_dir)?;
        let arc = Arc::new(language);
        self.entries.lock().await.insert(key, Arc::clone(&arc));
        Ok(arc)
    }
}

/// Re-exec the current binary with `generate-check --write-to <dir>` and
/// feed `grammar_json` on stdin. Returns the captured stdout (an error
/// message from the codegen pipeline) when the subprocess exits non-zero.
async fn run_codegen_subprocess(dir: &Path, grammar_json: &str) -> Result<(), ReplCompileError> {
    let exe = std::env::current_exe().map_err(ReplCompileError::Spawn)?;
    let mut child = tokio::process::Command::new(exe)
        .arg("generate-check")
        .arg("--write-to")
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(ReplCompileError::Spawn)?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(grammar_json.as_bytes())
            .await
            .map_err(ReplCompileError::Spawn)?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(ReplCompileError::Spawn)?;
    if output.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err(ReplCompileError::Codegen(msg))
    }
}

/// Compile `<src_dir>/parser.c` (and any sibling `scanner.c`) into a
/// shared library and load it as a `tree_sitter::Language`. Uses the
/// stock `tree_sitter_loader::Loader`; per-grammar `src/tree_sitter/`
/// headers in the same dir resolve `#include "tree_sitter/parser.h"`.
pub fn load_language(src_dir: &Path) -> Result<Language, ReplCompileError> {
    let loader = Loader::new().map_err(|e| ReplCompileError::Compile(e.to_string()))?;
    let config = CompileConfig::new(src_dir, None, None);
    // `load_language_at_path` (vs `_with_name`) reads the `name` field
    // from `<src_dir>/grammar.json`, which is what the generated parser's
    // exported `tree_sitter_<name>` symbol matches.
    loader
        .load_language_at_path(config)
        .map_err(|e| ReplCompileError::Compile(e.to_string()))
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
            ReplCache::prepare(&grammar, "start").expect("start exists");
        let (out_json_helper, key_helper) =
            ReplCache::prepare(&grammar, "helper").expect("helper exists");

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
            ReplCache::prepare(&grammar, "no_such_rule"),
            Err(ReplCompileError::RuleNotFound)
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
            ReplCache::prepare(&grammar, "identifier").expect("prepare succeeds");
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
        let (prepared_json, _) =
            ReplCache::prepare(&grammar, "comment").expect("prepare succeeds");
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
    fn parse_rule_header_basic() {
        assert_eq!(parse_rule_header("# rule: expression\n1 + 2"), Some("expression"));
        assert_eq!(parse_rule_header("# rule: program"), Some("program"));
        // Tolerate extra whitespace around the name.
        assert_eq!(parse_rule_header("# rule:    spaced   \n"), Some("spaced"));
    }

    #[test]
    fn parse_rule_header_missing_or_malformed() {
        assert_eq!(parse_rule_header(""), None);
        assert_eq!(parse_rule_header("rule: no_pound"), None);
        assert_eq!(parse_rule_header("# something else"), None);
        assert_eq!(parse_rule_header("# rule:"), None);
        assert_eq!(parse_rule_header("# rule:   "), None);
    }

    #[test]
    fn prepare_is_deterministic() {
        // Same input → same key + same JSON.
        let json = r#"{"name":"tiny","rules":{"start":{"type":"STRING","value":"hi"}}}"#;
        let grammar = parse_grammar(json).unwrap();
        let (j1, k1) = ReplCache::prepare(&grammar, "start").unwrap();
        let (j2, k2) = ReplCache::prepare(&grammar, "start").unwrap();
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

        let (prepared_json, _) =
            ReplCache::prepare(&grammar, "comment").expect("prepare succeeds");

        // Compile in a temp dir + load.
        let tmp = tempfile::TempDir::new().unwrap();
        crate::generate_check::generate_to_dir(tmp.path(), &prepared_json).expect("generate");
        let language = load_language(&tmp.path().join("src")).expect("loader");

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
        let language = load_language(&tmp.path().join("src")).expect("loader compile + dlopen");

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse("hello world", None).unwrap();
        assert_eq!(tree.root_node().to_sexp(), "(source_file (word) (word))");
    }
}
