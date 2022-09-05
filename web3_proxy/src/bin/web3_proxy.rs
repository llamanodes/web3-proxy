//! Web3_proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.
//!
//! Signed transactions (eth_sendRawTransaction) are sent in parallel to the configured private RPCs (eden, ethermine, flashbots, etc.).
//!
//! All other requests are sent to an RPC server on the latest block (alchemy, moralis, rivet, your own node, or one of many other providers).
//! If multiple servers are in sync, the fastest server is prioritized. Since the fastest server is most likely to serve requests, slow servers are unlikely to ever get any requests.

//#![warn(missing_docs)]
#![forbid(unsafe_code)]

use parking_lot::deadlock;
use std::fs;
use std::sync::atomic::{self, AtomicUsize};
use std::thread;
use tokio::runtime;
use tokio::time::Duration;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;
use web3_proxy::app::{flatten_handle, Web3ProxyApp};
use web3_proxy::config::{CliConfig, TopConfig};
use web3_proxy::frontend;
use web3_proxy::stats::AppStatsRegistry;

fn run(
    shutdown_receiver: flume::Receiver<()>,
    cli_config: CliConfig,
    top_config: TopConfig,
) -> anyhow::Result<()> {
    debug!(?cli_config, ?top_config);

    // spawn a thread for deadlock detection
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(10));
        let deadlocks = deadlock::check_deadlock();
        if deadlocks.is_empty() {
            continue;
        }

        println!("{} deadlocks detected", deadlocks.len());
        for (i, threads) in deadlocks.iter().enumerate() {
            println!("Deadlock #{}", i);
            for t in threads {
                println!("Thread Id {:#?}", t.thread_id());
                println!("{:#?}", t.backtrace());
            }
        }
    });

    // set up tokio's async runtime
    let mut rt_builder = runtime::Builder::new_multi_thread();

    let chain_id = top_config.app.chain_id;
    rt_builder.enable_all().thread_name_fn(move || {
        static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
        // TODO: what ordering? i think we want seqcst so that these all happen in order, but that might be stricter than we really need
        let worker_id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
        // TODO: i think these max at 15 characters
        format!("web3-{}-{}", chain_id, worker_id)
    });

    if cli_config.workers > 0 {
        rt_builder.worker_threads(cli_config.workers);
    }

    // start tokio's async runtime
    let rt = rt_builder.build()?;

    let num_workers = rt.metrics().num_workers();
    debug!(?num_workers);

    rt.block_on(async {
        let app_stats_registry = AppStatsRegistry::new();

        let app_stats = app_stats_registry.stats.clone();

        let app_frontend_port = cli_config.port;
        let app_prometheus_port = cli_config.prometheus_port;

        let (app, app_handle) = Web3ProxyApp::spawn(app_stats, top_config).await?;

        let frontend_handle = tokio::spawn(frontend::serve(app_frontend_port, app));

        let prometheus_handle = tokio::spawn(app_stats_registry.serve(app_prometheus_port));

        // if everything is working, these should both run forever
        // TODO: try_join these instead? use signal_shutdown here?
        tokio::select! {
            x = app_handle => {
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
            _ = shutdown_receiver.recv_async() => {
                // TODO: think more about this. we need some way for tests to tell the app to stop
                info!("received shutdown signal");

                // TODO: wait for outstanding requests to complete. graceful shutdown will make our users happier

                return Ok(())
            }
        };

        Ok(())
    })
}

fn main() -> anyhow::Result<()> {
    // if RUST_LOG isn't set, configure a default
    // TODO: is there a better way to do this?
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info,web3_proxy=debug");
    }

    // install global collector configured based on RUST_LOG env var.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .compact()
        .init();

    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    // initial configuration from flags
    let cli_config: CliConfig = argh::from_env();

    // advanced configuration is on disk
    info!("Loading config @ {}", cli_config.config);
    let top_config: String = fs::read_to_string(cli_config.config.clone())?;
    let top_config: TopConfig = toml::from_str(&top_config)?;

    // TODO: this doesn't seem to do anything
    proctitle::set_title(format!("web3_proxy-{}", top_config.app.chain_id));

    // tokio has code for catching ctrl+c so we use that
    // this shutdown sender is currently only used in tests, but we might make a /shutdown endpoint or something
    let (_shutdown_sender, shutdown_receiver) = flume::bounded(1);

    run(shutdown_receiver, cli_config, top_config)
}

