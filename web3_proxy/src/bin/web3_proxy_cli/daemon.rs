#![forbid(unsafe_code)]

use argh::FromArgs;
use futures::StreamExt;
use log::{error, info, warn};
use num::Zero;
use tokio::sync::broadcast;
use web3_proxy::app::{flatten_handle, flatten_handles, Web3ProxyApp};
use web3_proxy::config::TopConfig;
use web3_proxy::{frontend, metrics_frontend};

/// count requests
#[derive(FromArgs, PartialEq, Debug, Eq)]
#[argh(subcommand, name = "proxyd")]
pub struct ProxydSubCommand {
    /// path to a toml of rpc servers
    /// what port the proxy should listen on
    #[argh(option, default = "8544")]
    pub port: u16,

    /// what port the proxy should expose prometheus stats on
    #[argh(option, default = "8543")]
    pub prometheus_port: u16,
}

impl ProxydSubCommand {
    pub async fn main(self, top_config: TopConfig, num_workers: usize) -> anyhow::Result<()> {
        let (shutdown_sender, _) = broadcast::channel(1);

        run(
            top_config,
            self.port,
            self.prometheus_port,
            num_workers,
            shutdown_sender,
        )
        .await
    }
}

async fn run(
    top_config: TopConfig,
    frontend_port: u16,
    prometheus_port: u16,
    num_workers: usize,
    shutdown_sender: broadcast::Sender<()>,
) -> anyhow::Result<()> {
    // tokio has code for catching ctrl+c so we use that
    // this shutdown sender is currently only used in tests, but we might make a /shutdown endpoint or something
    // we do not need this receiver. new receivers are made by `shutdown_sender.subscribe()`

    let app_frontend_port = frontend_port;
    let app_prometheus_port = prometheus_port;

    // start the main app
    let mut spawned_app =
        Web3ProxyApp::spawn(top_config, num_workers, shutdown_sender.subscribe()).await?;

    let frontend_handle = tokio::spawn(frontend::serve(app_frontend_port, spawned_app.app.clone()));

    // TODO: should we put this in a dedicated thread?
    let prometheus_handle = tokio::spawn(metrics_frontend::serve(
        spawned_app.app.clone(),
        app_prometheus_port,
    ));

    let mut shutdown_receiver = shutdown_sender.subscribe();

    // if everything is working, these should both run forever
    tokio::select! {
        x = flatten_handles(spawned_app.app_handles) => {
            match x {
                Ok(_) => info!("app_handle exited"),
                Err(e) => {
                    return Err(e);
                }
            }
        }
        x = flatten_handle(frontend_handle) => {
            match x {
                Ok(_) => info!("frontend exited"),
                Err(e) => {
                    return Err(e);
                }
            }
        }
        x = flatten_handle(prometheus_handle) => {
            match x {
                Ok(_) => info!("prometheus exited"),
                Err(e) => {
                    return Err(e);
                }
            }
        }
        x = tokio::signal::ctrl_c() => {
            match x {
                Ok(_) => info!("quiting from ctrl-c"),
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
        x = shutdown_receiver.recv() => {
            match x {
                Ok(_) => info!("quiting from shutdown receiver"),
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    };

    // one of the handles stopped. send a value so the others know to shut down
    if let Err(err) = shutdown_sender.send(()) {
        warn!("shutdown sender err={:?}", err);
    };

    // wait for things like saving stats to the database to complete
    info!("waiting on important background tasks");
    let mut background_errors = 0;
    while let Some(x) = spawned_app.background_handles.next().await {
        match x {
            Err(e) => {
                error!("{:?}", e);
                background_errors += 1;
            }
            Ok(Err(e)) => {
                error!("{:?}", e);
                background_errors += 1;
            }
            Ok(Ok(_)) => continue,
        }
    }

    if background_errors.is_zero() {
        info!("finished");
        Ok(())
    } else {
        // TODO: collect instead?
        Err(anyhow::anyhow!("finished with errors!"))
    }
}

#[cfg(test)]
mod tests {
    use ethers::{
        prelude::{Http, Provider, U256},
        utils::Anvil,
    };
    use hashbrown::HashMap;
    use std::env;

    use web3_proxy::{
        config::{AppConfig, Web3ConnectionConfig},
        rpcs::blockchain::ArcBlock,
    };

    use super::*;

    #[tokio::test]
    async fn it_works() {
        // TODO: move basic setup into a test fixture
        let path = env::var("PATH").unwrap();

        println!("path: {}", path);

        // TODO: how should we handle logs in this?
        // TODO: option for super verbose logs
        std::env::set_var("RUST_LOG", "info,web3_proxy=debug");

        let _ = env_logger::builder().is_test(true).try_init();

        let anvil = Anvil::new().spawn();

        println!("Anvil running at `{}`", anvil.endpoint());

        let anvil_provider = Provider::<Http>::try_from(anvil.endpoint()).unwrap();

        // mine a block because my code doesn't like being on block 0
        // TODO: make block 0 okay? is it okay now?
        let _: U256 = anvil_provider
            .request("evm_mine", None::<()>)
            .await
            .unwrap();

        // make a test TopConfig
        // TODO: load TopConfig from a file? CliConfig could have `cli_config.load_top_config`. would need to inject our endpoint ports
        let top_config = TopConfig {
            app: AppConfig {
                chain_id: 31337,
                default_user_max_requests_per_period: Some(6_000_000),
                min_sum_soft_limit: 1,
                min_synced_rpcs: 1,
                public_requests_per_period: Some(1_000_000),
                response_cache_max_bytes: 10_usize.pow(7),
                redirect_public_url: Some("example.com/".to_string()),
                redirect_rpc_key_url: Some("example.com/{{rpc_key_id}}".to_string()),
                ..Default::default()
            },
            balanced_rpcs: HashMap::from([
                (
                    "anvil".to_string(),
                    Web3ConnectionConfig {
                        disabled: false,
                        display_name: None,
                        url: anvil.endpoint(),
                        backup: None,
                        block_data_limit: None,
                        soft_limit: 100,
                        hard_limit: None,
                        tier: 0,
                        subscribe_txs: Some(false),
                        extra: Default::default(),
                    },
                ),
                (
                    "anvil_ws".to_string(),
                    Web3ConnectionConfig {
                        disabled: false,
                        display_name: None,
                        url: anvil.ws_endpoint(),
                        backup: None,
                        block_data_limit: None,
                        soft_limit: 100,
                        hard_limit: None,
                        tier: 0,
                        subscribe_txs: Some(false),
                        extra: Default::default(),
                    },
                ),
            ]),
            private_rpcs: None,
            extra: Default::default(),
        };

        let (shutdown_sender, _) = broadcast::channel(1);

        // spawn another thread for running the app
        // TODO: allow launching into the local tokio runtime instead of creating a new one?
        let handle = {
            let shutdown_sender = shutdown_sender.clone();

            let frontend_port = 0;
            let prometheus_port = 0;

            tokio::spawn(async move {
                run(
                    top_config,
                    frontend_port,
                    prometheus_port,
                    2,
                    shutdown_sender,
                )
                .await
            })
        };

        // TODO: do something to the node. query latest block, mine another block, query again
        let proxy_provider = Provider::<Http>::try_from(anvil.endpoint()).unwrap();

        let anvil_result = anvil_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap()
            .unwrap();
        let proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(anvil_result, proxy_result);

        let first_block_num = anvil_result.number.unwrap();

        let _: U256 = anvil_provider
            .request("evm_mine", None::<()>)
            .await
            .unwrap();

        let anvil_result = anvil_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap()
            .unwrap();
        let proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(anvil_result, proxy_result);

        let second_block_num = anvil_result.number.unwrap();

        assert_eq!(first_block_num, second_block_num - 1);

        // tell the test app to shut down
        shutdown_sender.send(()).unwrap();

        println!("waiting for shutdown...");
        // TODO: panic if a timeout is reached
        handle.await.unwrap().unwrap();
    }
}
