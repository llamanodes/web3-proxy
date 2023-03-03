#![forbid(unsafe_code)]
use argh::FromArgs;
use futures::StreamExt;
use log::{error, info, warn};
use num::Zero;
use std::path::PathBuf;
use std::time::Duration;
use std::{fs, thread};
use tokio::sync::broadcast;
use web3_proxy::app::{flatten_handle, flatten_handles, Web3ProxyApp};
use web3_proxy::config::TopConfig;
use web3_proxy::{frontend, metrics_frontend};

/// start the main proxy daemon
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
    pub async fn main(
        self,
        top_config: TopConfig,
        top_config_path: PathBuf,
        num_workers: usize,
    ) -> anyhow::Result<()> {
        let (shutdown_sender, _) = broadcast::channel(1);

        // TODO: i think there is a small race. if config_path changes

        run(
            top_config,
            Some(top_config_path),
            self.port,
            self.prometheus_port,
            num_workers,
            shutdown_sender,
        )
        .await
    }
}

async fn run(
    mut top_config: TopConfig,
    top_config_path: Option<PathBuf>,
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
    let mut shutdown_receiver = shutdown_sender.subscribe();

    // start the main app
    let mut spawned_app =
        Web3ProxyApp::spawn(top_config.clone(), num_workers, shutdown_sender.subscribe()).await?;

    // start thread for watching config
    if let Some(top_config_path) = top_config_path {
        let config_sender = spawned_app.new_top_config_sender;
        /*
        #[cfg(feature = "inotify")]
        {
            let mut inotify = Inotify::init().expect("Failed to initialize inotify");

            inotify
                .add_watch(top_config_path.clone(), WatchMask::MODIFY)
                .expect("Failed to add inotify watch on config");

            let mut buffer = [0u8; 4096];

            // TODO: exit the app if this handle exits
            thread::spawn(move || loop {
                // TODO: debounce

                let events = inotify
                    .read_events_blocking(&mut buffer)
                    .expect("Failed to read inotify events");

                for event in events {
                    if event.mask.contains(EventMask::MODIFY) {
                        info!("config changed");
                        match fs::read_to_string(&top_config_path) {
                            Ok(top_config) => match toml::from_str(&top_config) {
                                Ok(top_config) => {
                                    config_sender.send(top_config).unwrap();
                                }
                                Err(err) => {
                                    // TODO: panic?
                                    error!("Unable to parse config! {:#?}", err);
                                }
                            },
                            Err(err) => {
                                // TODO: panic?
                                error!("Unable to read config! {:#?}", err);
                            }
                        };
                    } else {
                        // TODO: is "MODIFY" enough, or do we want CLOSE_WRITE?
                        unimplemented!();
                    }
                }
            });
        }
        */
        // #[cfg(not(feature = "inotify"))]
        {
            thread::spawn(move || loop {
                match fs::read_to_string(&top_config_path) {
                    Ok(new_top_config) => match toml::from_str(&new_top_config) {
                        Ok(new_top_config) => {
                            if new_top_config != top_config {
                                top_config = new_top_config;
                                config_sender.send(top_config.clone()).unwrap();
                            }
                        }
                        Err(err) => {
                            // TODO: panic?
                            error!("Unable to parse config! {:#?}", err);
                        }
                    },
                    Err(err) => {
                        // TODO: panic?
                        error!("Unable to read config! {:#?}", err);
                    }
                }

                thread::sleep(Duration::from_secs(10));
            });
        }
    }

    // start the prometheus metrics port
    let prometheus_handle = tokio::spawn(metrics_frontend::serve(
        spawned_app.app.clone(),
        app_prometheus_port,
    ));

    // wait until the app has seen its first consensus head block
    // TODO: if backups were included, wait a little longer?
    let _ = spawned_app.app.head_block_receiver().changed().await;

    // start the frontend port
    let frontend_handle = tokio::spawn(frontend::serve(app_frontend_port, spawned_app.app.clone()));

    // if everything is working, these should all run forever
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
        config::{AppConfig, Web3RpcConfig},
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
                response_cache_max_bytes: 10_u64.pow(7),
                redirect_public_url: Some("example.com/".to_string()),
                redirect_rpc_key_url: Some("example.com/{{rpc_key_id}}".to_string()),
                ..Default::default()
            },
            balanced_rpcs: HashMap::from([
                (
                    "anvil".to_string(),
                    Web3RpcConfig {
                        http_url: Some(anvil.endpoint()),
                        soft_limit: 100,
                        tier: 0,
                        ..Default::default()
                    },
                ),
                (
                    "anvil_ws".to_string(),
                    Web3RpcConfig {
                        ws_url: Some(anvil.ws_endpoint()),
                        soft_limit: 100,
                        tier: 0,
                        ..Default::default()
                    },
                ),
                (
                    "anvil_both".to_string(),
                    Web3RpcConfig {
                        http_url: Some(anvil.endpoint()),
                        ws_url: Some(anvil.ws_endpoint()),
                        ..Default::default()
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
                    None,
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
