use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType};
use tracing::info;

use crate::diagnostics;
use crate::server::Backend;
use crate::workspace_index;

/// Receive on-disk change notifications for `.tsg` files.
///
/// For each notified URI we do two things:
///
///  1. Update the closed-file dep index. Created or changed files get
///     re-scanned for `inherit`/`import` paths; deleted files have their
///     entries dropped. (Open files maintain their dep list via
///     `get_analysis`; we deliberately don't touch them here.)
///  2. Republish DSL diagnostics for every open document whose latest
///     analysis depended on the notified path.
///
/// Generate-check is not triggered here - watcher events can fire on every
/// save in the workspace; the user gets a fresh check on their next save of
/// the affected file itself.
pub async fn did_change_watched_files(backend: &Backend, params: DidChangeWatchedFilesParams) {
    for change in params.changes {
        let Ok(raw_path) = change.uri.to_file_path() else {
            continue;
        };
        // Deleted files won't canonicalize - use the raw path for cleanup.
        let path_for_index = match change.typ {
            FileChangeType::DELETED => raw_path.clone(),
            _ => match dunce::canonicalize(&raw_path) {
                Ok(p) => p,
                Err(_) => continue,
            },
        };
        info!(
            "did_change_watched_files: {} ({:?})",
            path_for_index.display(),
            change.typ
        );

        // Skip the closed-file index update if this file is currently open;
        // its Document.deps is authoritative.
        let is_open = tower_lsp::lsp_types::Url::from_file_path(&path_for_index)
            .ok()
            .is_some_and(|u| backend.document_map.contains_key(&u));

        if !is_open {
            update_closed_index(backend, &path_for_index, change.typ);
        }

        republish_dependents(backend, &path_for_index).await;
    }
}

fn update_closed_index(backend: &Backend, path: &std::path::Path, change_type: FileChangeType) {
    let prev = backend
        .closed_file_deps
        .get(path)
        .map(|v| v.clone())
        .unwrap_or_default();
    match change_type {
        FileChangeType::CREATED | FileChangeType::CHANGED => {
            let new_deps = workspace_index::index_file(&backend.dependents, path, &prev);
            backend
                .closed_file_deps
                .insert(path.to_path_buf(), new_deps);
        }
        FileChangeType::DELETED => {
            workspace_index::drop_file(&backend.dependents, path, &prev);
            backend.closed_file_deps.remove(path);
        }
        _ => {}
    }
}

async fn republish_dependents(backend: &Backend, canonical_path: &std::path::Path) {
    // Snapshot the dependent set under the dashmap guard, then drop it
    // before awaiting any work that re-enters document_map / dependents.
    let dep_uris: Vec<_> = backend
        .dependents
        .get(canonical_path)
        .map(|set| set.iter().cloned().collect())
        .unwrap_or_default();

    for dep_uri in dep_uris {
        let Some((text, version)) = backend
            .document_map
            .get(&dep_uri)
            .map(|d| (d.text.clone(), d.version))
        else {
            continue;
        };
        // run_and_publish itself fans out to that file's own dependents,
        // so a single notification can refresh a transitive chain (A -> B -> C).
        diagnostics::run_and_publish(
            &backend.client,
            &backend.document_map,
            &backend.generate_child,
            &backend.dependents,
            false,
            dep_uri,
            text,
            version,
        )
        .await;
    }
}
