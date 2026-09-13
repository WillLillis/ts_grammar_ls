use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{TextEdit, Url};
use tree_sitter_generate::OptLevel;
use tree_sitter_generate::nativedsl::InputGrammar;
use tree_sitter_generate::nativedsl::serialize::grammar_to_json;

use crate::analysis;
use crate::document::Module;
use crate::lints::{self, LintFinding};
use crate::text;

/// Exit code when no findings were produced.
pub const EXIT_OK: i32 = 0;
/// Exit code when at least one finding was produced.
pub const EXIT_FOUND: i32 = 1;
/// Exit code when the file could not be read or analyzed.
pub const EXIT_ERROR: i32 = 2;

/// Default grammar file name picked up when the CLI is given a directory.
pub const DEFAULT_GRAMMAR_FILE: &str = "grammar.tsg";

/// Run the lint CLI command against a single grammar entry point.
///
/// `path` is either a `.tsg` file or a directory; in the directory case
/// `grammar.tsg` is used. The loader pulls all imports/inherits into the
/// same shared AST, and the runner walks each module so findings in
/// helper files are surfaced through the grammar entry point - no need
/// to list helpers separately and no double-counting.
///
/// `allow` is the set of lint names to skip (parsed from `--allow NAME`
/// CLI args). Unknown names are reported as errors so typos surface
/// instead of silently doing nothing.
#[must_use]
pub fn run(path: &Path, allow: &[String], fix: bool) -> i32 {
    let grammar_path = match resolve_grammar_path(path) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{msg}");
            return EXIT_ERROR;
        }
    };

    let mut disabled = lints::LintSet::default();
    let mut had_unknown_lint = false;
    for name in allow {
        match lints::LintId::from_name(name) {
            Some(id) => {
                disabled.insert(id);
            }
            None => {
                eprintln!("unknown lint: `{name}`");
                had_unknown_lint = true;
            }
        }
    }
    if had_unknown_lint {
        return EXIT_ERROR;
    }

    if fix {
        return run_with_fixes(&grammar_path, &disabled);
    }

    let collected = match analyze_and_collect(&grammar_path, &disabled) {
        Ok(c) => c,
        Err(code) => return code,
    };
    for finding in &collected.findings {
        let source = collected
            .files
            .get(&finding.path)
            .map_or("", |(s, _)| s.as_str());
        print_finding(&finding.path, source, finding);
    }
    if collected.findings.is_empty() {
        EXIT_OK
    } else {
        EXIT_FOUND
    }
}

/// Cap on `--fix` passes. Each pass that applies anything removes at least one
/// finding, so real grammars converge in a handful; the cap only guards a
/// pathological non-converging case.
const MAX_FIX_PASSES: usize = 64;

/// One analysis pass: findings plus the source/rope of every reachable module,
/// owned so it outlives the analysis (the `--fix` loop re-reads from disk each
/// pass). `Err` carries the exit code to return.
struct Collected {
    findings: Vec<LintFinding>,
    files: rustc_hash::FxHashMap<PathBuf, (String, ropey::Rope)>,
}

fn analyze_and_collect(grammar_path: &Path, disabled: &lints::LintSet) -> Result<Collected, i32> {
    let source = std::fs::read_to_string(grammar_path).map_err(|e| {
        eprintln!("{}: {e}", grammar_path.display());
        EXIT_ERROR
    })?;
    let uri = Url::from_file_path(grammar_path).map_err(|()| {
        eprintln!("{}: not a valid file URI", grammar_path.display());
        EXIT_ERROR
    })?;
    let outcome = analysis::analyze(source, &uri).ok_or_else(|| {
        eprintln!("{}: failed to analyze", grammar_path.display());
        EXIT_ERROR
    })?;
    let Some(Ok(grammar)) = outcome.pipeline else {
        eprintln!(
            "{}: pipeline did not produce a grammar (fix DSL errors first)",
            grammar_path.display()
        );
        return Err(EXIT_ERROR);
    };

    // Lints walk root + all transitively-loaded modules themselves and tag
    // findings with the source file they belong to.
    let mut findings = {
        let ctx = lints::LintContext {
            module: &outcome.module,
            grammar: Some(&grammar),
        };
        lints::run_all(&ctx, disabled)
    };

    // The unnecessary-conflicts lint isn't AST-derivable - it needs the full
    // LR table build. Run the generator in-process (one-shot CLI, so no
    // subprocess isolation like the live server needs) to get tree-sitter's
    // structured conflict diagnostics, then map them onto the `conflicts:`
    // declarations via the shared helper.
    if !disabled.contains(&lints::LintId::UnnecessaryConflicts) {
        // Takes the grammar by value: `normalize` consumes it and
        // `InputGrammar` isn't `Clone`.
        findings.extend(unnecessary_conflict_findings(grammar, &outcome.module));
    }

    let files = outcome
        .module
        .reachable_modules()
        .iter()
        .map(|m| (m.path.clone(), (m.source.clone(), m.rope.clone())))
        .collect();
    Ok(Collected { findings, files })
}

