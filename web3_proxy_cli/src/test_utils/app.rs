use crate::sub_commands::ProxydSubCommand;
use std::{
    env,
    str::FromStr,
    sync::atomic::{AtomicU16, Ordering},
    thread,
};
use std::{sync::Arc, time::Duration};
use tracing::info;
use web3_proxy::prelude::anyhow;
use web3_proxy::prelude::ethers::{
    prelude::{Http, Provider},
    types::Address,
};
use web3_proxy::prelude::hashbrown::HashMap;
use web3_proxy::prelude::serde_json::json;
use web3_proxy::prelude::tokio::{
    runtime::Builder,
    sync::{
        broadcast::{self, error::SendError},
        mpsc, oneshot,
    },
    time::{sleep, Instant},
};
use web3_proxy::test_utils::{TestAnvil, TestInflux, TestMysql};
use web3_proxy::{
    config::{AppConfig, TopConfig, Web3RpcConfig},
    stats::FlushedStats,
};

pub struct TestApp {
    /// **THREAD** (not async) handle for the proxy.
    /// In an Option so we can take it and not break the `impl Drop`
    pub proxy_handle: Option<thread::JoinHandle<anyhow::Result<()>>>,

    /// connection to the proxy that is connected to anil.
    pub proxy_provider: Provider<Http>,

    /// tell the app to flush stats to the database
    flush_stat_buffer_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,

    /// tell the app to shut down (use `self.stop()`).
    shutdown_sender: broadcast::Sender<()>,
}

impl TestApp {
    pub async fn spawn(
        anvil: &TestAnvil,
        db: Option<&TestMysql>,
        influx: Option<&TestInflux>,
        unique_id: Option<u64>,
    ) -> Self {
        let chain_id = anvil.instance.chain_id();
        let num_workers = 4;

        // TODO: move basic setup into a test fixture
        let path = env::var("PATH").unwrap();

        info!(%path);

        let db_url = db.map(|x| x.url.clone());

        let (influx_host, influx_org, influx_token, influx_bucket) = match influx {
            None => (None, None, None, None),
            Some(x) => (
                Some(x.host.clone()),
                Some(x.org.clone()),
                Some(x.token.clone()),
                Some(x.bucket.clone()),
            ),
        };

        // make a test TopConfig
        // TODO: test influx
        // TODO: test redis
        let app_config: AppConfig = serde_json::from_value(json!({
            "chain_id": chain_id,
            "db_url": db_url,
            "influxdb_host": influx_host,
            "influxdb_org": influx_org,
            "influxdb_token": influx_token,
            "influxdb_bucket": influx_bucket,
            "unique_id": unique_id.unwrap_or_default(),
            "default_user_max_requests_per_period": Some(6_000_000),
            "deposit_factory_contract": Address::from_str(
                "4e3BC2054788De923A04936C6ADdB99A05B0Ea36",
            )
            .ok(),
            "min_sum_soft_limit": 1,
            "min_synced_rpcs": 1,
            "public_requests_per_period": Some(1_000_000),
            "response_cache_max_bytes": 10_u64.pow(7),
        }))
        .unwrap();

        info!("App Config is: {:?}", app_config);

        let top_config = TopConfig {
            app: app_config,
            balanced_rpcs: HashMap::from([(
                "anvil".to_string(),
                Web3RpcConfig {
                    http_url: Some(anvil.instance.endpoint()),
                    ws_url: Some(anvil.instance.ws_endpoint()),
                    ..Default::default()
                },
            )]),
            // influxdb_client: influx.map(|x| x.client),
            private_rpcs: HashMap::from([(
                "anvil_private".to_string(),
                Web3RpcConfig {
                    http_url: Some(anvil.instance.endpoint()),
                    ws_url: Some(anvil.instance.ws_endpoint()),
                    ..Default::default()
                },
            )]),
            bundler_4337_rpcs: Default::default(),
            extra: Default::default(),
        };

        let (shutdown_sender, _shutdown_receiver) = broadcast::channel(1);

        let frontend_port_arc = Arc::new(AtomicU16::new(0));
        let prometheus_port_arc = Arc::new(AtomicU16::new(0));

        let (flush_stat_buffer_sender, flush_stat_buffer_receiver) = mpsc::channel(1);

        // spawn the app
        // TODO: spawn in a thread so we can run from non-async tests and so the Drop impl can wait for it to stop
        let handle = {
            let frontend_port_arc = frontend_port_arc.clone();
            let prometheus_port_arc = prometheus_port_arc.clone();
            let flush_stat_buffer_sender = flush_stat_buffer_sender.clone();
            let shutdown_sender = shutdown_sender.clone();

            // TODO: thread isn't enough! this needs its own process for the globals to be isolated!
            thread::spawn(move || {
                let runtime = Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(num_workers)
                    .build()
                    .unwrap();

                runtime.block_on(ProxydSubCommand::_main(
                    top_config,
                    None,
                    frontend_port_arc,
                    prometheus_port_arc,
                    num_workers,
                    shutdown_sender,
                    flush_stat_buffer_sender,
                    flush_stat_buffer_receiver,
                ))
            })
        };

        let mut frontend_port = frontend_port_arc.load(Ordering::SeqCst);
        let start = Instant::now();
        while frontend_port == 0 {
            // we have to give it some time because it might have to do migrations
            if start.elapsed() > Duration::from_secs(30) {
                panic!("took too long to start!");
            }

            sleep(Duration::from_millis(10)).await;
            frontend_port = frontend_port_arc.load(Ordering::SeqCst);
        }

        let proxy_endpoint = format!("http://127.0.0.1:{}", frontend_port);

        let proxy_provider = Provider::<Http>::try_from(proxy_endpoint).unwrap();

        Self {
            proxy_handle: Some(handle),
            proxy_provider,
            flush_stat_buffer_sender,
            shutdown_sender,
        }
    }

    pub async fn flush_stats(&self) -> anyhow::Result<FlushedStats> {
        let (tx, rx) = oneshot::channel();

        self.flush_stat_buffer_sender.send(tx).await?;

        let x = rx.await?;

        Ok(x)
    }

    pub async fn flush_stats_and_wait(&self) -> anyhow::Result<FlushedStats> {
        let mut x = FlushedStats::default();

        loop {
            // give stats time to get into the channel
            // TODO: do this better
            sleep(Duration::from_secs(5)).await;

            // Flush all stats here
            // TODO: the test should maybe pause time so that stats definitely flush from our queries.
            let flush_count = self.flush_stats().await?;

            x += flush_count;

            if flush_count.relational_frontend_requests + flush_count.timeseries_frontend_requests
                == 0
            {
                break;
            }

            info!(?flush_count);
        }

        Ok(x)
    }

    pub fn stop(&self) -> Result<usize, SendError<()>> {
        self.shutdown_sender.send(())
    }

    pub fn wait_for_stop(mut self) {
        let _ = self.stop();

        if let Some(handle) = self.proxy_handle.take() {
            handle.join().unwrap().unwrap();
        }
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.stop();

        // TODO: do we care about waiting for it to stop? it will slow our tests down so we probably only care about waiting in some tests
    }
}
