use std::sync::Arc;

use tower_lsp::lsp_types::DidChangeTextDocumentParams;

use crate::diagnostics;
use crate::server::{Backend, cancel_pending_diagnostics};

#[allow(clippy::unused_async, reason = "required by LanguageServer trait")]
pub async fn did_change(backend: &Backend, params: DidChangeTextDocumentParams) {
    let uri = params.text_document.uri;
    let version = params.text_document.version;

    // We use FULL sync, so there's exactly one change with the full text.
    let Some(change) = params.content_changes.into_iter().next() else {
        return;
    };
    let text = change.text;

    if let Some(mut doc) = backend.document_map.get_mut(&uri) {
        doc.text.clone_from(&text);
        doc.rope = ropey::Rope::from_str(&text);
        doc.version = version;
    }

    // Kill any in-flight generate-check from a prior save/open of THIS file -
    // it's running on stale text now. Other files' generate-checks are left alone.
    diagnostics::kill_generate_child(&backend.generate_child, &uri);

    // Cancel any in-flight publish task; this one supersedes it.
    cancel_pending_diagnostics(&backend.publish_handle, &uri);

    let client = backend.client.clone();
    let document_map = Arc::clone(&backend.document_map);
    let generate_child = Arc::clone(&backend.generate_child);
    let dependents = Arc::clone(&backend.dependents);
    let uri_clone = uri.clone();

    // Typing path: DSL diagnostics only, off the LSP request task so a slow
    // publish doesn't head-of-line-block subsequent requests. Generate-check
    // is reserved for save/open since it can take many seconds. The handle
    // is tracked so a later did_change/did_save/did_open can cancel any
    // still-in-flight publish and supersede it.
    let handle = tokio::spawn(async move {
        diagnostics::run_and_publish(
            &client,
            &document_map,
            &generate_child,
            &dependents,
            false,
            uri_clone,
            text,
            version,
        )
        .await;
    });

    backend.publish_handle.insert(uri, handle);
}
