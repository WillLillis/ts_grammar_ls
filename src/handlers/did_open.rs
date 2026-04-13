use ropey::Rope;
use tower_lsp::lsp_types::DidOpenTextDocumentParams;
use tracing::info;

use crate::diagnostics;
use crate::document::{DiagnosticCache, Document};
use crate::server::Backend;

pub async fn did_open(backend: &Backend, params: DidOpenTextDocumentParams) {
    let uri = params.text_document.uri;
    let text = params.text_document.text;
    let version = params.text_document.version;
    info!("did_open: {uri}");

    backend.document_map.insert(
        uri.clone(),
        Document {
            rope: Rope::from_str(&text),
            text: text.clone(),
            version,
            diagnostics: DiagnosticCache::default(),
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
