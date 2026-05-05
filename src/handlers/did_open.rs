use ropey::Rope;
use tower_lsp::lsp_types::DidOpenTextDocumentParams;
use tracing::info;

use crate::diagnostics;
use crate::document::{DiagnosticCache, Document};
use crate::server::{Backend, cancel_pending_diagnostics};

pub async fn did_open(backend: &Backend, params: DidOpenTextDocumentParams) {
    let uri = params.text_document.uri;
    let text = params.text_document.text;
    let version = params.text_document.version;
    info!("did_open: {uri}");

    // If this URI already had a pending debounced publish, supersede it.
    cancel_pending_diagnostics(&backend.publish_handle, &uri);

    backend.document_map.insert(
        uri.clone(),
        Document {
            rope: Rope::from_str(&text),
            text: text.clone(),
            version,
            diagnostics: DiagnosticCache::default(),
            last_good_analysis: None,
        },
    );

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
