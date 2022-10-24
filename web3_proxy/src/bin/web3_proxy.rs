//! Web3_proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.
//!
//! Signed transactions (eth_sendRawTransaction) are sent in parallel to the configured private RPCs (eden, ethermine, flashbots, etc.).
//!
//! All other requests are sent to an RPC server on the latest block (alchemy, moralis, rivet, your own node, or one of many other providers).
//! If multiple servers are in sync, the fastest server is prioritized. Since the fastest server is most likely to serve requests, slow servers are unlikely to ever get any requests.

//#![warn(missing_docs)]
#![forbid(unsafe_code)]

use futures::StreamExt;
use parking_lot::deadlock;
use std::fs;
use std::sync::atomic::{self, AtomicUsize};
use std::thread;
use tokio::runtime;
use tokio::sync::broadcast;
use tokio::time::Duration;
use tracing::{debug, info, warn};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;
use web3_proxy::app::{flatten_handle, flatten_handles, Web3ProxyApp};
use web3_proxy::config::{CliConfig, TopConfig};
use web3_proxy::{frontend, metrics_frontend};

fn run(
    shutdown_sender: broadcast::Sender<()>,
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
        let app_frontend_port = cli_config.port;
        let app_prometheus_port = cli_config.prometheus_port;

        let (app, app_handles, mut important_background_handles) =
            Web3ProxyApp::spawn(top_config, num_workers, shutdown_sender.subscribe()).await?;

        let frontend_handle = tokio::spawn(frontend::serve(app_frontend_port, app.clone()));

        let prometheus_handle = tokio::spawn(metrics_frontend::serve(app, app_prometheus_port));

        // if everything is working, these should both run forever
        // TODO: join these instead and use shutdown handler properly. probably use tokio's ctrl+c helper
        tokio::select! {
            x = flatten_handles(app_handles) => {
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
        };

        // one of the handles stopped. send a value so the others know to shut down
        if let Err(err) = shutdown_sender.send(()) {
            warn!(?err, "shutdown sender");
        };

        // wait on all the important background tasks (like saving stats to the database) to complete
        while let Some(x) = important_background_handles.next().await {
            match x {
                Err(e) => return Err(e.into()),
                Ok(Err(e)) => return Err(e),
                Ok(Ok(_)) => continue,
            }
        }

        info!("finished");

        Ok(())
    })
}

fn main() -> anyhow::Result<()> {
    // if RUST_LOG isn't set, configure a default
    // TODO: is there a better way to do this?
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var(
            "RUST_LOG",
            "info,ethers=debug,redis_rate_limit=debug,web3_proxy=debug",
        );
    }

    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    // initial configuration from flags
    let cli_config: CliConfig = argh::from_env();

    // advanced configuration is on disk
    let top_config: String = fs::read_to_string(cli_config.config.clone())?;
    let top_config: TopConfig = toml::from_str(&top_config)?;

    // TODO: this doesn't seem to do anything
    proctitle::set_title(format!("web3_proxy-{}", top_config.app.chain_id));

    // connect to sentry for error reporting
    // if no sentry, only log to stdout
    let _sentry_guard = if let Some(sentry_url) = top_config.app.sentry_url.clone() {
        let guard = sentry::init((
            sentry_url,
            sentry::ClientOptions {
                release: sentry::release_name!(),
                // TODO: Set this a to lower value (from config) in production
                traces_sample_rate: 1.0,
                ..Default::default()
            },
        ));

        // TODO: how do we put the EnvFilter on this?
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .compact()
                    .with_filter(EnvFilter::from_default_env()),
            )
            .with(sentry_tracing::layer())
            .init();

        Some(guard)
    } else {
        // install global collector configured based on RUST_LOG env var.
        // TODO: attach sentry here
        tracing_subscriber::fmt()
            .compact()
            .with_env_filter(EnvFilter::from_default_env())
            .init();

        None
    };

    // we used to do this earlier, but now we attach sentry
    debug!("CLI config @ {:#?}", cli_config.config);

    // tokio has code for catching ctrl+c so we use that
    // this shutdown sender is currently only used in tests, but we might make a /shutdown endpoint or something
    let (shutdown_sender, _shutdown_receiver) = broadcast::channel(1);

    run(shutdown_sender, cli_config, top_config)
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
        // TODO: sentry is needed here!
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
            cookie_key_filename: "./does/not/exist/development_cookie_key".to_string(),
        };

        // make a test AppConfig
        let app_config = TopConfig {
            app: AppConfig {
                chain_id: 31337,
                default_user_requests_per_minute: Some(6_000_000),
                min_sum_soft_limit: 1,
                min_synced_rpcs: 1,
                public_requests_per_minute: Some(1_000_000),
                response_cache_max_bytes: 10_usize.pow(7),
                redirect_public_url: Some("example.com/".to_string()),
                redirect_user_url: Some("example.com/{{user_id}}".to_string()),
                ..Default::default()
            },
            balanced_rpcs: HashMap::from([
                (
                    "anvil".to_string(),
                    Web3ConnectionConfig::new(false, anvil.endpoint(), 100, None, 1, Some(false)),
                ),
                (
                    "anvil_ws".to_string(),
                    Web3ConnectionConfig::new(false, anvil.ws_endpoint(), 100, None, 0, Some(true)),
                ),
            ]),
            private_rpcs: None,
        };

        let (shutdown_sender, _shutdown_receiver) = broadcast::channel(1);

        // spawn another thread for running the app
        // TODO: allow launching into the local tokio runtime instead of creating a new one?
        let handle = {
            let shutdown_sender = shutdown_sender.clone();

            thread::spawn(move || run(shutdown_sender, cli_config, app_config))
        };

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
