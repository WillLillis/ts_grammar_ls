use tower_lsp::lsp_types::DidCloseTextDocumentParams;
use tracing::info;

use crate::diagnostics;
use crate::server::{Backend, cancel_pending_diagnostics, drop_dependents};
use crate::workspace_index;

#[allow(clippy::unused_async, reason = "required by LanguageServer trait")]
pub async fn did_close(backend: &Backend, params: DidCloseTextDocumentParams) {
    let uri = params.text_document.uri;
    info!("did_close: {uri}");

    // REPL input buffer: drop the in-memory session. The compiled
    // `Language` lives in `repl_cache` and stays warm for future REPL
    // opens against the same (grammar, rule) - we don't want to throw
    // that away. Tree buffers carry no session state.
    if let Some(input_uri) = crate::repl::ReplInputUri::try_from_uri(&uri) {
        backend.repl_sessions.remove(&input_uri);
        return;
    }
    if crate::repl::ReplTreeUri::try_from_uri(&uri).is_some() {
        return;
    }

    // Remove this URI from the reverse-dependency index before dropping the
    // document; we need its deps to know which entries to clean up.
    if let Some((_, doc)) = backend.document_map.remove(&uri) {
        drop_dependents(&backend.dependents, &doc.deps, &uri);
    }
    // Re-index the file as closed (using disk content) so cross-file rename
    // can still reach it. If the file lives in the workspace this restores
    // the dependents/closed_file_deps entries we just removed; if it lives
    // outside we silently drop it from the workspace graph.
    if let Ok(path) = uri.to_file_path()
        && let Ok(canonical) = dunce::canonicalize(&path)
    {
        let prev = backend
            .closed_file_deps
            .get(&canonical)
            .map(|v| v.clone())
            .unwrap_or_default();
        let new_deps = workspace_index::index_file(&backend.dependents, &canonical, &prev);
        backend.closed_file_deps.insert(canonical, new_deps);
    }
    cancel_pending_diagnostics(&backend.publish_handle, &uri);
    // Kill any in-flight generate-check subprocess for this document.
    diagnostics::kill_generate_child(&backend.generate_child, &uri);
}
