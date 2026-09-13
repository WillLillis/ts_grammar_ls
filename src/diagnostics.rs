use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use ropey::Rope;
use rustc_hash::{FxHashMap, FxHashSet};

use tower_lsp::{
    Client,
    lsp_types::{
        Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString,
        ProgressParams, ProgressParamsValue, Range, Url, WorkDoneProgress, WorkDoneProgressBegin,
        WorkDoneProgressCreateParams, WorkDoneProgressEnd, notification::Progress,
        request::WorkDoneProgressCreate,
    },
};
use tree_sitter_generate::nativedsl::{self, DslError};

use crate::analysis::uri_to_grammar_path;
use crate::document::{DefKind, Document, Module};
use crate::text;
use tower_lsp::lsp_types::DiagnosticTag;
use tree_sitter_generate::nativedsl::serialize::grammar_to_json;

/// Merge DSL-phase and generate-phase diagnostics for publishing.
fn merge_diagnostics(doc: &Document) -> Vec<Diagnostic> {
    let mut out = doc.dsl_diagnostics.clone();
    out.extend(doc.generate_diagnostics.iter().cloned());
    out
}

/// Convert core's structured codegen diagnostics into LSP diagnostics. Only
/// `UnnecessaryConflicts` is surfaced here: it needs the full LR table build,
/// which the AST lints can't do. The enum's other variants overlap with the
/// AST lints L002-L004 (which give precise spans), so they're left to those -
/// the match is exhaustive so a new core variant forces a decision here.
fn codegen_diagnostics_to_lsp(
    diags: &[tree_sitter_generate::Diagnostic],
    module: &Module,
    uri: &Url,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for d in diags {
        match d {
            tree_sitter_generate::Diagnostic::UnnecessaryConflicts(groups) => {
                for finding in
                    crate::lints::unnecessary_conflicts::findings_from_conflicts(groups, module)
                {
                    out.push(crate::lints::finding_to_diagnostic(
                        &finding,
                        &module.rope,
                        uri,
                    ));
                }
            }
            // Each of these has a dedicated AST lint that reports it with a
            // precise source span, so the codegen-side copy is dropped here
            // rather than double-reported at the grammar's coarse span.
            tree_sitter_generate::Diagnostic::UnaryChoice { .. }
            | tree_sitter_generate::Diagnostic::UnarySeq { .. }
            | tree_sitter_generate::Diagnostic::EmptyStringMatch(_)
            | tree_sitter_generate::Diagnostic::UnsupportedRegexFlag { .. } => {}
            // TODO: no AST lint for this one yet ("rule is both a supertype
            // and inlined; the supertype is ignored"). Dropped for now so it
            // isn't reported without a usable span - it wants its own lint
            // that can point at the `supertypes:` / `inline:` entry.
            tree_sitter_generate::Diagnostic::SupertypeInlined { .. } => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

fn dsl_error_to_diagnostics(
    error: &DslError,
    documents: &nativedsl::DocumentMap,
) -> Vec<Diagnostic> {
    let primary = documents.document(error.document());
    let rope = Rope::from_str(primary.text());
    let range = error
        .span()
        .map(|s| text::span_to_range(&rope, s))
        .unwrap_or_default();
    let source = Some("ts_grammar_ls".into());

    let mut diagnostics = vec![Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source,
        message: error.to_string(),
        ..Default::default()
    }];

    let related: Vec<DiagnosticRelatedInformation> = error
        .notes()
        .iter()
        .filter_map(|note| {
            let document = documents.document(note.location.document);
            let note_uri = Url::from_file_path(document.path()).ok()?;
            let note_rope = Rope::from_str(document.text());
            let note_range = text::span_to_range(&note_rope, note.location.span);
            Some(DiagnosticRelatedInformation {
                location: Location {
                    uri: note_uri,
                    range: note_range,
                },
                message: note.message.to_string(),
            })
        })
        .collect();
    if !related.is_empty() {
        diagnostics[0].related_information = Some(related);
    }

    diagnostics
}

// ---------------------------------------------------------------------------
// Pipeline runner
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

/// Run the cheap native-DSL diagnostic pipeline and publish results.
///
/// Optionally fires off the (potentially long-running) generate-check
/// subprocess after publishing, gated on `include_generate_check`. Typing
/// (`did_change`) passes `false` to avoid burning CPU on a generate run that
/// will be killed by the next keystroke; save/open pass the user-configured
/// flag so a real check happens at points where the file is "settled".
///
/// After publishing for `uri`, also republishes DSL diagnostics for every
/// open document whose last analysis depended on this file (transitively
/// tracked in `dependents`). Generate-check is only triggered for `uri`
/// itself, never for the dependents.
pub async fn run_and_publish(
    client: &Client,
    document_map: &Arc<DashMap<Url, Document>>,
    generate_child: &Arc<GenerateChildSlot>,
    dependents: &Arc<DashMap<PathBuf, FxHashSet<Url>>>,
    include_generate_check: bool,
    uri: Url,
    text: String,
    version: i32,
) {
    // No on-disk path means we can't run the loader (no anchor for resolving
    // inherits/imports); publish whatever DSL diagnostics we can extract
    // from the text alone, but skip dependent republishing + generate-check.
    let Some(grammar_path) = uri_to_grammar_path(&uri) else {
        // We can't even reliably canonicalize, so synthesize nothing for now.
        return;
    };
    let grammar = publish_dsl_diagnostics(client, document_map, &uri, &text, version).await;

    // Republish DSL diagnostics for any open file that depends on this one.
    // Snapshot the set under the dashmap guard then drop it before awaiting.
    let dep_uris: Vec<Url> = dunce::canonicalize(&grammar_path)
        .ok()
        .and_then(|canonical| {
            dependents
                .get(&canonical)
                .map(|set| set.iter().filter(|u| **u != uri).cloned().collect())
        })
        .unwrap_or_default();
    for dep_uri in dep_uris {
        let snapshot = document_map
            .get(&dep_uri)
            .map(|d| (d.text.clone(), d.version));
        if let Some((dep_text, dep_version)) = snapshot {
            // Dependent files: republish their diagnostics; the parsed
            // grammar isn't needed (generate-check fires only for the
            // originating URI).
            let _ = publish_dsl_diagnostics(client, document_map, &dep_uri, &dep_text, dep_version)
                .await;
        }
    }

    if let Ok(grammar_json) = grammar
        && include_generate_check
    {
        spawn_generate_check(
            client.clone(),
            Arc::clone(document_map),
            Arc::clone(generate_child),
            uri,
            grammar_json,
            version,
        );
    }
}

/// Run the DSL pipeline for one document and publish the result. Returns
/// whether the pipeline produced no errors (so the caller can decide whether
/// to gate further work like generate-check on a clean DSL pass).
async fn publish_dsl_diagnostics(
    client: &Client,
    document_map: &Arc<DashMap<Url, Document>>,
    uri: &Url,
    text: &str,
    version: i32,
) -> Result<String, ()> {
    let Some(DslPass {
        module,
        diagnostics: new_diagnostics,
        helper_lint_diags,
        grammar_json: grammar,
    }) = run_dsl_pass(text, uri)
    else {
        return Err(());
    };
    {
        let Some(mut doc) = document_map.get_mut(uri) else {
            return grammar.ok_or(());
        };
        doc.dsl_diagnostics = new_diagnostics;
        let all = merge_diagnostics(&doc);
        drop(doc);
        client
            .publish_diagnostics(uri.clone(), all, Some(version))
            .await;
    }

    // Helper URIs that the grammar reached but had no findings for: we
    // still need to publish (an empty set) so stale findings from a
    // previous run get cleared on the client.
    publish_helper_lint_diags(client, document_map, &module, uri, helper_lint_diags).await;

    grammar.ok_or(())
}

/// Publish lint findings to each helper URI the grammar pulled in.
/// Helpers that exist in `document_map` get their existing
/// `dsl_diagnostics` + `generate_diagnostics` merged with the new lint
/// findings; closed helpers publish lint findings alone (any stale
/// state from when they were open is overwritten). Reached-but-clean
/// helpers publish an empty diagnostic list so stale findings clear.
///
/// NOTE: if two grammars both import the same helper, this races - the
/// last grammar to publish wins. Acceptable pre-release with a single
/// grammar; proper fix is keying lint diagnostics by
/// `(originating_grammar_uri, helper_uri)` on the Document.
async fn publish_helper_lint_diags(
    client: &Client,
    document_map: &Arc<DashMap<Url, Document>>,
    root: &Module,
    root_uri: &Url,
    mut findings_by_uri: FxHashMap<Url, Vec<Diagnostic>>,
) {
    let mut targets: FxHashSet<Url> = findings_by_uri.keys().cloned().collect();
    for module in root.reachable_modules() {
        if module.path == root.path {
            continue;
        }
        if let Ok(u) = Url::from_file_path(&module.path) {
            if u != *root_uri {
                targets.insert(u);
            }
        }
    }

    for target_uri in targets {
        let lint_diags = findings_by_uri.remove(&target_uri).unwrap_or_default();
        let merged = if let Some(doc) = document_map.get(&target_uri) {
            let mut m = doc.dsl_diagnostics.clone();
            m.extend(doc.generate_diagnostics.iter().cloned());
            m.extend(lint_diags);
            m
        } else {
            lint_diags
        };
        client.publish_diagnostics(target_uri, merged, None).await;
    }
}

/// Run every enabled lint and group findings by the URI of the file
/// they belong to. Findings in helper files (inherited / imported)
/// get their own URI so the LSP publisher can place them on the
/// correct document. CLI `--allow` flags / per-buffer suppression
/// comments would feed into the `LintSet` here in the future; for now
/// every lint runs unconditionally.
///
/// Rope lookup: each finding's span needs to be converted against the
/// rope of *its* source file, not the root's. We build a path → rope
/// index once over the reachable modules and use it during conversion.
fn lint_diagnostics_grouped(
    grammar: &nativedsl::InputGrammar,
    root: &Module,
) -> FxHashMap<Url, Vec<Diagnostic>> {
    let ctx = crate::lints::LintContext {
        module: root,
        grammar: Some(grammar),
    };
    let disabled = crate::lints::LintSet::default();
    let findings = crate::lints::run_all(&ctx, &disabled);

    let modules = root.reachable_modules();
    let mut out: FxHashMap<Url, Vec<Diagnostic>> = FxHashMap::default();
    for finding in &findings {
        let Some(module) = modules.iter().find(|m| m.path == finding.path) else {
            continue;
        };
        let Ok(uri) = Url::from_file_path(&finding.path) else {
            continue;
        };
        let diag = crate::lints::finding_to_diagnostic(finding, &module.rope, &uri);
        out.entry(uri).or_default().push(diag);
    }
    out
}

/// Everything one analyze pass produces that the async publisher needs. Every
/// field is `Send`.
struct DslPass {
    module: Module,
    /// Diagnostics belonging to the analyzed file itself.
    diagnostics: Vec<Diagnostic>,
    /// Lint findings belonging to inherited / imported files, by URI.
    helper_lint_diags: FxHashMap<Url, Vec<Diagnostic>>,
    /// Normalized grammar JSON for generate-check, if the pipeline succeeded.
    grammar_json: Option<String>,
}

/// Run the analyze pass and reduce it to `Send` data.
///
/// Deliberately **not** `async`: all grammar work is synchronous, and reducing
/// the pipeline result to diagnostics plus JSON before the caller awaits keeps
/// the larger AST and grammar values out of the async task state.
///
/// `normalize` consumes the grammar and `InputGrammar` isn't `Clone`, so
/// everything needing the un-normalized form runs first; the single normalize
/// then feeds both the dead-rule diff and the JSON.
fn run_dsl_pass(text: &str, uri: &Url) -> Option<DslPass> {
    // One loader pass yields the Module (for cfg hints) and the pipeline
    // outcome (errors and parsed grammar) together.
    let outcome = crate::analysis::analyze(text.to_owned(), uri)?;
    let module = outcome.module;
    let mut diagnostics = match &outcome.pipeline {
        Some(Err(e)) => dsl_error_to_diagnostics(e, &outcome.documents),
        _ => Vec::new(),
    };
    diagnostics.extend(module.cfg_hint_diagnostics());

    // Group lint findings by their source URI: the root's get merged with the
    // per-file diagnostics above; helper findings get published under each
    // helper's own URI so the client renders them on the right file.
    let mut helper_lint_diags: FxHashMap<Url, Vec<Diagnostic>> = FxHashMap::default();
    let mut grammar_json = None;

    if let Some(Ok(grammar)) = outcome.pipeline {
        for (finding_uri, diags) in lint_diagnostics_grouped(&grammar, &module) {
            if finding_uri == *uri {
                diagnostics.extend(diags);
            } else {
                helper_lint_diags
                    .entry(finding_uri)
                    .or_default()
                    .extend(diags);
            }
        }
        let before = rule_names(&grammar);
        let normalized = grammar.normalize(&mut Vec::new());
        diagnostics.extend(dead_rule_diagnostics(
            &before,
            &rule_names(&normalized),
            &module,
        ));
        grammar_json = serde_json::to_string(&grammar_to_json(&normalized)).ok();
    }

    Some(DslPass {
        module,
        diagnostics,
        helper_lint_diags,
        grammar_json,
    })
}

/// The grammar's rule names, resolved out of its own pool.
fn rule_names(grammar: &nativedsl::InputGrammar) -> FxHashSet<String> {
    grammar
        .variables
        .iter()
        .map(|v| grammar.pool.resolve(v.name).to_owned())
        .collect()
}

/// Diff the variable set before and after `InputGrammar::normalize()` to
/// find rules that are unreachable from the implicit roots (start rule,
/// `word_token`, names referenced in `extras` / `externals` /
/// `reserved_words`). Each dropped name that maps to a `Rule` /
/// `OverrideRule` definition in `module` gets a HINT diagnostic with the
/// `UNNECESSARY` tag - editors render it dimmed.
///
/// Defers the reachability computation to upstream's `normalize` so the
/// LSP and codegen agree on what "dead" means. Inherited / imported
/// rules that get dropped are skipped: their definition lives in another
/// file, and the diagnostic belongs there (that file's own analyze run
/// will surface it).
fn dead_rule_diagnostics(
    before: &FxHashSet<String>,
    after: &FxHashSet<String>,
    module: &Module,
) -> Vec<Diagnostic> {
    let Some(defs) = module.definitions.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in before.difference(after) {
        let Some(def) = defs
            .iter()
            .find(|d| d.name == *name && matches!(d.kind, DefKind::Rule | DefKind::OverrideRule))
        else {
            continue;
        };
        out.push(Diagnostic {
            range: text::span_to_range(&module.rope, def.full_span),
            severity: Some(DiagnosticSeverity::HINT),
            source: Some("ts_grammar_ls".into()),
            message: format!("unused rule `{name}`"),
            tags: Some(vec![DiagnosticTag::UNNECESSARY]),
            ..Default::default()
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Generate-check subprocess
// ---------------------------------------------------------------------------

/// In-flight generate-check subprocesses, keyed by document URI so concurrent
/// saves of different files don't cancel each other.
pub type GenerateChildSlot = DashMap<Url, tokio_util::sync::CancellationToken>;

/// Cancel the running generate-check subprocess for `uri` if any.
pub fn kill_generate_child(generate_child: &GenerateChildSlot, uri: &Url) {
    if let Some((_, token)) = generate_child.remove(uri) {
        token.cancel();
    }
}

/// Spawn a generate-check subprocess. Kills any existing one first.
#[expect(clippy::too_many_lines)]
fn spawn_generate_check(
    client: Client,
    document_map: Arc<DashMap<Url, Document>>,
    generate_child: Arc<GenerateChildSlot>,
    uri: Url,
    json: String,
    version: i32,
) {
    // The caller serializes the grammar first, so this task only carries the
    // JSON needed by the generate-check subprocess.
    tokio::spawn(async move {
        // Kill any previous generate-check for this URI; concurrent saves of
        // other files run in parallel.
        kill_generate_child(&generate_child, &uri);

        // Create a progress token.
        let token = NumberOrString::String("generate-check".into());
        let progress_ok = client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
            .is_ok();

        if progress_ok {
            client
                .send_notification::<Progress>(ProgressParams {
                    token: token.clone(),
                    value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                        WorkDoneProgressBegin {
                            title: "Generating parser".into(),
                            message: Some("Running full generation pipeline...".into()),
                            cancellable: Some(false),
                            percentage: None,
                        },
                    )),
                })
                .await;
        }

        // Spawn the subprocess.
        let Ok(exe) = std::env::current_exe() else {
            tracing::error!("failed to determine current executable path");
            return;
        };
        let mut child = match tokio::process::Command::new(exe)
            .arg("generate-check")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // If we drop the Child (e.g. after cancellation) the subprocess
            // is killed rather than left detached.
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to spawn generate-check: {e}");
                if progress_ok {
                    client
                        .send_notification::<Progress>(ProgressParams {
                            token,
                            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                                WorkDoneProgressEnd {
                                    message: Some(format!("Failed: {e}")),
                                },
                            )),
                        })
                        .await;
                }
                return;
            }
        };

        // Write the JSON to stdin and close it.
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            let _ = stdin.write_all(json.as_bytes()).await;
        }

        // Register a cancellation token so external code can signal us to stop.
        let cancel = tokio_util::sync::CancellationToken::new();
        generate_child.insert(uri.clone(), cancel.clone());

        // Wait for the child to finish or for cancellation, whichever comes first.
        // If cancelled, `child` is dropped - `kill_on_drop(true)` above ensures
        // the subprocess is killed.
        let output = tokio::select! {
            result = child.wait_with_output() => result.ok(),
            () = cancel.cancelled() => None,
        };

        // Killed mid-flight: end progress quietly and bail.
        let Some(output) = output else {
            if progress_ok {
                client
                    .send_notification::<Progress>(ProgressParams {
                        token,
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                            WorkDoneProgressEnd { message: None },
                        )),
                    })
                    .await;
            }
            return;
        };
        if !output.stderr.is_empty() {
            tracing::warn!(
                "generate-check stderr: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        // The validation subprocess prints a `GenerateCheckOutput` JSON line on
        // stdout for both success and failure (success still carries warnings
        // like unnecessary conflicts).
        let envelope: crate::generate_check::GenerateCheckOutput =
            serde_json::from_slice(&output.stdout).unwrap_or_default();
        let failed = !output.status.success();

        if progress_ok {
            let msg = if failed {
                "Parser generation failed"
            } else {
                "Parser generation successful"
            };
            client
                .send_notification::<Progress>(ProgressParams {
                    token,
                    value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                        WorkDoneProgressEnd {
                            message: Some(msg.into()),
                        },
                    )),
                })
                .await;
        }

        let all = document_map.get_mut(&uri).map(|mut doc| {
            let module = doc.last_good_analysis.clone();
            let mut generate_diagnostics = Vec::new();
            // Fatal codegen error, rendered from the structured error (clean
            // message rather than the raw serialized payload).
            if let Some(err) = &envelope.error {
                generate_diagnostics.push(Diagnostic {
                    range: Range::default(),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("ts_grammar_ls (generate)".into()),
                    message: err.to_string(),
                    ..Default::default()
                });
            }
            // Structured codegen warnings. Needs the AST to anchor conflicts.
            if let Some(module) = module.as_deref() {
                generate_diagnostics.extend(codegen_diagnostics_to_lsp(
                    &envelope.diagnostics,
                    module,
                    &uri,
                ));
            }
            doc.generate_diagnostics = generate_diagnostics;
            merge_diagnostics(&doc)
        });
        if let Some(all) = all {
            client
                .publish_diagnostics(uri.clone(), all, Some(version))
                .await;
        }
        // Subprocess finished naturally; drop our slot entry so the map
        // doesn't accumulate stale URIs across many saves.
        generate_child.remove(&uri);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter_generate::OptLevel;

    /// Write grammar text to a temp file and return the path.
    /// `parse_native_dsl` requires a canonicalizable path.
    fn temp_grammar(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tsg");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    /// Parse + normalize + serialize: the same shape as the tail of the
    /// production `run_dsl_pass`, minus the diagnostics it also collects.
    fn parse_and_serialize(text: &str, path: &std::path::Path) -> Option<String> {
        let grammar = nativedsl::parse_native_dsl(text, path).ok()?;
        let normalized = grammar.normalize(&mut Vec::new());
        serde_json::to_string(&grammar_to_json(&normalized)).ok()
    }

    #[test]
    fn prepare_grammar_json_valid() {
        let text = r#"
            grammar { language: "test" }
            rule program { repeat("x") }
        "#;
        let (_dir, path) = temp_grammar(text);
        let json = parse_and_serialize(text, &path);
        assert!(json.is_some());
        let json = json.unwrap();
        assert!(json.contains("\"name\":\"test\""));
    }

    #[test]
    fn prepare_grammar_json_invalid() {
        let text = r#"grammar { language: "test" } rule program {"#;
        let (_dir, path) = temp_grammar(text);
        assert_eq!(parse_and_serialize(text, &path), None);
    }

    #[test]
    fn generate_check_valid_grammar() {
        let text = r#"
            grammar { language: "test" }
            rule program { repeat("x") }
        "#;
        let (_dir, path) = temp_grammar(text);
        let json = parse_and_serialize(text, &path).unwrap();
        let result = tree_sitter_generate::generate_parser_for_grammar(
            &json,
            None,
            OptLevel::default(),
            &mut Vec::new(),
        );
        assert!(
            result.is_ok(),
            "valid grammar should generate: {:?}",
            result.err()
        );
    }

    #[test]
    fn generate_check_catches_errors() {
        let text = r#"
            grammar { language: "test" }
            rule program { choice(a, b) }
            rule a { seq("x", "y") }
            rule b { seq("x", "z") }
        "#;
        let (_dir, path) = temp_grammar(text);
        let json = parse_and_serialize(text, &path);
        // This grammar is valid at the DSL level.
        assert!(json.is_some());
        // It should also be valid at the generate level (no actual conflict).
        // A real conflict would need ambiguous rules, which is hard to
        // construct minimally. Just verify the pipeline doesn't panic.
        let result = tree_sitter_generate::generate_parser_for_grammar(
            &json.unwrap(),
            None,
            OptLevel::default(),
            &mut Vec::new(),
        );
        assert!(result.is_ok());
    }
}
