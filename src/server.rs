use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tower_lsp::{
    Client, LanguageServer, jsonrpc,
    lsp_types::{
        CodeActionParams, CodeActionResponse, CompletionParams, CompletionResponse,
        DidChangeConfigurationParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
        DocumentHighlight, DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse,
        GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeParams,
        InitializeResult, Location, PrepareRenameResponse, ReferenceParams, RenameParams,
        SemanticTokensParams, SemanticTokensResult, TextDocumentPositionParams, TextEdit, Url,
        WorkspaceEdit,
    },
};

use crate::config::Config;
use crate::diagnostics::GenerateChildSlot;
use crate::document::Document;
use crate::handlers;

pub struct Backend {
    pub client: Client,
    pub document_map: Arc<DashMap<Url, Document>>,
    /// Pending diagnostic-publish task per document. Replaced on every
    /// `did_change` and aborted on `did_save`/`did_open`/`did_close` so the
    /// most recent publish always wins.
    pub publish_handle: Arc<DashMap<Url, JoinHandle<()>>>,
    /// Handle to the currently running generate-check subprocess, if any.
    /// Killed on new edits to avoid stale work.
    pub generate_child: Arc<GenerateChildSlot>,
    /// Server configuration (can be updated at runtime).
    pub config: Arc<RwLock<Config>>,
}

/// Abort and discard any pending diagnostic-publish task for `uri`.
pub fn cancel_pending_diagnostics(handles: &DashMap<Url, JoinHandle<()>>, uri: &Url) {
    if let Some((_, handle)) = handles.remove(uri) {
        handle.abort();
    }
}

impl Backend {
    /// Run analysis for a document. Always re-runs the pipeline; the document
    /// only retains a `last_good_analysis` fallback that's served when the
    /// current text fails to parse (so features keep working mid-keystroke).
    #[must_use]
    pub fn get_analysis(
        &self,
        uri: &tower_lsp::lsp_types::Url,
    ) -> Option<std::sync::Arc<crate::document::Analysis>> {
        // Snapshot the source text under the read guard, then drop it before
        // calling analyze (which re-enters document_map for inherits/imports).
        let text = self.document_map.get(uri)?.text.clone();
        let fresh = crate::analysis::analyze(&text, uri);

        if fresh.definitions.is_some() {
            let arc = std::sync::Arc::new(fresh);
            if let Some(mut doc) = self.document_map.get_mut(uri) {
                doc.last_good_analysis = Some(std::sync::Arc::clone(&arc));
            }
            Some(arc)
        } else {
            // Parse failed: serve the last known-good analysis if we have one.
            self.document_map
                .get(uri)?
                .last_good_analysis
                .clone()
                .or_else(|| Some(std::sync::Arc::new(fresh)))
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        Ok(handlers::initialize::initialize(self, params).await)
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        handlers::did_change_configuration::did_change_configuration(self, params).await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        handlers::did_open::did_open(self, params).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        handlers::did_change::did_change(self, params).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        handlers::did_save::did_save(self, params).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        handlers::did_close::did_close(self, params).await;
    }

    async fn hover(&self, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
        Ok(handlers::hover::hover(self, &params))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> jsonrpc::Result<Option<GotoDefinitionResponse>> {
        Ok(handlers::goto_definition::goto_definition(self, &params))
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> jsonrpc::Result<Option<CompletionResponse>> {
        Ok(handlers::completion::completion(self, &params))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> jsonrpc::Result<Option<DocumentSymbolResponse>> {
        Ok(handlers::document_symbol::document_symbol(self, &params))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> jsonrpc::Result<Option<Vec<DocumentHighlight>>> {
        Ok(handlers::document_highlight::document_highlight(
            self, &params,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> jsonrpc::Result<Option<Vec<Location>>> {
        Ok(handlers::references::references(self, &params))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> jsonrpc::Result<Option<SemanticTokensResult>> {
        Ok(handlers::semantic_tokens::semantic_tokens_full(
            self, &params,
        ))
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
        Ok(handlers::formatting::formatting(self, &params).await)
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> jsonrpc::Result<Option<CodeActionResponse>> {
        Ok(handlers::code_action::code_action(self, &params))
    }

    async fn rename(&self, params: RenameParams) -> jsonrpc::Result<Option<WorkspaceEdit>> {
        Ok(handlers::rename::rename(self, &params))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> jsonrpc::Result<Option<PrepareRenameResponse>> {
        Ok(handlers::rename::prepare_rename(self, &params))
    }
}
