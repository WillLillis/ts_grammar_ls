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
    fn prepare_is_deterministic() {
        // Same input → same key + same JSON.
        let json = r#"{"name":"tiny","rules":{"start":{"type":"STRING","value":"hi"}}}"#;
        let grammar = parse_grammar(json).unwrap();
        let (j1, k1) = ReplCache::prepare(&grammar, "start").unwrap();
        let (j2, k2) = ReplCache::prepare(&grammar, "start").unwrap();
        assert_eq!(j1, j2);
        assert_eq!(k1, k2);
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
