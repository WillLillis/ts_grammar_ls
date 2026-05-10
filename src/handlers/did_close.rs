use tower_lsp::lsp_types::DidCloseTextDocumentParams;
use tracing::info;

use crate::diagnostics;
use crate::server::{Backend, cancel_pending_diagnostics, drop_dependents};

#[allow(clippy::unused_async, reason = "required by LanguageServer trait")]
pub async fn did_close(backend: &Backend, params: DidCloseTextDocumentParams) {
    let uri = params.text_document.uri;
    info!("did_close: {uri}");

    // Remove this URI from the reverse-dependency index before dropping the
    // document; we need its deps to know which entries to clean up.
    if let Some((_, doc)) = backend.document_map.remove(&uri) {
        drop_dependents(&backend.dependents, &doc.deps, &uri);
    }
    cancel_pending_diagnostics(&backend.publish_handle, &uri);
    // Kill any in-flight generate-check subprocess for this document.
    diagnostics::kill_generate_child(&backend.generate_child, &uri);
}
