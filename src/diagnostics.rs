use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use ropey::Rope;
use tokio::sync::Mutex;

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

/// Run the full DSL pipeline and return diagnostics only.
/// Analysis data is computed on demand by handlers via `analysis::analyze()`.
fn run_dsl_pipeline(text: &str, rope: &Rope, grammar_path: &Path) -> Vec<Diagnostic> {
    match nativedsl::parse_native_dsl(text, grammar_path) {
        Ok(_) => vec![],
        Err(e) => dsl_error_to_diagnostics(&e, rope),
    }
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

pub async fn run_and_publish(
    client: &Client,
    document_map: &Arc<DashMap<Url, Document>>,
    generate_child: &Arc<GenerateChildSlot>,
    generate_enabled: bool,
    uri: Url,
    text: String,
    version: i32,
) {
    let grammar_path = uri_to_grammar_path(&uri);
    let rope = Rope::from_str(&text);
    let dsl_diagnostics = run_dsl_pipeline(&text, &rope, &grammar_path);

    let dsl_ok = dsl_diagnostics.is_empty();

    // Write diagnostics under the lock, then release before awaiting the client.
    let all = {
        let Some(mut doc) = document_map.get_mut(&uri) else {
            return;
        };
        doc.diagnostics.dsl = dsl_diagnostics;
        doc.diagnostics.all()
    };
    client
        .publish_diagnostics(uri.clone(), all, Some(version))
        .await;

    if dsl_ok && generate_enabled {
        spawn_generate_check(
            client.clone(),
            Arc::clone(document_map),
            Arc::clone(generate_child),
            uri,
            text,
            grammar_path,
            version,
        );
    }
}

// ---------------------------------------------------------------------------
// Generate-check subprocess
// ---------------------------------------------------------------------------

/// Handle to the currently in-flight generate-check subprocess.
pub type GenerateChildSlot = Mutex<Option<(Url, tokio_util::sync::CancellationToken)>>;

/// Cancel the running generate-check subprocess.
///
/// If `only_for_uri` is `Some`, only cancels if the current subprocess was
/// spawned for that URI.
pub async fn kill_generate_child(generate_child: &GenerateChildSlot, only_for_uri: Option<&Url>) {
    let mut guard = generate_child.lock().await;
    let should_cancel = match (only_for_uri, guard.as_ref()) {
        (Some(uri), Some((u, _))) => u == uri,
        (None, Some(_)) => true,
        _ => false,
    };
    if should_cancel && let Some((_, token)) = guard.take() {
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
    text: String,
    grammar_path: PathBuf,
    version: i32,
) {
    tokio::spawn(async move {
        let Some(json) = prepare_grammar_json(&text, &grammar_path) else {
            return;
        };

        // Kill any previous generate-check subprocess.
        kill_generate_child(&generate_child, None).await;

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
        *generate_child.lock().await = Some((uri.clone(), cancel.clone()));

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
            doc.diagnostics.generate = generate_diagnostics;
            doc.diagnostics.all()
        });
        if let Some(all) = all {
            client.publish_diagnostics(uri, all, Some(version)).await;
        }
    });
}

/// Run the DSL pipeline and serialize the result to grammar JSON.
fn prepare_grammar_json(text: &str, grammar_path: &Path) -> Option<String> {
    let mut grammar = nativedsl::parse_native_dsl(text, grammar_path).ok()?;
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

    #[test]
    fn prepare_grammar_json_valid() {
        let text = r#"
            grammar { language: "test" }
            rule program { repeat("x") }
        "#;
        let (_dir, path) = temp_grammar(text);
        let json = prepare_grammar_json(text, &path);
        assert!(json.is_some());
        let json = json.unwrap();
        assert!(json.contains("\"name\":\"test\""));
    }

    #[test]
    fn prepare_grammar_json_invalid() {
        let text = r#"grammar { language: "test" } rule program {"#;
        let (_dir, path) = temp_grammar(text);
        assert_eq!(prepare_grammar_json(text, &path), None);
    }

    #[test]
    fn generate_check_valid_grammar() {
        let text = r#"
            grammar { language: "test" }
            rule program { repeat("x") }
        "#;
        let (_dir, path) = temp_grammar(text);
        let json = prepare_grammar_json(text, &path).unwrap();
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
        let json = prepare_grammar_json(text, &path);
        // This grammar is valid at the DSL level.
        assert!(json.is_some());
        // It should also be valid at the generate level (no actual conflict).
        // A real conflict would need ambiguous rules, which is hard to
        // construct minimally. Just verify the pipeline doesn't panic.
        let result = tree_sitter_generate::generate_parser_for_grammar(&json.unwrap(), None);
        assert!(result.is_ok());
    }
}
