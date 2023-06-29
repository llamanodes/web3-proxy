#![forbid(unsafe_code)]

use argh::FromArgs;
use futures::StreamExt;
use num::Zero;
use std::path::PathBuf;
use std::sync::atomic::AtomicU16;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, thread};
use tokio::sync::broadcast;
use tracing::{error, info, trace, warn};
use web3_proxy::app::{flatten_handle, flatten_handles, Web3ProxyApp};
use web3_proxy::config::TopConfig;
use web3_proxy::{frontend, prometheus};

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

        let frontend_port = Arc::new(self.port.into());
        let prometheus_port = Arc::new(self.prometheus_port.into());

        run(
            top_config,
            Some(top_config_path),
            frontend_port,
            prometheus_port,
            num_workers,
            shutdown_sender,
        )
        .await
    }
}

async fn run(
    top_config: TopConfig,
    top_config_path: Option<PathBuf>,
    frontend_port: Arc<AtomicU16>,
    prometheus_port: Arc<AtomicU16>,
    num_workers: usize,
    frontend_shutdown_sender: broadcast::Sender<()>,
) -> anyhow::Result<()> {
    // tokio has code for catching ctrl+c so we use that
    // this shutdown sender is currently only used in tests, but we might make a /shutdown endpoint or something
    // we do not need this receiver. new receivers are made by `shutdown_sender.subscribe()`

    // TODO: should we use a watch or broadcast for these?
    // Maybe this one ?
    // let mut shutdown_receiver = shutdown_sender.subscribe();
    let (app_shutdown_sender, _app_shutdown_receiver) = broadcast::channel(1);

    let frontend_shutdown_receiver = frontend_shutdown_sender.subscribe();
    let prometheus_shutdown_receiver = app_shutdown_sender.subscribe();

    // TODO: should we use a watch or broadcast for these?
    let (frontend_shutdown_complete_sender, mut frontend_shutdown_complete_receiver) =
        broadcast::channel(1);

    // start the main app
    let mut spawned_app = Web3ProxyApp::spawn(
        frontend_port,
        prometheus_port,
        top_config.clone(),
        num_workers,
        app_shutdown_sender.clone(),
    )
    .await?;

    // start thread for watching config
    if let Some(top_config_path) = top_config_path {
        let config_sender = spawned_app.new_top_config;
        {
            let mut current_config = config_sender.borrow().clone();

            thread::spawn(move || loop {
                match fs::read_to_string(&top_config_path) {
                    Ok(new_top_config) => match toml::from_str::<TopConfig>(&new_top_config) {
                        Ok(new_top_config) => {
                            if new_top_config != current_config {
                                // TODO: print the differences
                                // TODO: first run seems to always see differences. why?
                                info!("config @ {:?} changed", top_config_path);
                                config_sender.send(new_top_config.clone()).unwrap();
                                current_config = new_top_config;
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
    let prometheus_handle = tokio::spawn(prometheus::serve(
        spawned_app.app.clone(),
        prometheus_shutdown_receiver,
    ));

    info!("waiting for head block");
    loop {
        spawned_app.app.head_block_receiver().changed().await?;

        if spawned_app
            .app
            .head_block_receiver()
            .borrow_and_update()
            .is_some()
        {
            break;
        } else {
            info!("no head block yet!");
        }
    }

    // start the frontend port
    let frontend_handle = tokio::spawn(frontend::serve(
        spawned_app.app,
        frontend_shutdown_receiver,
        frontend_shutdown_complete_sender,
    ));

    let frontend_handle = flatten_handle(frontend_handle);

    // if everything is working, these should all run forever
    let mut exited_with_err = false;
    let mut frontend_exited = false;
    tokio::select! {
        x = flatten_handles(spawned_app.app_handles) => {
            match x {
                Ok(_) => info!("app_handle exited"),
                Err(e) => {
                    error!("app_handle exited: {:#?}", e);
                    exited_with_err = true;
                }
            }
        }
        x = frontend_handle => {
            frontend_exited = true;
            match x {
                Ok(_) => info!("frontend exited"),
                Err(e) => {
                    error!("frontend exited: {:#?}", e);
                    exited_with_err = true;
                }
            }
        }
        x = flatten_handle(prometheus_handle) => {
            match x {
                Ok(_) => info!("prometheus exited"),
                Err(e) => {
                    error!("prometheus exited: {:#?}", e);
                    exited_with_err = true;
                }
            }
        }
        x = tokio::signal::ctrl_c() => {
            // TODO: unix terminate signal, too
            match x {
                Ok(_) => info!("quiting from ctrl-c"),
                Err(e) => {
                    // TODO: i don't think this is possible
                    error!("error quiting from ctrl-c: {:#?}", e);
                    exited_with_err = true;
                }
            }
        }
        // TODO: This seems to have been removed on the main branch
        // TODO: how can we properly watch background handles here? this returns None immediatly and the app exits. i think the bug is somewhere else though
        x = spawned_app.background_handles.next() => {
            match x {
                Some(Ok(_)) => info!("quiting from background handles"),
                Some(Err(e)) => {
                    error!("quiting from background handle error: {:#?}", e);
                    exited_with_err = true;
                }
                None => {
                    // TODO: is this an error?
                    warn!("background handles exited");
                }
            }
        }
    };

    // TODO: This is also not there on the main branch
    // if a future above completed, make sure the frontend knows to start turning off
    if !frontend_exited {
        if let Err(err) = frontend_shutdown_sender.send(()) {
            // TODO: this is actually expected if the frontend is already shut down
            warn!(?err, "shutdown sender");
        };
    }

    // TODO: Also not there on main branch
    // TODO: wait until the frontend completes
    if let Err(err) = frontend_shutdown_complete_receiver.recv().await {
        warn!(?err, "shutdown completition");
    } else {
        info!("frontend exited gracefully");
    }

    // now that the frontend is complete, tell all the other futures to finish
    if let Err(err) = app_shutdown_sender.send(()) {
        warn!(?err, "backend sender");
    };

    info!(
        "waiting on {} important background tasks",
        spawned_app.background_handles.len()
    );
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
            Ok(Ok(_)) => {
                // TODO: how can we know which handle exited?
                trace!("a background handle exited");
                continue;
            }
        }
    }

    if background_errors.is_zero() && !exited_with_err {
        info!("finished");
        Ok(())
    } else {
        // TODO: collect all the errors here instead?
        Err(anyhow::anyhow!("finished with errors!"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::{
        prelude::{Http, Provider, U256},
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
    use tokio::{
        sync::broadcast::error::SendError,
        task::JoinHandle,
        time::{sleep, Instant},
    };
    use web3_proxy::{
        config::{AppConfig, Web3RpcConfig},
        rpcs::blockchain::ArcBlock,
    };

    // TODO: put it in a thread?
    struct TestApp {
        _anvil: AnvilInstance,
        handle: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
        anvil_provider: Provider<Http>,
        proxy_provider: Provider<Http>,
        shutdown_sender: broadcast::Sender<()>,
    }

    impl TestApp {
        async fn spawn() -> Self {
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
            // TODO: load TopConfig from a file? CliConfig could have `cli_config.load_top_config`. would need to inject our endpoint ports
            let top_config = TopConfig {
                app: AppConfig {
                    chain_id: 31337,
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
                    "anvil_both".to_string(),
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

            // spawn another thread for running the app
            // TODO: allow launching into the local tokio runtime instead of creating a new one?
            let handle = {
                tokio::spawn(run(
                    top_config,
                    None,
                    frontend_port_arc.clone(),
                    prometheus_port_arc,
                    2,
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
                handle: Mutex::new(Some(handle)),
                anvil_provider,
                proxy_provider,
                shutdown_sender,
                _anvil: anvil,
            }
        }

        fn stop(&self) -> Result<usize, SendError<()>> {
            self.shutdown_sender.send(())
        }

        async fn wait(&self) {
            // TODO: lock+take feels weird, but it works
            let handle = self.handle.lock().take();

            if let Some(handle) = handle {
                let _ = self.stop();

                info!("waiting for the app to stop...");
                handle.await.unwrap().unwrap();
            }
        }
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    #[test_log::test(tokio::test)]
    async fn it_works() {
        let x = TestApp::spawn().await;

        let anvil_provider = &x.anvil_provider;
        let proxy_provider = &x.proxy_provider;

        let anvil_result = anvil_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap()
            .unwrap();
        let proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(anvil_result, proxy_result);

        let first_block_num = anvil_result.number.unwrap();

        // mine a block
        let _: U256 = anvil_provider.request("evm_mine", ()).await.unwrap();

        // make sure the block advanced
        let anvil_result = anvil_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap()
            .unwrap();

        let second_block_num = anvil_result.number.unwrap();

        assert_eq!(first_block_num, second_block_num - 1);

        let mut proxy_result;
        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("took too long to sync!");
            }

            proxy_result = proxy_provider
                .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
                .await
                .unwrap();

            if let Some(ref proxy_result) = proxy_result {
                if proxy_result.number != Some(first_block_num) {
                    break;
                }
            }

            sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(anvil_result, proxy_result.unwrap());

        x.wait().await;
    }
}