#[cfg(test)]
mod tests {
    use ethers::{
        prelude::{Block, Http, Provider, TxHash, U256},
        utils::Anvil,
    };
    use hashbrown::HashMap;
    use std::env;

    use web3_proxy::config::{AppConfig, Web3ConnectionConfig};

    use super::*;

    #[tokio::test]
    async fn it_works() {
        // TODO: move basic setup into a test fixture
        let path = env::var("PATH").unwrap();

        println!("path: {}", path);

        // TODO: how should we handle logs in this?
        // TODO: option for super verbose logs
        std::env::set_var("RUST_LOG", "info,web3_proxy=debug");
        // install global collector configured based on RUST_LOG env var.
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .compact()
            .with_test_writer()
            .init();

        let anvil = Anvil::new().spawn();

        println!("Anvil running at `{}`", anvil.endpoint());

        let anvil_provider = Provider::<Http>::try_from(anvil.endpoint()).unwrap();

        // mine a block because my code doesn't like being on block 0
        // TODO: make block 0 okay?
        let _: U256 = anvil_provider
            .request("evm_mine", None::<()>)
            .await
            .unwrap();

        // make a test CliConfig
        let cli_config = CliConfig {
            port: 0,
            prometheus_port: 0,
            workers: 4,
            config: "./does/not/exist/test.toml".to_string(),
        };

        // make a test AppConfig
        let app_config = TopConfig {
            app: AppConfig {
                chain_id: 31337,
                default_requests_per_minute: 6_000_000,
                min_sum_soft_limit: 1,
                min_synced_rpcs: 1,
                public_rate_limit_per_minute: 6_000_000,
                response_cache_max_bytes: 10_usize.pow(7),
                redirect_public_url: "example.com/".to_string(),
                redirect_user_url: "example.com/{{user_id}}".to_string(),
                ..Default::default()
            },
            balanced_rpcs: HashMap::from([
                (
                    "anvil".to_string(),
                    Web3ConnectionConfig::new(anvil.endpoint(), 100, None, 1, Some(false)),
                ),
                (
                    "anvil_ws".to_string(),
                    Web3ConnectionConfig::new(anvil.ws_endpoint(), 100, None, 0, Some(true)),
                ),
            ]),
            private_rpcs: None,
        };

        let (shutdown_sender, shutdown_receiver) = flume::bounded(1);

        // spawn another thread for running the app
        // TODO: allow launching into the local tokio runtime instead of creating a new one?
        let handle = thread::spawn(move || run(shutdown_receiver, cli_config, app_config));

        // TODO: do something to the node. query latest block, mine another block, query again
        let proxy_provider = Provider::<Http>::try_from(anvil.endpoint()).unwrap();

        let anvil_result: Block<TxHash> = anvil_provider
            .request("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap();
        let proxy_result: Block<TxHash> = proxy_provider
            .request("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap();

        assert_eq!(anvil_result, proxy_result);

        let first_block_num = anvil_result.number.unwrap();

        let _: U256 = anvil_provider
            .request("evm_mine", None::<()>)
            .await
            .unwrap();

        let anvil_result: Block<TxHash> = anvil_provider
            .request("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap();
        let proxy_result: Block<TxHash> = proxy_provider
            .request("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap();

        assert_eq!(anvil_result, proxy_result);

        let second_block_num = anvil_result.number.unwrap();

        assert_ne!(first_block_num, second_block_num);

        // tell the test app to shut down
        shutdown_sender.send(()).unwrap();

        println!("waiting for shutdown...");
        // TODO: panic if a timeout is reached
        handle.join().unwrap().unwrap();
    }
}
