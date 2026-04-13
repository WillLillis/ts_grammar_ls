use std::sync::Arc;

use tower_lsp::lsp_types::DidChangeTextDocumentParams;

use crate::diagnostics;
use crate::server::Backend;

const DEBOUNCE_MS: u64 = 200;

pub async fn did_change(backend: &Backend, params: DidChangeTextDocumentParams) {
    let uri = params.text_document.uri;
    let version = params.text_document.version;

    // We use FULL sync, so there's exactly one change with the full text.
    let Some(change) = params.content_changes.into_iter().next() else {
        return;
    };
    let text = change.text;

    // Update the document.
    if let Some(mut doc) = backend.document_map.get_mut(&uri) {
        doc.text.clone_from(&text);
        doc.rope = ropey::Rope::from_str(&text);
        doc.version = version;
    }

    // Kill any running generate-check subprocess - the input has changed.
    diagnostics::kill_generate_child(&backend.generate_child, None).await;

    // Record this version for debounce staleness checks.
    backend.debounce_version.insert(uri.clone(), version);

    // Spawn a debounced diagnostic task.
    let client = backend.client.clone();
    let document_map = Arc::clone(&backend.document_map);
    let debounce_version = Arc::clone(&backend.debounce_version);
    let generate_child = Arc::clone(&backend.generate_child);
    let generate_enabled = backend.config.read().await.diagnostics.generate_diagnostics;
    let uri_clone = uri.clone();

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(DEBOUNCE_MS)).await;

        // Check if a newer version has been dispatched since we started waiting.
        let current = debounce_version.get(&uri_clone).map_or(0, |v| *v);
        if current != version {
            return; // Stale, a newer change is pending.
        }

        diagnostics::run_and_publish(
            &client,
            &document_map,
            &generate_child,
            generate_enabled,
            uri_clone,
            text,
            version,
        )
        .await;
    });
}