/// Apply fixes in a loop, re-analyzing between passes. Edits from independent
/// findings can overlap (two adjacent `conflicts:` entries share the comma
/// between them); a single pass applies a maximal non-overlapping batch, and
/// re-analysis lets the rest land next pass. Prints a summary plus any findings
/// left unfixed; exit is `EXIT_FOUND` when unfixable findings remain.
fn run_with_fixes(grammar_path: &Path, disabled: &lints::LintSet) -> i32 {
    let mut total = 0usize;
    for _ in 0..MAX_FIX_PASSES {
        let collected = match analyze_and_collect(grammar_path, disabled) {
            Ok(c) => c,
            Err(code) => return code,
        };
        match apply_pass(&collected) {
            Err(code) => return code,
            Ok(0) => {
                if total > 0 {
                    println!("applied {total} fix(es)");
                }
                let remaining: Vec<&LintFinding> = collected
                    .findings
                    .iter()
                    .filter(|f| f.fix.is_none())
                    .collect();
                for f in &remaining {
                    let source = collected.files.get(&f.path).map_or("", |(s, _)| s.as_str());
                    print_finding(&f.path, source, f);
                }
                return if remaining.is_empty() {
                    EXIT_OK
                } else {
                    EXIT_FOUND
                };
            }
            Ok(n) => total += n,
        }
    }
    eprintln!("--fix did not converge after {MAX_FIX_PASSES} passes");
    EXIT_ERROR
}

/// Apply one maximal non-overlapping batch of fixes across all files, writing
/// each changed file. Returns the number of edits applied this pass.
fn apply_pass(collected: &Collected) -> Result<usize, i32> {
    use rustc_hash::FxHashMap;
    let mut edits_by_path: FxHashMap<&PathBuf, Vec<&TextEdit>> = FxHashMap::default();
    for f in &collected.findings {
        if let Some(fix) = &f.fix {
            edits_by_path.entry(&f.path).or_default().extend(&fix.edits);
        }
    }

    let mut applied = 0usize;
    for (path, edits) in edits_by_path {
        let Some((source, rope)) = collected.files.get(path) else {
            continue;
        };
        // LSP ranges -> byte ranges; apply largest-offset-first so earlier
        // offsets stay valid. Skip any edit overlapping one already applied.
        let mut byte_edits: Vec<(u32, u32, &str)> = edits
            .iter()
            .filter_map(|e| {
                let s = text::position_to_offset(rope, e.range.start)?;
                let end = text::position_to_offset(rope, e.range.end)?;
                Some((s, end, e.new_text.as_str()))
            })
            .collect();
        byte_edits.sort_by_key(|&(start, ..)| std::cmp::Reverse(start));

        let mut content = source.clone();
        let mut prev_start = u32::MAX;
        let mut n = 0usize;
        for &(s, end, new_text) in &byte_edits {
            if end > prev_start {
                continue;
            }
            content.replace_range(s as usize..end as usize, new_text);
            prev_start = s;
            n += 1;
        }
        if n > 0 {
            if let Err(e) = std::fs::write(path, &content) {
                eprintln!("{}: {e}", path.display());
                return Err(EXIT_ERROR);
            }
            applied += n;
        }
    }
    Ok(applied)
}

