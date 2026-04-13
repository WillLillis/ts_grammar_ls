use tower_lsp::lsp_types::DidChangeConfigurationParams;
use tracing::info;

use crate::config::Config;
use crate::server::Backend;

pub async fn did_change_configuration(backend: &Backend, params: DidChangeConfigurationParams) {
    if let Ok(config) = serde_json::from_value::<Config>(params.settings) {
        info!("configuration updated");
        *backend.config.write().await = config;
    }
}
