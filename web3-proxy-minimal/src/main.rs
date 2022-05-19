mod app;
mod connection;
mod connections;

use parking_lot::deadlock;
use std::env;
use std::sync::atomic::{self, AtomicUsize};
use std::thread;
use std::time::Duration;
use tokio::runtime;

use crate::app::Web3ProxyApp;

fn main() -> anyhow::Result<()> {
    // TODO: is there a better way to do this?
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "web3_proxy_minimal=debug");
    }

    // install global collector configured based on RUST_LOG env var.
    // tracing_subscriber::fmt().init();
    console_subscriber::init();

    fdlimit::raise_fd_limit();

    let chain_id = 1;
    let workers = 4;

    let mut rt_builder = runtime::Builder::new_multi_thread();

    rt_builder.enable_all().thread_name_fn(move || {
        static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
        // TODO: what ordering? i think we want seqcst so that these all happen in order, but that might be stricter than we really need
        let worker_id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
        // TODO: i think these max at 15 characters
        format!("web3-{}-{}", chain_id, worker_id)
    });

    if workers > 0 {
        rt_builder.worker_threads(workers);
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
        let balanced_rpcs = vec![
            "http://127.0.0.1:8545",
            "ws://127.0.0.1:8546",
            "http://127.0.0.1:8549",
            "ws://127.0.0.1:8549",
            "https://api.edennetwork.io/v1/",
            "https://api.edennetwork.io/v1/beta",
            "https://rpc.ethermine.org",
            "https://rpc.flashbots.net",
            "https://gibson.securerpc.com/v1",
            "wss://ws-nd-373-761-850.p2pify.com/106d73af4cebc487df5ba92f1ad8dee7",
            "wss://mainnet.infura.io/ws/v3/c6fa1b6f17124b44ae71b2b25601aee0",
            "wss://ecfa2710350449f490725c4525cba584.eth.ws.rivet.cloud/",
            "wss://speedy-nodes-nyc.moralis.io/3587198387de4b2d711f6999/eth/mainnet/archive/ws",
        ]
        .into_iter()
        .map(|x| x.to_string())
        .collect();

        let app = Web3ProxyApp::try_new(chain_id, balanced_rpcs).await?;

        app.run().await
    })
}
