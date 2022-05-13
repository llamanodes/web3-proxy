use argh::FromArgs;
use governor::clock::QuantaClock;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::connection::Web3Connection;
use crate::Web3ProxyApp;

#[derive(FromArgs)]
/// Web3-proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.
pub struct CliConfig {
    /// what port the proxy should listen on
    #[argh(option, default = "8544")]
    pub listen_port: u16,

    /// path to a toml of rpc servers
    #[argh(option, default = "\"./config/example.toml\".to_string()")]
    pub rpc_config_path: String,
}

#[derive(Deserialize)]
pub struct RpcConfig {
    pub shared: RpcSharedConfig,
    pub balanced_rpcs: HashMap<String, Web3ConnectionConfig>,
    pub private_rpcs: Option<HashMap<String, Web3ConnectionConfig>>,
}

/// shared configuration between Web3Connections
#[derive(Deserialize)]
pub struct RpcSharedConfig {
    /// TODO: what type for chain_id? TODO: this isn't at the right level. this is inside a "Config"
    pub chain_id: usize,
}

#[derive(Deserialize)]
pub struct Web3ConnectionConfig {
    url: String,
    soft_limit: u32,
    hard_limit: Option<u32>,
}

impl RpcConfig {
    /// Create a Web3ProxyApp from config
    pub async fn try_build(self) -> anyhow::Result<Web3ProxyApp> {
        let balanced_rpcs = self.balanced_rpcs.into_values().collect();

        let private_rpcs = if let Some(private_rpcs) = self.private_rpcs {
            private_rpcs.into_values().collect()
        } else {
            vec![]
        };

        Web3ProxyApp::try_new(self.shared.chain_id, balanced_rpcs, private_rpcs).await
    }
}

impl Web3ConnectionConfig {
    /// Create a Web3Connection from config
    pub async fn try_build(
        self,
        clock: &QuantaClock,
        chain_id: usize,
        http_client: Option<reqwest::Client>,
    ) -> anyhow::Result<Arc<Web3Connection>> {
        Web3Connection::try_new(
            chain_id,
            self.url,
            http_client,
            self.hard_limit,
            clock,
            self.soft_limit,
        )
        .await
    }
}
