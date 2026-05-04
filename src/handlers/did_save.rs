use tower_lsp::lsp_types::DidSaveTextDocumentParams;
use tracing::info;

use crate::diagnostics;
use crate::server::Backend;

pub async fn did_save(backend: &Backend, params: DidSaveTextDocumentParams) {
    let uri = params.text_document.uri;
    info!("did_save: {uri}");

    let (text, version) = match backend.document_map.get_mut(&uri) {
        Some(mut doc) => {
            // Invalidate the cached analysis so the next get_analysis
            // recomputes with fresh data from disk for external modules.
            doc.analysis = None;
            (doc.text.clone(), doc.version)
        }
        None => return,
    };

    let generate_enabled = backend.config.read().await.diagnostics.generate_diagnostics;
    diagnostics::run_and_publish(
        &backend.client,
        &backend.document_map,
        &backend.generate_child,
        generate_enabled,
        uri,
        text,
        version,
    )
    .await;
}
