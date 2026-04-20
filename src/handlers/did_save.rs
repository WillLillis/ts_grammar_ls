use tower_lsp::lsp_types::DidSaveTextDocumentParams;
use tracing::info;

use crate::analysis::SourceVersion;
use crate::diagnostics;
use crate::server::Backend;

/// Check if any grammar cache entry has a stale disk mtime.
fn has_stale_grammar_cache(backend: &Backend) -> bool {
    backend.grammar_cache.iter().any(|entry| {
        let SourceVersion::Disk(cached_mtime) = &entry.version else {
            return false;
        };
        std::fs::metadata(entry.key())
            .and_then(|m| m.modified())
            .is_ok_and(|mtime| mtime != *cached_mtime)
    })
}

pub async fn did_save(backend: &Backend, params: DidSaveTextDocumentParams) {
    let uri = params.text_document.uri;
    info!("did_save: {uri}");

    let (text, version) = match backend.document_map.get_mut(&uri) {
        Some(mut doc) => {
            // If any cached external grammar has a stale mtime, clear the
            // analysis so the next get_analysis recomputes with fresh data.
            // This picks up changes from cross-file renames or external edits.
            if has_stale_grammar_cache(backend) {
                doc.analysis = None;
            }
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
