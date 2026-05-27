use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use rustc_hash::FxHashSet;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tower_lsp::{
    Client, LanguageServer, jsonrpc,
    lsp_types::{
        CodeActionParams, CodeActionResponse, CompletionParams, CompletionResponse,
        DidChangeConfigurationParams, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
        DocumentFormattingParams, DocumentRangeFormattingParams,
        DocumentHighlight, DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse,
        GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeParams,
        InitializeResult, InitializedParams, Location, PrepareRenameResponse, ReferenceParams,
        RenameParams,
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
    /// Reverse-dependency index: for each external file path, the set of
    /// document URIs (open or closed-but-on-disk-in-workspace) whose last
    /// known dependency set loaded that path. Used to find dependents that
    /// need fresh diagnostics when an external file changes, and to extend
    /// cross-file rename across the workspace.
    pub dependents: Arc<DashMap<PathBuf, FxHashSet<Url>>>,
    /// Forward dep list for closed `.tsg` files indexed via the workspace
    /// scanner. Open files store their deps directly on `Document.deps`;
    /// closed files have no `Document` so we track them here for diff-based
    /// updates when a watcher event fires.
    pub closed_file_deps: Arc<DashMap<PathBuf, Vec<PathBuf>>>,
    /// Workspace folders advertised by the client at `initialize`; used by
    /// the workspace scanner to know where to look for `.tsg` files.
    pub workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    /// Server configuration (can be updated at runtime).
    pub config: Arc<RwLock<Config>>,
    /// On-disk + in-memory cache of compiled REPL parsers (one entry per
    /// (grammar JSON, rule name) pair). Shared across all REPL sessions
    /// since two sessions for the same grammar+rule can share the load.
    pub repl_cache: Arc<crate::repl::ReplCache>,
    /// Per-REPL-buffer state. Keyed by the REPL input URI. Created when
    /// `tsg.openRepl` fires; the change handler looks the session up by
    /// URI to know which grammar / rule to compile against.
    pub repl_sessions: Arc<DashMap<Url, std::sync::Mutex<crate::repl::ReplSession>>>,
}

/// Abort and discard any pending diagnostic-publish task for `uri`.
pub fn cancel_pending_diagnostics(handles: &DashMap<Url, JoinHandle<()>>, uri: &Url) {
    if let Some((_, handle)) = handles.remove(uri) {
        handle.abort();
    }
}

/// Walk a module's external dependencies (inherit + transitive imports) and
/// collect their canonical paths.
fn collect_deps(module: &crate::document::Module) -> Vec<PathBuf> {
    fn walk(out: &mut Vec<PathBuf>, info: &crate::document::Module) {
        out.push(info.path.clone());
        for (_, sub) in &info.import_modules {
            walk(out, sub);
        }
    }
    let mut deps = Vec::new();
    if let Some(base) = module.base_module.as_deref() {
        walk(&mut deps, base);
    }
    for (_, info) in &module.import_modules {
        walk(&mut deps, info);
    }
    deps
}

/// Reconcile `Document.deps` and the reverse `dependents` index against a
/// fresh analysis. Inserts/removes only the paths that changed.
fn update_dependents(
    dependents: &DashMap<PathBuf, FxHashSet<Url>>,
    doc: &mut Document,
    uri: &Url,
    new_deps: Vec<PathBuf>,
) {
    let old_deps = std::mem::replace(&mut doc.deps, new_deps);
    let old_set: FxHashSet<&PathBuf> = old_deps.iter().collect();
    let new_set: FxHashSet<&PathBuf> = doc.deps.iter().collect();

    for path in old_set.difference(&new_set) {
        if let Some(mut entry) = dependents.get_mut(*path) {
            entry.remove(uri);
        }
    }
    for path in new_set.difference(&old_set) {
        dependents
            .entry((*path).clone())
            .or_default()
            .insert(uri.clone());
    }
}

/// Drop a URI's entries from the reverse index; called from `did_close`.
pub fn drop_dependents(
    dependents: &DashMap<PathBuf, FxHashSet<Url>>,
    deps: &[PathBuf],
    uri: &Url,
) {
    for path in deps {
        if let Some(mut entry) = dependents.get_mut(path) {
            entry.remove(uri);
        }
    }
}

impl Backend {
    /// Run analysis for a document. Always re-runs the pipeline; the document
    /// retains a `last_good_analysis` fallback (snapshot from the last full
    /// Loader-pipeline success) so features keep working mid-keystroke when
    /// the current text fails to parse / resolve / typecheck.
    ///
    /// `last_good_analysis` only updates when `analyze` reports
    /// `loader_succeeded`. The manual-parse fallback inside `analyze` (which
    /// runs when any Loader stage failed - including resolve errors from
    /// unresolved-name typos mid-edit) lacks cross-file info, so we serve
    /// the previous full snapshot instead of letting features silently lose
    /// their `base_module` / `import_modules`.
    #[must_use]
    pub fn get_analysis(
        &self,
        uri: &tower_lsp::lsp_types::Url,
    ) -> Option<std::sync::Arc<crate::document::Module>> {
        // Snapshot the source text under the read guard, then drop it before
        // calling analyze (which re-enters document_map for inherits/imports).
        let text = self.document_map.get(uri)?.text.clone();
        let fresh = crate::analysis::analyze(text, uri)?.module;

        if fresh.loader_succeeded {
            let new_deps = collect_deps(&fresh);
            let arc = std::sync::Arc::new(fresh);
            if let Some(mut doc) = self.document_map.get_mut(uri) {
                doc.last_good_analysis = Some(std::sync::Arc::clone(&arc));
                update_dependents(&self.dependents, &mut doc, uri, new_deps);
            }
            Some(arc)
        } else {
            // Loader failed (or parse failed): serve the last known-good
            // snapshot if we have one, else fall back to whatever partial
            // analysis we managed to produce.
            self.document_map
                .get(uri)?
                .last_good_analysis
                .clone()
                .or_else(|| Some(std::sync::Arc::new(fresh)))
        }
    }

    /// Get the analysis for `uri` and convert `pos` to a byte offset, in one
    /// go. Both must succeed; if either fails, this returns `None`.
    ///
    /// The position is mapped through the LIVE document's rope, not the
    /// analysis's. `get_analysis` may serve `last_good_analysis` (a snapshot
    /// from before a transient parse error), whose rope reflects the older
    /// text. If the user has edited since that snapshot, mapping a live
    /// cursor through the stale rope can land on a wholly different region
    /// of the file - typically shifted by however many line breaks were
    /// added/removed. Using the live rope keeps the offset in live-buffer
    /// coordinates; downstream lookups against stale spans degrade
    /// gracefully (a span that no longer matches the live offset just
    /// fails to be picked up, rather than picking up the wrong identifier).
    #[must_use]
    pub fn resolve_position(
        &self,
        uri: &tower_lsp::lsp_types::Url,
        pos: tower_lsp::lsp_types::Position,
    ) -> Option<(std::sync::Arc<crate::document::Module>, u32)> {
        let offset = {
            let doc = self.document_map.get(uri)?;
            crate::text::position_to_offset(&doc.rope, pos)?
        };
        let analysis = self.get_analysis(uri)?;
        Some((analysis, offset))
    }

    /// Resolve `uri` to an analysis regardless of open/closed status. For
    /// open files this is `get_analysis` (with all its caching/fallback
    /// behavior); for closed files we read from disk and run the pipeline
    /// once. Used by cross-file rename to reach workspace dependents that
    /// the user hasn't opened in their editor.
    #[must_use]
    pub fn analysis_for_uri(
        &self,
        uri: &tower_lsp::lsp_types::Url,
    ) -> Option<std::sync::Arc<crate::document::Module>> {
        if self.document_map.contains_key(uri) {
            return self.get_analysis(uri);
        }
        let path = uri.to_file_path().ok()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let module = crate::analysis::analyze(text, uri)?.module;
        module
            .definitions
            .is_some()
            .then(|| std::sync::Arc::new(module))
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        Ok(handlers::initialize::initialize(self, params).await)
    }

    async fn initialized(&self, _params: InitializedParams) {
        handlers::initialize::initialized(self).await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        handlers::did_change_configuration::did_change_configuration(self, params).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        handlers::did_change_watched_files::did_change_watched_files(self, params).await;
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

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
        Ok(handlers::formatting::range_formatting(self, &params).await)
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

    async fn execute_command(
        &self,
        params: tower_lsp::lsp_types::ExecuteCommandParams,
    ) -> jsonrpc::Result<Option<serde_json::Value>> {
        Ok(handlers::repl::open_repl(self, &params).await)
    }
}
