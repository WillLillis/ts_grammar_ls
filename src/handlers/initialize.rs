use tower_lsp::lsp_types::{
    CompletionOptions, DidChangeWatchedFilesRegistrationOptions, FileSystemWatcher,
    GlobPattern, HoverProviderCapability, InitializeParams, InitializeResult, OneOf, Registration,
    RenameOptions, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, WorkDoneProgressOptions,
};
use tracing::warn;

use crate::config::{self, Config};
use crate::handlers::semantic_tokens;
use crate::server::Backend;
use crate::workspace_index;

/// Initializes the server
pub async fn initialize(backend: &Backend, params: InitializeParams) -> InitializeResult {
    let roots = config::workspace_roots_from_params(&params);
    let workspace_root = roots.first().cloned();

    // Load config: initialization_options > workspace root > user config > defaults.
    let config = if let Some(opts) = params.initialization_options
        && let Ok(c) = serde_json::from_value::<Config>(opts)
    {
        c
    } else {
        config::load_config(None, workspace_root.as_deref())
    };

    *backend.config.write().await = config;
    *backend.workspace_roots.write().await = roots;

    InitializeResult {
        capabilities: ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            definition_provider: Some(OneOf::Left(true)),
            references_provider: Some(OneOf::Left(true)),
            document_highlight_provider: Some(OneOf::Left(true)),
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(vec![".".into(), ":".into()]),
                ..Default::default()
            }),
            document_symbol_provider: Some(OneOf::Left(true)),
            document_formatting_provider: Some(OneOf::Left(true)),
            document_range_formatting_provider: Some(OneOf::Left(true)),
            code_action_provider: Some(tower_lsp::lsp_types::CodeActionProviderCapability::Simple(
                true,
            )),
            rename_provider: Some(OneOf::Right(RenameOptions {
                prepare_provider: Some(true),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            })),
            semantic_tokens_provider: Some(
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    legend: semantic_tokens::legend(),
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                    range: None,
                    ..Default::default()
                }),
            ),
            ..Default::default()
        },
        server_info: Some(ServerInfo {
            name: "ts_grammar_ls".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        }),
    }
}

/// Called by the client after `initialize` completes. We use this hook to
/// dynamically register a `**/*.tsg` file watcher so the client notifies us
/// when external (closed-but-on-disk) grammar files change. Clients that
/// don't support dynamic registration will return an error here, which we
/// log and ignore - the LSP still works, just without auto-refresh of
/// closed-file dependents.
pub async fn initialized(backend: &Backend) {
    // Fire watcher registration as a detached task: it awaits a response from
    // the client, and we don't want the workspace scan (or any subsequent
    // `initialized` work) waiting on that round-trip.
    let client = backend.client.clone();
    tokio::spawn(async move {
        let registrations = vec![
            Registration {
                id: "ts-grammar-ls/watch-tsg".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers: vec![FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/*.tsg".into()),
                        kind: None,
                    }],
                })
                .ok(),
            },
            // The `did_change_configuration` handler is wired but won't be
            // invoked unless the client knows we want notifications.
            Registration {
                id: "ts-grammar-ls/did-change-configuration".into(),
                method: "workspace/didChangeConfiguration".into(),
                register_options: None,
            },
        ];
        if let Err(e) = client.register_capability(registrations).await {
            warn!("client did not accept dynamic registrations: {e}");
        }
    });

    scan_workspace(backend).await;
}

/// Walk every workspace root for `.tsg` files and populate the dep index.
/// Skips files already open (their `Document.deps` is the source of truth).
async fn scan_workspace(backend: &Backend) {
    let roots = backend.workspace_roots.read().await.clone();
    if roots.is_empty() {
        return;
    }

    // Filesystem walk + parse can be non-trivial on large trees. Run on a
    // blocking thread so we don't tie up the LSP request runtime.
    let dependents = std::sync::Arc::clone(&backend.dependents);
    let closed_file_deps = std::sync::Arc::clone(&backend.closed_file_deps);
    let document_map = std::sync::Arc::clone(&backend.document_map);
    let _ = tokio::task::spawn_blocking(move || {
        for root in &roots {
            for path in workspace_index::discover_tsg_files(root) {
                let Ok(canonical) = dunce::canonicalize(&path) else {
                    continue;
                };
                // If this file is open, its analysis (via get_analysis) is
                // the canonical dep source. Skip the closed-file path.
                if let Ok(uri) = tower_lsp::lsp_types::Url::from_file_path(&canonical)
                    && document_map.contains_key(&uri)
                {
                    continue;
                }
                let prev = closed_file_deps
                    .get(&canonical)
                    .map(|v| v.clone())
                    .unwrap_or_default();
                let new_deps = workspace_index::index_file(&dependents, &canonical, &prev);
                closed_file_deps.insert(canonical, new_deps);
            }
        }
    })
    .await;
}
