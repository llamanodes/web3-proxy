use ethers::{
    prelude::{
        rand::{self, distributions::Alphanumeric, Rng},
        Http, Provider,
    },
    signers::LocalWallet,
    types::Address,
    utils::{Anvil, AnvilInstance},
};
use hashbrown::HashMap;
use migration::sea_orm::DatabaseConnection;
use parking_lot::Mutex;
use serde_json::json;
use std::{
    env,
    process::Command as SyncCommand,
    str::FromStr,
    sync::atomic::{AtomicU16, Ordering},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    process::Command as AsyncCommand,
    sync::{
        broadcast::{self, error::SendError},
        mpsc, oneshot,
    },
    task::JoinHandle,
    time::{sleep, Instant},
};
use tracing::{info, trace, warn};
use web3_proxy::{
    config::{AppConfig, TopConfig, Web3RpcConfig},
    relational_db::get_migrated_db,
    stats::FlushedStats,
    sub_commands::ProxydSubCommand,
};

#[derive(Clone)]
pub struct DbData {
    pub conn: Option<DatabaseConnection>,
    pub container_name: String,
    pub url: Option<String>,
}

pub struct TestApp {
    /// anvil shuts down when this guard is dropped.
    pub anvil: AnvilInstance,

    /// connection to anvil.
    pub anvil_provider: Provider<Http>,

    /// keep track of the database so it can be stopped on drop
    pub db: Option<DbData>,

    /// spawn handle for the proxy.
    pub proxy_handle: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,

    /// connection to the proxy that is connected to anil.
    pub proxy_provider: Provider<Http>,

    /// tell the app to flush stats to the database
    flush_stat_buffer_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,

    /// tell the app to shut down (use `self.stop()`).
    shutdown_sender: broadcast::Sender<()>,
}

