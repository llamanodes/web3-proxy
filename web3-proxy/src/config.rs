use argh::FromArgs;
use ethers::prelude::{Block, TxHash};
use futures::Future;
use serde::Deserialize;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use crate::app::AnyhowJoinHandle;
use crate::connection::Web3Connection;
use crate::Web3ProxyApp;

#[derive(Debug, FromArgs)]
/// Web3-proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.
pub struct CliConfig {
    /// what port the proxy should listen on
    #[argh(option, default = "8544")]
    pub port: u16,

    /// number of worker threads. Defaults to the number of logical processors
    #[argh(option, default = "0")]
    pub workers: usize,

    /// path to a toml of rpc servers
    #[argh(option, default = "\"./config/development.toml\".to_string()")]
    pub config: String,
}

#[derive(Debug, Deserialize)]
pub struct RpcConfig {
    pub shared: RpcSharedConfig,
    pub balanced_rpcs: HashMap<String, Web3ConnectionConfig>,
    pub private_rpcs: Option<HashMap<String, Web3ConnectionConfig>>,
}

/// shared configuration between Web3Connections
#[derive(Debug, Deserialize)]
pub struct RpcSharedConfig {
    /// TODO: what type for chain_id? TODO: this isn't at the right level. this is inside a "Config"
    pub chain_id: usize,
    pub rate_limit_redis: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Web3ConnectionConfig {
    url: String,
    soft_limit: u32,
    hard_limit: Option<u32>,
}

impl RpcConfig {
    /// Create a Web3ProxyApp from config
    // #[instrument(name = "try_build_RpcConfig", skip_all)]
    pub async fn spawn(
        self,
    ) -> anyhow::Result<(
        Arc<Web3ProxyApp>,
        Pin<Box<dyn Future<Output = anyhow::Result<()>>>>,
    )> {
        let balanced_rpcs = self.balanced_rpcs.into_values().collect();

        let private_rpcs = if let Some(private_rpcs) = self.private_rpcs {
            private_rpcs.into_values().collect()
        } else {
            vec![]
        };

        Web3ProxyApp::spawn(
            self.shared.chain_id,
            self.shared.rate_limit_redis,
            balanced_rpcs,
            private_rpcs,
        )
        .await
    }
}

impl Web3ConnectionConfig {
    /// Create a Web3Connection from config
    // #[instrument(name = "try_build_Web3ConnectionConfig", skip_all)]
    pub async fn spawn(
        self,
        rate_limiter: Option<&redis_cell_client::MultiplexedConnection>,
        chain_id: usize,
        http_client: Option<&reqwest::Client>,
        block_sender: Option<flume::Sender<(Block<TxHash>, Arc<Web3Connection>)>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Web3Connection>)>>,
        reconnect: bool,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        let hard_rate_limit = self.hard_limit.map(|x| (x, rate_limiter.unwrap()));

        Web3Connection::spawn(
            chain_id,
            self.url,
            http_client,
            hard_rate_limit,
            self.soft_limit,
            block_sender,
            tx_id_sender,
            reconnect,
        )
        .await
    }
}
