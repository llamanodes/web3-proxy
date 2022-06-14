#![forbid(unsafe_code)]

mod app;
mod config;
mod connection;
mod connections;
mod frontend;
mod jsonrpc;

use crate::app::{flatten_handle, Web3ProxyApp};
use crate::config::{CliConfig, RpcConfig};
use parking_lot::deadlock;
use std::fs;
use std::sync::atomic::{self, AtomicUsize};
use std::thread;
use std::time::Duration;
use tokio::runtime;
use tracing::{error, info, trace};
use tracing_subscriber::EnvFilter;

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

    fdlimit::raise_fd_limit();

    let cli_config: CliConfig = argh::from_env();

    info!("Loading rpc config @ {}", cli_config.config);
    let rpc_config: String = fs::read_to_string(cli_config.config)?;
    let rpc_config: RpcConfig = toml::from_str(&rpc_config)?;

    trace!("rpc_config: {:?}", rpc_config);

    // TODO: this doesn't seem to do anything
    proctitle::set_title(format!("web3-proxy-{}", rpc_config.shared.chain_id));

    let chain_id = rpc_config.shared.chain_id;

    let mut rt_builder = runtime::Builder::new_multi_thread();

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

    let rt = rt_builder.build()?;

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

    // spawn the root task
    rt.block_on(async {
        let (app, app_handle) = rpc_config.spawn().await?;

        let frontend_handle = tokio::spawn(frontend::run(cli_config.port, app));

        match tokio::try_join!(flatten_handle(app_handle), flatten_handle(frontend_handle)) {
            Ok(_) => {
                // do something with the values
                info!("app completed")
            }
            Err(err) => {
                error!(?err, "app failed");
            }
        }

        Ok(())
    })
}