impl TestApp {
    pub async fn spawn(chain_id: u64, setup_db: bool) -> Self {
        info!(?chain_id);

        let num_workers = 2;

        // TODO: move basic setup into a test fixture
        let path = env::var("PATH").unwrap();

        info!(%path);

        // TODO: configurable rpc and block
        let anvil = Anvil::new()
            .chain_id(chain_id)
            // .fork("https://polygon.llamarpc.com@44300000")
            .spawn();

        info!("Anvil running at `{}`", anvil.endpoint());

        let anvil_provider = Provider::<Http>::try_from(anvil.endpoint()).unwrap();

        // TODO: instead of starting a db every time, use a connection pool and transactions to begin/rollback
        let db = if setup_db {
            // sqlite doesn't seem to work. our migrations are written for mysql
            // so lets use docker to start mysql
            let password: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(16)
                .map(char::from)
                .collect();

            let random: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(8)
                .map(char::from)
                .collect();

            let db_container_name = format!("web3-proxy-test-{}", random);

            info!(%db_container_name);

            // create the db_data as soon as the url is known
            // when this is dropped, the db will be stopped
            let mut db_data = DbData {
                conn: None,
                container_name: db_container_name.clone(),
                url: None,
            };

            let _ = AsyncCommand::new("docker")
                .args([
                    "run",
                    "--name",
                    &db_container_name,
                    "--rm",
                    "-d",
                    "-e",
                    &format!("MYSQL_ROOT_PASSWORD={}", password),
                    "-e",
                    "MYSQL_DATABASE=web3_proxy_test",
                    "-p",
                    "0:3306",
                    "mysql",
                ])
                .output()
                .await
                .expect("failed to start db");

            // give the db a second to start
            // TODO: wait until docker says it is healthy
            sleep(Duration::from_secs(1)).await;

            // TODO: why is this always empty?!
            let docker_inspect_output = AsyncCommand::new("docker")
                .args(["inspect", &db_container_name])
                .output()
                .await
                .unwrap();

            let docker_inspect_json = String::from_utf8(docker_inspect_output.stdout).unwrap();

            trace!(%docker_inspect_json);

            let docker_inspect_json: serde_json::Value =
                serde_json::from_str(&docker_inspect_json).unwrap();

            let mysql_ports = docker_inspect_json
                .get(0)
                .unwrap()
                .get("NetworkSettings")
                .unwrap()
                .get("Ports")
                .unwrap()
                .get("3306/tcp")
                .unwrap()
                .get(0)
                .unwrap();

            trace!(?mysql_ports);

            let mysql_port: u64 = mysql_ports
                .get("HostPort")
                .expect("unable to determine mysql port")
                .as_str()
                .unwrap()
                .parse()
                .unwrap();

            let mysql_ip = mysql_ports
                .get("HostIp")
                .and_then(|x| x.as_str())
                .expect("unable to determine mysql ip");
            // let mysql_ip = "localhost";
            // let mysql_ip = "127.0.0.1";

            let db_url = format!(
                "mysql://root:{}@{}:{}/web3_proxy_test",
                password, mysql_ip, mysql_port
            );

            info!(%db_url, "waiting for start");

            db_data.url = Some(db_url.clone());

            let start = Instant::now();
            let max_wait = Duration::from_secs(30);
            loop {
                if start.elapsed() > max_wait {
                    panic!("db took too long to start");
                }

                if TcpStream::connect(format!("{}:{}", mysql_ip, mysql_port))
                    .await
                    .is_ok()
                {
                    break;
                };

                // not open wait. sleep and then try again
                sleep(Duration::from_secs(1)).await;
            }

            // TODO: make sure mysql is actually ready for connections
            sleep(Duration::from_secs(1)).await;

            info!(%db_url, elapsed=%start.elapsed().as_secs_f32(), "db post is open. Migrating now...");

            // try to migrate
            let start = Instant::now();
            let max_wait = Duration::from_secs(30);
            loop {
                if start.elapsed() > max_wait {
                    panic!("db took too long to start");
                }

                match get_migrated_db(db_url.clone(), 1, 1).await {
                    Ok(x) => {
                        // it worked! yey!
                        db_data.conn = Some(x);
                        break;
                    }
                    Err(err) => {
                        // not connected. sleep and then try again
                        warn!(?err, "unable to migrate db. retrying in 1 second");
                        sleep(Duration::from_secs(1)).await;
                    }
                }
            }

            info!(%db_url, elapsed=%start.elapsed().as_secs_f32(), "db is migrated");

            Some(db_data)
        } else {
            None
        };

        let db_url = db.as_ref().and_then(|x| x.url.clone());

        // make a test TopConfig
        // TODO: test influx
        // TODO: test redis
        let app_config: AppConfig = serde_json::from_value(json!({
            "chain_id": chain_id,
            "db_url": db_url,
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

        let top_config = TopConfig {
            app: app_config,
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

        let (flush_stat_buffer_sender, flush_stat_buffer_receiver) = mpsc::channel(1);

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
                flush_stat_buffer_sender.clone(),
                flush_stat_buffer_receiver,
            ))
        };

        let mut frontend_port = frontend_port_arc.load(Ordering::Relaxed);
        let start = Instant::now();
        while frontend_port == 0 {
            // we have to give it some time because it might have to do migrations
            if start.elapsed() > Duration::from_secs(10) {
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
            db,
            proxy_handle: Mutex::new(Some(handle)),
            proxy_provider,
            flush_stat_buffer_sender,
            shutdown_sender,
        }
    }

    #[allow(unused)]
    pub fn db_conn(&self) -> &DatabaseConnection {
        self.db.as_ref().unwrap().conn.as_ref().unwrap()
    }

    #[allow(unused)]
    pub async fn flush_stats(&self) -> anyhow::Result<FlushedStats> {
        let (tx, rx) = oneshot::channel();

        self.flush_stat_buffer_sender.send(tx).await?;

        let x = rx.await?;

        Ok(x)
    }

    pub fn stop(&self) -> Result<usize, SendError<()>> {
        self.shutdown_sender.send(())
    }

    #[allow(unused)]
    pub async fn wait(&self) {
        let _ = self.stop();

        // TODO: lock+take feels weird, but it works
        let handle = self.proxy_handle.lock().take();

        if let Some(handle) = handle {
            info!("waiting for the app to stop...");
            handle.await.unwrap().unwrap();
        }
    }

    #[allow(unused)]
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

impl Drop for DbData {
    fn drop(&mut self) {
        info!(%self.container_name, "killing db");

        let _ = SyncCommand::new("docker")
            .args(["kill", "-s", "9", &self.container_name])
            .output();
    }
}
