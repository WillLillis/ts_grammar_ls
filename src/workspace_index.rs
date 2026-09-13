//! Workspace-wide grammar dependency index.
//!
//! Walks every `.tsg` file under the configured workspace folder(s) at startup
//! and on `did_change_watched_files` events to extract `inherit(...)` /
//! `import(...)` paths. Results populate the `Backend.dependents` reverse
//! index, which `did_change_watched_files` and `rename_cross_file` already
//! consult for *open* dependents - extending it lets cross-file rename reach
//! closed files in the same project without opening them.

use std::path::{Path, PathBuf};

use dashmap::DashMap;
use rustc_hash::FxHashSet;
use tower_lsp::lsp_types::Url;

use crate::analysis;

/// Bound the recursive walk to avoid pathological trees (symlink loops,
/// runaway nesting). 32 levels is well beyond any realistic project layout.
const MAX_WALK_DEPTH: usize = 32;

/// Walk `root` recursively for `.tsg` files. Skips hidden directories
/// (anything starting with `.`) so we don't descend into `.git`, `.venv`,
/// etc. Returns paths in iteration order; canonicalization happens later
/// when we resolve deps.
pub fn discover_tsg_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            // Skip hidden dirs.
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }
            walk(&path, depth + 1, out);
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some("tsg") {
            out.push(path);
        }
    }
}

/// Read `path` from disk, run a cheap lex+parse to extract its inherit/import
/// dependencies, then add this file as a dependent of each. Replaces any
/// previous entry for `path`'s URI in the reverse index by passing the
/// previously-tracked deps in `prev_deps`.
///
/// Returns the new dep list so the caller can store it for a future diff.
pub fn index_file(
    dependents: &DashMap<PathBuf, FxHashSet<Url>>,
    path: &Path,
    prev_deps: &[PathBuf],
) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        // Unreadable - clear any previous entries we had for it.
        if let Ok(uri) = Url::from_file_path(path) {
            for dep in prev_deps {
                if let Some(mut set) = dependents.get_mut(dep) {
                    set.remove(&uri);
                }
            }
        }
        return Vec::new();
    };
    let new_deps = analysis::extract_deps(&text, path);
    let Ok(uri) = Url::from_file_path(path) else {
        return new_deps;
    };

    let prev: FxHashSet<&PathBuf> = prev_deps.iter().collect();
    let new: FxHashSet<&PathBuf> = new_deps.iter().collect();
    for dropped in prev.difference(&new) {
        if let Some(mut set) = dependents.get_mut(*dropped) {
            set.remove(&uri);
        }
    }
    for added in new.difference(&prev) {
        dependents
            .entry((*added).clone())
            .or_default()
            .insert(uri.clone());
    }
    new_deps
}

/// Drop the URI for `path` from every entry in `dependents` listed in
/// `prev_deps`. Called when a file is deleted from disk.
pub fn drop_file(
    dependents: &DashMap<PathBuf, FxHashSet<Url>>,
    path: &Path,
    prev_deps: &[PathBuf],
) {
    let Ok(uri) = Url::from_file_path(path) else {
        return;
    };
    for dep in prev_deps {
        if let Some(mut set) = dependents.get_mut(dep) {
            set.remove(&uri);
        }
    }
}
