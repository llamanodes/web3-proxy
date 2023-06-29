use ethers::{
    prelude::{Http, Provider},
    signers::LocalWallet,
    types::Address,
    utils::{Anvil, AnvilInstance},
};
use hashbrown::HashMap;
use parking_lot::Mutex;
use std::{
    env,
    str::FromStr,
    sync::atomic::{AtomicU16, Ordering},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::broadcast::{self, error::SendError},
    task::JoinHandle,
    time::{sleep, Instant},
};
use tracing::info;
use web3_proxy::{
    config::{AppConfig, TopConfig, Web3RpcConfig},
    sub_commands::ProxydSubCommand,
};

pub struct TestApp {
    /// anvil shuts down when this guard is dropped.
    pub anvil: AnvilInstance,

    /// connection to anvil.
    pub anvil_provider: Provider<Http>,

    /// spawn handle for the proxy.
    pub handle: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,

    /// connection to the proxy that is connected to anil.
    pub proxy_provider: Provider<Http>,

    /// tell the app to shut down (use `self.stop()`).
    shutdown_sender: broadcast::Sender<()>,
}

impl TestApp {
    pub async fn spawn() -> Self {
        let num_workers = 2;

        // TODO: move basic setup into a test fixture
        let path = env::var("PATH").unwrap();

        info!("path: {}", path);

        // TODO: configurable rpc and block
        let anvil = Anvil::new()
            // .fork("https://polygon.llamarpc.com@44300000")
            .spawn();

        info!("Anvil running at `{}`", anvil.endpoint());

        let anvil_provider = Provider::<Http>::try_from(anvil.endpoint()).unwrap();

        // make a test TopConfig
        // TODO: test influx
        // TODO: test redis
        let top_config = TopConfig {
            app: AppConfig {
                chain_id: 31337,
                // TODO: [make sqlite work](<https://www.sea-ql.org/SeaORM/docs/write-test/sqlite/>)
                // db_url: Some("sqlite::memory:".into()),
                default_user_max_requests_per_period: Some(6_000_000),
                deposit_factory_contract: Address::from_str(
                    "4e3BC2054788De923A04936C6ADdB99A05B0Ea36",
                )
                .ok(),
                min_sum_soft_limit: 1,
                min_synced_rpcs: 1,
                public_requests_per_period: Some(1_000_000),
                response_cache_max_bytes: 10_u64.pow(7),
                ..Default::default()
            },
            balanced_rpcs: HashMap::from([(
                "anvil".to_string(),
                Web3RpcConfig {
                    http_url: Some(anvil.endpoint()),
                    ws_url: Some(anvil.ws_endpoint()),
                    ..Default::default()
                },
            )]),
            private_rpcs: None,
            bundler_4337_rpcs: None,
            extra: Default::default(),
        };

        let (shutdown_sender, _shutdown_receiver) = broadcast::channel(1);

        let frontend_port_arc = Arc::new(AtomicU16::new(0));
        let prometheus_port_arc = Arc::new(AtomicU16::new(0));

        // spawn the app
        // TODO: spawn in a thread so we can run from non-async tests and so the Drop impl can wait for it to stop
        let handle = {
            tokio::spawn(ProxydSubCommand::_main(
                top_config,
                None,
                frontend_port_arc.clone(),
                prometheus_port_arc,
                num_workers,
                shutdown_sender.clone(),
            ))
        };

        let mut frontend_port = frontend_port_arc.load(Ordering::Relaxed);
        let start = Instant::now();
        while frontend_port == 0 {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("took too long to start!");
            }

            sleep(Duration::from_millis(10)).await;
            frontend_port = frontend_port_arc.load(Ordering::Relaxed);
        }

        let proxy_endpoint = format!("http://127.0.0.1:{}", frontend_port);

        let proxy_provider = Provider::<Http>::try_from(proxy_endpoint).unwrap();

        Self {
            anvil,
            anvil_provider,
            handle: Mutex::new(Some(handle)),
            proxy_provider,
            shutdown_sender,
        }
    }

    pub fn stop(&self) -> Result<usize, SendError<()>> {
        self.shutdown_sender.send(())
    }

    pub async fn wait(&self) {
        // TODO: lock+take feels weird, but it works
        let handle = self.handle.lock().take();

        if let Some(handle) = handle {
            let _ = self.stop();

            info!("waiting for the app to stop...");
            handle.await.unwrap().unwrap();
        }
    }

    pub fn wallet(&self, id: usize) -> LocalWallet {
        self.anvil.keys()[id].clone().into()
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.stop();

        // TODO: do we care about waiting for it to stop? it will slow our tests down so we probably only care about waiting in some tests
    }
}
