use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::RwLock;
use tower_lsp::{
    Client, LanguageServer, jsonrpc,
    lsp_types::{
        CodeActionParams, CodeActionResponse, CompletionParams, CompletionResponse,
        DidChangeConfigurationParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
        DocumentHighlight, DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse,
        GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeParams,
        InitializeResult, Location, ReferenceParams, SemanticTokensParams, SemanticTokensResult,
        TextEdit, Url,
    },
};

use crate::analysis::{AnalysisContext, BaseGrammarCache};
use crate::config::Config;
use crate::diagnostics::GenerateChildSlot;
use crate::document::Document;
use crate::handlers;

pub struct Backend {
    pub client: Client,
    pub document_map: Arc<DashMap<Url, Document>>,
    /// Tracks the latest version we've dispatched diagnostics for, so stale
    /// results from debounced tasks can be discarded.
    pub debounce_version: Arc<DashMap<Url, i32>>,
    /// Handle to the currently running generate-check subprocess, if any.
    /// Killed on new edits to avoid stale work.
    pub generate_child: Arc<GenerateChildSlot>,
    /// Server configuration (can be updated at runtime).
    pub config: Arc<RwLock<Config>>,
    /// Cache of parsed base grammars, keyed by path. Invalidated when the
    /// source version (document version or disk mtime) changes.
    pub base_grammar_cache: Arc<BaseGrammarCache>,
}

impl Backend {
    /// Build an `AnalysisContext` referencing this backend's cache and document map.
    #[must_use]
    pub fn analysis_context(&self) -> AnalysisContext<'_> {
        AnalysisContext {
            base_cache: &self.base_grammar_cache,
            document_map: &self.document_map,
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
}
