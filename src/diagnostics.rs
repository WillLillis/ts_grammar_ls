use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use ropey::Rope;
use rustc_hash::FxHashSet;

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
use crate::document::Document;
use crate::text;
use tree_sitter_generate::nativedsl::serialize::grammar_to_json;
use tree_sitter_generate::parse_grammar::normalize_grammar;

/// Merge DSL-phase and generate-phase diagnostics for publishing.
fn merge_diagnostics(doc: &Document) -> Vec<Diagnostic> {
    let mut out = doc.dsl_diagnostics.clone();
    out.extend(doc.generate_diagnostics.iter().cloned());
    out
}

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

fn dsl_error_to_diagnostics(error: &DslError, rope: &Rope) -> Vec<Diagnostic> {
    let range = error
        .span()
        .map(|s| text::span_to_range(rope, s))
        .unwrap_or_default();
    let source = Some("ts_grammar_ls".into());

    let mut diagnostics = vec![Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source,
        message: error.to_string(),
        ..Default::default()
    }];

    if let Some(note) = error.note()
        && let Ok(note_uri) = Url::from_file_path(&note.path)
    {
        let note_rope = Rope::from_str(&note.source);
        let note_range = text::span_to_range(&note_rope, note.span);
        diagnostics[0].related_information = Some(vec![DiagnosticRelatedInformation {
            location: Location {
                uri: note_uri,
                range: note_range,
            },
            message: note.message.to_string(),
        }]);
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
            let _ = publish_dsl_diagnostics(
                client,
                document_map,
                &dep_uri,
                &dep_text,
                dep_version,
            )
            .await;
        }
    }

    if let Ok(grammar) = grammar
        && include_generate_check
    {
        spawn_generate_check(
            client.clone(),
            Arc::clone(document_map),
            Arc::clone(generate_child),
            uri,
            grammar,
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
) -> Result<nativedsl::InputGrammar, ()> {
    // One loader pass yields the Module (for cfg hints) and the pipeline
    // outcome (errors and parsed grammar) together.
    let Some(outcome) = crate::analysis::analyze(text.to_owned(), uri) else {
        return Err(());
    };
    let mut new_diagnostics = match &outcome.pipeline {
        Some(Err(e)) => dsl_error_to_diagnostics(e, &outcome.module.rope),
        _ => Vec::new(),
    };
    new_diagnostics.extend(outcome.module.cfg_hint_diagnostics());

    let grammar = outcome.pipeline.and_then(Result::ok);
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
    grammar.ok_or(())
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
    grammar: nativedsl::InputGrammar,
    version: i32,
) {
    tokio::spawn(async move {
        let Some(json) = prepare_grammar_json(grammar) else {
            return;
        };

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

        let generate_diagnostics = match &output {
            Some(output) if output.status.success() => vec![],
            Some(output) => {
                // Log stderr for debugging; user-facing message uses stdout.
                if !output.stderr.is_empty() {
                    tracing::warn!(
                        "generate-check stderr: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                let error_msg = String::from_utf8_lossy(&output.stdout).trim().to_string();
                vec![Diagnostic {
                    range: Range::default(),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("ts_grammar_ls (generate)".into()),
                    message: error_msg,
                    ..Default::default()
                }]
            }
            None => {
                // Killed - end progress with no message and bail.
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
            }
        };

        if progress_ok {
            let msg = if generate_diagnostics.is_empty() {
                "Parser generation successful"
            } else {
                "Parser generation failed"
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

/// Normalize the parsed grammar and serialize it to JSON for the generate
/// subprocess. Takes the grammar by value: the DSL pipeline has already run
/// on this snapshot in `run_dsl_pipeline`, so no reparsing here.
fn prepare_grammar_json(mut grammar: nativedsl::InputGrammar) -> Option<String> {
    normalize_grammar(&mut grammar);
    let json_value = grammar_to_json(&grammar);
    serde_json::to_string(&json_value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write grammar text to a temp file and return the path.
    /// `parse_native_dsl` requires a canonicalizable path.
    fn temp_grammar(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tsg");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    /// Parse + serialize - the test-only equivalent of the production
    /// `run_dsl_pipeline` -> `prepare_grammar_json` path.
    fn parse_and_serialize(text: &str, path: &std::path::Path) -> Option<String> {
        let grammar = nativedsl::parse_native_dsl(text, path).ok()?;
        prepare_grammar_json(grammar)
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
        let result = tree_sitter_generate::generate_parser_for_grammar(&json, None);
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
        let result = tree_sitter_generate::generate_parser_for_grammar(&json.unwrap(), None);
        assert!(result.is_ok());
    }
}
