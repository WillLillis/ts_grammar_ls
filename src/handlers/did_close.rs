use tower_lsp::lsp_types::DidCloseTextDocumentParams;
use tracing::info;

use crate::diagnostics;
use crate::server::Backend;

pub async fn did_close(backend: &Backend, params: DidCloseTextDocumentParams) {
    let uri = params.text_document.uri;
    info!("did_close: {uri}");
    backend.document_map.remove(&uri);
    backend.debounce_version.remove(&uri);
    // Kill any in-flight generate-check subprocess for this document.
    diagnostics::kill_generate_child(&backend.generate_child, Some(&uri)).await;
}
