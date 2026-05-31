use ropey::Rope;
use tower_lsp::lsp_types::DidOpenTextDocumentParams;
use tracing::info;

use crate::diagnostics;
use crate::document::Document;
use crate::server::{Backend, cancel_pending_diagnostics, drop_dependents};

pub async fn did_open(backend: &Backend, params: DidOpenTextDocumentParams) {
    let uri = params.text_document.uri;
    let text = params.text_document.text;
    let version = params.text_document.version;
    info!("did_open: {uri}");

    // REPL input buffers: skip the `.tsg` analysis path, route to the
    // session handler so it can stash text + kick a compile if needed.
    if let Some(input_uri) = crate::repl::ReplInputUri::try_from_uri(&uri) {
        crate::handlers::repl::handle_repl_change(backend, &input_uri, &text);
        return;
    }
    // Tree-side buffers: skip the analysis path. Their content is
    // rendered CST, not grammar source; the user-facing settings
    // (rule, format) are surfaced on the input buffer.
    if crate::repl::ReplTreeUri::try_from_uri(&uri).is_some() {
        return;
    }

    // If this URI already had a pending debounced publish, supersede it.
    cancel_pending_diagnostics(&backend.publish_handle, &uri);

    // If this file was previously tracked as closed-on-disk, drop those
    // index entries; `Document.deps` will become the canonical source once
    // analyze runs. Otherwise the URI shows up twice in `dependents`
    // (harmless but messy) and `closed_file_deps` keeps a stale list.
    if let Ok(path) = uri.to_file_path()
        && let Ok(canonical) = dunce::canonicalize(&path)
        && let Some((_, prev_deps)) = backend.closed_file_deps.remove(&canonical)
    {
        drop_dependents(&backend.dependents, &prev_deps, &uri);
    }

    backend.document_map.insert(
        uri.clone(),
        Document {
            rope: Rope::from_str(&text),
            text: text.clone(),
            version,
            dsl_diagnostics: Vec::new(),
            generate_diagnostics: Vec::new(),
            last_good_analysis: None,
            deps: Vec::new(),
        },
    );

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
