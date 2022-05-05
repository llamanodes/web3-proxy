use argh::FromArgs;
use governor::clock::QuantaClock;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::connection::Web3Connection;
use crate::Web3ProxyApp;

#[derive(FromArgs)]
/// Reach new heights.
pub struct CliConfig {
    /// what port the proxy should listen on
    #[argh(option, default = "8445")]
    pub listen_port: u16,

    /// what port the proxy should listen on
    // TODO: use flags for the config path  "./data/config/example.toml"
    #[argh(option, default = "\"./data/config/example.toml\".to_string()")]
    pub rpc_config_path: String,
}

#[derive(Deserialize)]
pub struct RpcConfig {
    // BTreeMap so that iterating keeps the same order
    pub balanced_rpc_tiers: BTreeMap<String, HashMap<String, Web3ConnectionConfig>>,
    pub private_rpcs: Option<HashMap<String, Web3ConnectionConfig>>,
}

#[derive(Deserialize)]
pub struct Web3ConnectionConfig {
    url: String,
    soft_limit: u32,
    hard_limit: Option<u32>,
}

impl RpcConfig {
    pub async fn try_build(self) -> anyhow::Result<Web3ProxyApp> {
        let balanced_rpc_tiers = self
            .balanced_rpc_tiers
            .into_values()
            .map(|x| x.into_values().collect())
            .collect();

        let private_rpcs = if let Some(private_rpcs) = self.private_rpcs {
            private_rpcs.into_values().collect()
        } else {
            vec![]
        };

        Web3ProxyApp::try_new(balanced_rpc_tiers, private_rpcs).await
    }
}

impl Web3ConnectionConfig {
    pub async fn try_build(
        self,
        clock: &QuantaClock,
        http_client: Option<reqwest::Client>,
    ) -> anyhow::Result<Arc<Web3Connection>> {
        Web3Connection::try_new(
            self.url,
            http_client,
            self.hard_limit,
            Some(clock),
            self.soft_limit,
        )
        .await
        .map(Arc::new)
    }
}
