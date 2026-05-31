use tower_lsp::lsp_types::DidSaveTextDocumentParams;
use tracing::info;

use crate::diagnostics;
use crate::server::{Backend, cancel_pending_diagnostics};

pub async fn did_save(backend: &Backend, params: DidSaveTextDocumentParams) {
    let uri = params.text_document.uri;
    info!("did_save: {uri}");

    // REPL buffers don't go through the `.tsg` diagnostic pipeline.
    // Saves on a REPL buffer are no-ops: the parse state lives in
    // memory on the session and is already refreshed by did_change.
    if crate::repl::ReplInputUri::try_from_uri(&uri).is_some()
        || crate::repl::ReplTreeUri::try_from_uri(&uri).is_some()
    {
        return;
    }

    let (text, version) = match backend.document_map.get(&uri) {
        Some(doc) => (doc.text.clone(), doc.version),
        None => return,
    };

    // Supersede any pending publish task; we're publishing fresh diagnostics now.
    cancel_pending_diagnostics(&backend.publish_handle, &uri);

    let generate_enabled = backend.config.read().await.diagnostics.generate_diagnostics;
    diagnostics::run_and_publish(
        &backend.client,
        &backend.document_map,
        &backend.generate_child,
        &backend.dependents,
        generate_enabled,
        uri,
        text,
        version,
    )
    .await;
}
