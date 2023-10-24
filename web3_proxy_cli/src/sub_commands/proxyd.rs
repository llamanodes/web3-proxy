use std::path::PathBuf;
use std::sync::atomic::AtomicU16;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, thread};
use tracing::{error, info, trace, warn};
use web3_proxy::app::App;
use web3_proxy::config::TopConfig;
use web3_proxy::globals::global_db_conn;
use web3_proxy::prelude::anyhow;
use web3_proxy::prelude::argh::{self, FromArgs};
use web3_proxy::prelude::futures::StreamExt;
use web3_proxy::prelude::num::Zero;
use web3_proxy::prelude::tokio;
use web3_proxy::prelude::tokio::signal::unix::SignalKind;
use web3_proxy::prelude::tokio::sync::{broadcast, mpsc, oneshot};
use web3_proxy::prelude::tokio::time::{sleep_until, Instant};
use web3_proxy::prelude::tokio::{select, signal};
use web3_proxy::prelude::toml;
use web3_proxy::stats::FlushedStats;
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
        let (frontend_shutdown_sender, _) = broadcast::channel(1);
        // TODO: i think there is a small race. if config_path changes

        let frontend_port = Arc::new(self.port.into());
        let prometheus_port = Arc::new(self.prometheus_port.into());
        let (flush_stat_buffer_sender, flush_stat_buffer_receiver) = mpsc::channel(8);

        Self::_main(
            top_config,
            Some(top_config_path),
            frontend_port,
            prometheus_port,
            num_workers,
            frontend_shutdown_sender,
            flush_stat_buffer_sender,
            flush_stat_buffer_receiver,
        )
        .await
    }

    /// this shouldn't really be pub except it makes test fixtures easier
    #[allow(clippy::too_many_arguments)]
    pub async fn _main(
        top_config: TopConfig,
        top_config_path: Option<PathBuf>,
        frontend_port: Arc<AtomicU16>,
        prometheus_port: Arc<AtomicU16>,
        num_workers: usize,
        frontend_shutdown_sender: broadcast::Sender<()>,
        flush_stat_buffer_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,
        flush_stat_buffer_receiver: mpsc::Receiver<oneshot::Sender<FlushedStats>>,
    ) -> anyhow::Result<()> {
        // tokio has code for catching ctrl+c so we use that to shut down in most cases
        // frontend_shutdown_sender is currently only used in tests, but we might make a /shutdown endpoint or something
        // we do not need this receiver. new receivers are made by `shutdown_sender.subscribe()`
        let (app_shutdown_sender, _app_shutdown_receiver) = broadcast::channel(1);

        let frontend_shutdown_receiver = frontend_shutdown_sender.subscribe();
        let prometheus_shutdown_receiver = app_shutdown_sender.subscribe();

        // TODO: should we use a watch or broadcast for these?
        let (frontend_shutdown_complete_sender, mut frontend_shutdown_complete_receiver) =
            broadcast::channel(1);

        // start the main app
        let mut spawned_app = App::spawn(
            frontend_port,
            prometheus_port,
            top_config.clone(),
            num_workers,
            app_shutdown_sender.clone(),
            flush_stat_buffer_sender,
            flush_stat_buffer_receiver,
        )
        .await?;

        let mut head_block_receiver = spawned_app.app.head_block_receiver();

        // start thread for watching config
        if let Some(top_config_path) = top_config_path {
            let config_sender = spawned_app.new_top_config;
            {
                let mut current_config = config_sender.borrow().clone();

                // TODO: move this to a helper function
                thread::spawn(move || loop {
                    // give the app some time to start before changing configs for the first time
                    thread::sleep(Duration::from_secs(60));

                    match fs::read_to_string(&top_config_path) {
                        Ok(new_top_config) => {
                            match toml::from_str::<TopConfig>(&new_top_config) {
                                Ok(mut new_top_config) => {
                                    new_top_config.clean();

                                    if new_top_config != current_config {
                                        trace!("current_config: {:#?}", current_config);
                                        trace!("new_top_config: {:#?}", new_top_config);

                                        // TODO: print the differences
                                        // TODO: first run seems to always see differences. why?
                                        info!("config @ {:?} changed", top_config_path);
                                        match config_sender.send(new_top_config.clone()) {
                                            Ok(()) => current_config = new_top_config,
                                            Err(err) => {
                                                error!(?err, "unable to apply new config")
                                            }
                                        }
                                    }
                                }
                                Err(err) => {
                                    // TODO: panic?
                                    error!("Unable to parse config! {:#?}", err);
                                }
                            }
                        }
                        Err(err) => {
                            // TODO: panic?
                            error!("Unable to read config! {:#?}", err);
                        }
                    }

                    // TODO: wait for SIGHUP instead?
                    // TODO: wait for file to change instead of polling. file notifications are really fragile depending on the system and setup though
                    thread::sleep(Duration::from_secs(30));
                });
            }
        }

        // start the prometheus metrics port
        let prometheus_handle = tokio::spawn(prometheus::serve(
            spawned_app.app.clone(),
            prometheus_shutdown_receiver,
        ));

        if spawned_app.app.config.db_url.is_some() {
            // give 30 seconds for the db to connect. if it does not connect, it will keep retrying
        }

        info!("waiting up to 60 seconds for a head block");
        let max_wait_until = Instant::now() + Duration::from_secs(60);
        loop {
            select! {
                _ = sleep_until(max_wait_until) => {
                    // TODO: an error would be fine if we had automated alerting in sentry
                    // for now, alerts are mostly in pagerduty and pagerduty alerts on panics
                    panic!("oh no! we never got a head block!");
                }
                _ = head_block_receiver.changed() => {
                    if let Some(head_block) = head_block_receiver
                        .borrow_and_update()
                        .as_ref()
                    {
                        info!(head_hash=?head_block.hash(), head_num=%head_block.number());
                        break;
                    } else {
                        // this is very unlikely to happen
                        info!("no head block yet!");
                        continue;
                    }
                }
            }
        }

        // start the frontend port
        let frontend_handle = tokio::spawn(frontend::serve(
            spawned_app.app.clone(),
            frontend_shutdown_receiver,
            frontend_shutdown_complete_sender,
        ));

        let mut terminate_stream = signal::unix::signal(SignalKind::terminate())?;

        // if everything is working, these should all run forever
        let mut exited_with_err = false;
        let mut frontend_exited = false;
        select! {
            x = spawned_app.balanced_handle => {
                match x {
                    Ok(_) => info!("balanced_handle exited"),
                    Err(e) => {
                        error!("balanced_handle exited: {:#?}", e);
                        exited_with_err = true;
                    }
                }
            }
            // // TODO: this handle always exits right away because it doesn't subscribe to any blocks
            // x = spawned_app.private_handle => {
            //     match x {
            //         Ok(_) => info!("private_handle exited"),
            //         Err(e) => {
            //             error!("private_handle exited: {:#?}", e);
            //             exited_with_err = true;
            //         }
            //     }
            // }
            // // TODO: this handle always exits right away because it doesn't subscribe to any blocks
            // x = spawned_app.bundler_4337_rpcs_handle => {
            //     match x {
            //         Ok(_) => info!("bundler_4337_rpcs_handle exited"),
            //         Err(e) => {
            //             error!("bundler_4337_rpcs_handle exited: {:#?}", e);
            //             exited_with_err = true;
            //         }
            //     }
            // }
            x = frontend_handle => {
                frontend_exited = true;
                match x {
                    Ok(Ok(_)) => info!("frontend exited"),
                    Ok(Err(e)) => {
                        error!("frontend exited: {:#?}", e);
                        exited_with_err = true;
                    }
                    Err(e) => {
                        error!(?e, "join on frontend failed");
                        exited_with_err = true;

                    }
                }
            }
            x = prometheus_handle => {
                match x {
                    Ok(Ok(_)) => info!("prometheus exited"),
                    Ok(Err(e)) => {
                        error!("prometheus exited: {:#?}", e);
                        exited_with_err = true;
                    }
                    Err(e) => {
                        error!(?e, "join on prometheus failed");
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
            x = terminate_stream.recv() => {
                match x {
                    Some(_) => info!("quiting from SIGTERM"),
                    None => {
                        // TODO: i don't think this is possible
                        error!("error quiting from SIGTERM");
                        exited_with_err = true;
                    }
                }
            }
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
        // TODO: can we use tokio::spawn Handle's abort?
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

        // TODO: make sure this happens even if we exit with an error
        if let Ok(db_conn) = global_db_conn() {
            /*
            From the sqlx docs:

            We recommend calling .close().await to gracefully close the pool and its connections when you are done using it.
            This will also wake any tasks that are waiting on an .acquire() call,
            so for long-lived applications it’s a good idea to call .close() during shutdown.
            */
            db_conn.close().await?;
        }

        if background_errors.is_zero() && !exited_with_err {
            info!("finished");
            Ok(())
        } else {
            // TODO: collect all the errors here instead?
            Err(anyhow::anyhow!("finished with errors!"))
        }
    }
}
