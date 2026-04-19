use tower_lsp::lsp_types::{
    CompletionOptions, HoverProviderCapability, InitializeParams, InitializeResult, OneOf,
    RenameOptions, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, WorkDoneProgressOptions,
};

use crate::config::{self, Config};
use crate::handlers::semantic_tokens;
use crate::server::Backend;

/// Initializes the server
pub async fn initialize(backend: &Backend, params: InitializeParams) -> InitializeResult {
    let workspace_root = config::workspace_root_from_params(&params);

    // Load config: initialization_options > workspace root > user config > defaults.
    let config = if let Some(opts) = params.initialization_options
        && let Ok(c) = serde_json::from_value::<Config>(opts)
    {
        c
    } else {
        config::load_config(None, workspace_root.as_deref())
    };

    *backend.config.write().await = config;

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