/// Build `unnecessary-conflicts` findings by generating the parser in-process
/// and mapping tree-sitter's structured conflict diagnostics onto the source
/// `conflicts:` declarations. Returns empty if the grammar can't be serialized
/// (the AST lints already ran independently, so we don't surface generate
/// failures from the lint command).
fn unnecessary_conflict_findings(grammar: InputGrammar, module: &Module) -> Vec<LintFinding> {
    let normalized = grammar.normalize(&mut Vec::new());
    let Ok(json) = serde_json::to_string(&grammar_to_json(&normalized)) else {
        return Vec::new();
    };
    let mut diags = Vec::new();
    let _ = tree_sitter_generate::generate_parser_for_grammar(
        &json,
        None,
        OptLevel::default(),
        &mut diags,
    );
    diags
        .iter()
        .filter_map(|d| match d {
            tree_sitter_generate::Diagnostic::UnnecessaryConflicts(groups) => Some(
                lints::unnecessary_conflicts::findings_from_conflicts(groups, module),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Resolve the user-supplied path to a `.tsg` file. Directories pick up
/// `grammar.tsg` inside them; files must have a `.tsg` extension.
fn resolve_grammar_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_dir() {
        let candidate = path.join(DEFAULT_GRAMMAR_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        return Err(format!(
            "{}: no {DEFAULT_GRAMMAR_FILE} found in directory",
            path.display()
        ));
    }
    if !path.is_file() {
        return Err(format!("{}: not a file", path.display()));
    }
    if path.extension().is_none_or(|e| e != "tsg") {
        return Err(format!("{}: not a .tsg file", path.display()));
    }
    Ok(path.to_path_buf())
}

/// Print a finding in compiler-style: `path:line:col: warning[L001] msg`.
fn print_finding(path: &Path, source: &str, finding: &lints::LintFinding) {
    let (line, col) = line_col(source, finding.span.start);
    let sev = match finding.lint.severity() {
        lints::Severity::Error => "error",
        lints::Severity::Warning => "warning",
        lints::Severity::Info => "info",
        lints::Severity::Hint => "hint",
    };
    println!(
        "{}:{}:{}: {}[{}] {}",
        path.display(),
        line,
        col,
        sev,
        finding.lint.code_str(),
        finding.message,
    );
}

/// 1-based line/column for a byte offset. Column is bytes from line start
/// (terminal output, not LSP - so we don't pay the UTF-16 conversion).
fn line_col(source: &str, offset: u32) -> (usize, usize) {
    let offset = (offset as usize).min(source.len());
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let col = prefix.rfind('\n').map_or(offset, |nl| offset - nl - 1) + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_first_line() {
        assert_eq!(line_col("abc\ndef", 0), (1, 1));
        assert_eq!(line_col("abc\ndef", 2), (1, 3));
    }

    #[test]
    fn line_col_subsequent_lines() {
        assert_eq!(line_col("abc\ndef", 4), (2, 1));
        assert_eq!(line_col("abc\ndef", 6), (2, 3));
    }

    #[test]
    fn line_col_clamps_oversized_offset() {
        assert_eq!(line_col("abc", 100), (1, 4));
    }

    #[test]
    fn resolve_grammar_path_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo.tsg");
        std::fs::write(&path, "").unwrap();
        assert_eq!(resolve_grammar_path(&path).unwrap(), path);
    }

    #[test]
    fn resolve_grammar_path_directory_picks_default() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join(DEFAULT_GRAMMAR_FILE);
        std::fs::write(&default, "").unwrap();
        assert_eq!(resolve_grammar_path(dir.path()).unwrap(), default);
    }

    #[test]
    fn resolve_grammar_path_directory_missing_default() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_grammar_path(dir.path()).is_err());
    }

    #[test]
    fn resolve_grammar_path_wrong_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo.txt");
        std::fs::write(&path, "").unwrap();
        assert!(resolve_grammar_path(&path).is_err());
    }

    /// All conflicts unnecessary (single-line): the whole `conflicts:` field is
    /// dropped rather than left as `conflicts: []`.
    #[test]
    fn fix_removes_adjacent_unnecessary_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        let src = "grammar {\n    language: \"t\",\n    conflicts: [[a, b], [c, d]],\n}\n\
                   rule program { choice(a, b, c, d) }\n\
                   rule a { \"w\" }\nrule b { \"x\" }\nrule c { \"y\" }\nrule d { \"z\" }\n";
        std::fs::write(&path, src).unwrap();

        assert_eq!(run(&path, &[], true), EXIT_OK);
        let fixed = std::fs::read_to_string(&path).unwrap();
        assert!(!fixed.contains("conflicts"), "fixed:\n{fixed}");
        assert!(fixed.contains("language: \"t\",\n}"), "fixed:\n{fixed}");
        // Nothing left to find on a re-lint.
        assert_eq!(run(&path, &[], false), EXIT_OK);
    }

    /// All conflicts unnecessary in a multi-line list with a per-entry comment:
    /// the whole field (and the comment) goes, with no empty `conflicts: [ ]`
    /// or orphaned comment left behind.
    #[test]
    fn fix_removes_whole_field_multiline_with_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grammar.tsg");
        let src = "grammar {\n    language: \"t\",\n    conflicts: [\n        [a, b], // note\n        [c, d],\n    ],\n}\n\
                   rule program { choice(a, b, c, d) }\n\
                   rule a { \"w\" }\nrule b { \"x\" }\nrule c { \"y\" }\nrule d { \"z\" }\n";
        std::fs::write(&path, src).unwrap();

        assert_eq!(run(&path, &[], true), EXIT_OK);
        let fixed = std::fs::read_to_string(&path).unwrap();
        assert!(!fixed.contains("conflicts"), "fixed:\n{fixed}");
        assert!(!fixed.contains("note"), "fixed:\n{fixed}");
        assert!(fixed.contains("language: \"t\",\n}"), "fixed:\n{fixed}");
        assert_eq!(run(&path, &[], false), EXIT_OK);
    }
}
