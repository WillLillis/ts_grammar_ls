use std::path::Path;

use crate::config::FormattingConfig;

/// Format a `.tsg` source file. Returns `None` if the source can't be parsed.
///
/// TODO: Rewrite after core AST refactor.
#[must_use]
pub fn format(_source: &str, _path: &Path, _config: &FormattingConfig) -> Option<String> {
    None
}
