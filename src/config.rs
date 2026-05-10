use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CONFIG_FILE_NAME: &str = ".ts_grammar_ls.toml";

/// Top-level configuration for the language server.
///
/// All fields are optional - omitted values use sensible defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Options related to diagnostics.
    pub diagnostics: DiagnosticConfig,
    /// Options related to formatting.
    pub formatting: FormattingConfig,
}

/// Diagnostic-related configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiagnosticConfig {
    /// Whether to run the full parser generation pipeline for additional
    /// diagnostics beyond what the DSL pipeline catches (e.g. unresolved
    /// conflicts, invalid token rules). This spawns a subprocess and can
    /// take several seconds for large grammars. Default: `true`.
    pub generate_diagnostics: bool,
}

impl Default for DiagnosticConfig {
    fn default() -> Self {
        Self {
            generate_diagnostics: true,
        }
    }
}

/// Formatting-related configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FormattingConfig {
    /// Maximum line width before breaking into multiple lines. Default: 100.
    pub max_line_width: usize,
    /// Number of spaces per indentation level. Default: 4.
    pub indent_width: usize,
    /// Add trailing commas in lists, seq, choice, etc. Default: true.
    pub trailing_commas: bool,
}

impl Default for FormattingConfig {
    fn default() -> Self {
        Self {
            max_line_width: 100,
            indent_width: 4,
            trailing_commas: true,
        }
    }
}

/// Load configuration with the following priority:
/// 1. Explicit path (from CLI `--config` arg)
/// 2. Workspace root (`.ts_grammar_ls.toml`)
/// 3. User config (`$XDG_CONFIG_HOME/ts_grammar_ls/.ts_grammar_ls.toml`)
/// 4. Default values
#[must_use]
pub fn load_config(explicit_path: Option<&Path>, workspace_root: Option<&Path>) -> Config {
    if let Some(path) = explicit_path
        && let Some(config) = read_config(path)
    {
        return config;
    }

    if let Some(root) = workspace_root {
        let path = root.join(CONFIG_FILE_NAME);
        if let Some(config) = read_config(&path) {
            return config;
        }
    }

    if let Ok(strategy) = etcetera::choose_base_strategy() {
        use etcetera::BaseStrategy;
        let path = strategy
            .config_dir()
            .join("ts_grammar_ls")
            .join(CONFIG_FILE_NAME);
        if let Some(config) = read_config(&path) {
            return config;
        }
    }

    Config::default()
}

fn read_config(path: &Path) -> Option<Config> {
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

/// Resolve the workspace root from LSP initialization params.
#[must_use]
pub fn workspace_root_from_params(
    params: &tower_lsp::lsp_types::InitializeParams,
) -> Option<PathBuf> {
    workspace_roots_from_params(params).into_iter().next()
}

/// All workspace roots the client advertised, in order. Multi-root clients
/// (VS Code with multiple folders) get all of them; single-root clients fall
/// back to `rootUri`.
#[must_use]
pub fn workspace_roots_from_params(
    params: &tower_lsp::lsp_types::InitializeParams,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(folders) = &params.workspace_folders {
        for folder in folders {
            if let Ok(path) = folder.uri.to_file_path() {
                out.push(path);
            }
        }
    }
    #[allow(deprecated)]
    if out.is_empty()
        && let Some(uri) = &params.root_uri
        && let Ok(path) = uri.to_file_path()
    {
        out.push(path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_enables_generate() {
        let config = Config::default();
        assert!(config.diagnostics.generate_diagnostics);
    }

    #[test]
    fn load_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "[diagnostics]\ngenerate_diagnostics = false\n").unwrap();

        let config = read_config(&path).unwrap();
        assert!(!config.diagnostics.generate_diagnostics);
    }

    #[test]
    fn load_partial_toml_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "[formatting]\n").unwrap();

        let config = read_config(&path).unwrap();
        // diagnostics section was omitted - should use defaults.
        assert!(config.diagnostics.generate_diagnostics);
    }

    #[test]
    fn load_empty_toml_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "").unwrap();

        let config = read_config(&path).unwrap();
        assert!(config.diagnostics.generate_diagnostics);
    }

    #[test]
    fn load_config_explicit_path_takes_priority() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let explicit_dir = tempfile::tempdir().unwrap();

        // Workspace config: generate enabled.
        std::fs::write(
            workspace_dir.path().join(CONFIG_FILE_NAME),
            "[diagnostics]\ngenerate_diagnostics = true\n",
        )
        .unwrap();

        // Explicit config: generate disabled.
        let explicit_path = explicit_dir.path().join("custom.toml");
        std::fs::write(
            &explicit_path,
            "[diagnostics]\ngenerate_diagnostics = false\n",
        )
        .unwrap();

        let config = load_config(Some(&explicit_path), Some(workspace_dir.path()));
        assert!(!config.diagnostics.generate_diagnostics);
    }

    #[test]
    fn load_config_workspace_before_default() {
        let workspace_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace_dir.path().join(CONFIG_FILE_NAME),
            "[diagnostics]\ngenerate_diagnostics = false\n",
        )
        .unwrap();

        let config = load_config(None, Some(workspace_dir.path()));
        assert!(!config.diagnostics.generate_diagnostics);
    }

    #[test]
    fn load_config_falls_back_to_default() {
        let config = load_config(None, None);
        assert!(config.diagnostics.generate_diagnostics);
    }

    #[test]
    fn invalid_toml_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "not valid {{{{ toml").unwrap();

        assert!(read_config(&path).is_none());
    }

    #[test]
    fn nonexistent_path_returns_none() {
        assert!(read_config(Path::new("/nonexistent/path/config.toml")).is_none());
    }
}
