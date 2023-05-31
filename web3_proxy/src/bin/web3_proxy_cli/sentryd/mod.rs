mod compare;
mod simple;

use anyhow::Context;
use argh::FromArgs;
use futures::{
    stream::{FuturesUnordered, StreamExt},
    Future,
};
use log::{error, info};
use pagerduty_rs::{eventsv2async::EventsV2 as PagerdutyAsyncEventsV2, types::Event};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use web3_proxy::{config::TopConfig, pagerduty::pagerduty_alert};

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Loop healthchecks and send pager duty alerts if any fail
#[argh(subcommand, name = "sentryd")]
pub struct SentrydSubCommand {
    #[argh(positional)]
    /// the main (HTTP only) web3-proxy being checked.
    web3_proxy: String,

    /// the chain id to require. Only used if not using --config.
    #[argh(option)]
    chain_id: Option<u64>,

    #[argh(option)]
    /// warning threshold for age of the best known head block
    max_age: i64,

    #[argh(option)]
    /// warning threshold for seconds between the rpc and best other_rpc's head blocks
    max_lag: i64,

    #[argh(option)]
    /// other (HTTP only) rpcs to compare the main rpc to
    other_rpc: Vec<String>,

    #[argh(option)]
    /// other (HTTP only) web3-proxies to compare the main rpc to
    other_proxy: Vec<String>,

    #[argh(option)]
    /// how many seconds between running checks
    seconds: Option<u64>,
}

#[derive(Debug)]
pub struct SentrydError {
    /// The class/type of the event, for example ping failure or cpu load
    class: String,
    /// Errors will send a pagerduty alert. others just give log messages
    level: log::Level,
    /// A short summary that should be mostly static
    summary: String,
    /// Lots of detail about the error
    extra: Option<serde_json::Value>,
}

/// helper for creating SentrydErrors
#[derive(Clone)]
pub struct SentrydErrorBuilder {
    class: String,
    level: log::Level,
}

impl SentrydErrorBuilder {
    fn build(&self, err: anyhow::Error) -> SentrydError {
        SentrydError {
            class: self.class.to_owned(),
            level: self.level.to_owned(),
            summary: format!("{}", err),
            extra: Some(json!(format!("{:#?}", err))),
        }
    }

    fn result(&self, err: anyhow::Error) -> SentrydResult {
        Err(self.build(err))
    }
}

type SentrydResult = Result<(), SentrydError>;

impl SentrydSubCommand {
    pub async fn main(
        self,
        pagerduty_async: Option<PagerdutyAsyncEventsV2>,
        top_config: Option<TopConfig>,
    ) -> anyhow::Result<()> {
        // sentry logging should already be configured

        let chain_id = self
            .chain_id
            .or_else(|| top_config.map(|x| x.app.chain_id))
            .context("--config or --chain-id required")?;

        let primary_proxy = self.web3_proxy.trim_end_matches('/').to_string();

        let other_proxy: Vec<_> = self
            .other_proxy
            .into_iter()
            .map(|x| x.trim_end_matches('/').to_string())
            .collect();

        let other_rpc: Vec<_> = self
            .other_rpc
            .into_iter()
            .map(|x| x.trim_end_matches('/').to_string())
            .collect();

        let seconds = self.seconds.unwrap_or(60);

        let mut handles = FuturesUnordered::new();

        // channels and a task for sending errors to logs/pagerduty
        let (error_sender, mut error_receiver) = mpsc::channel::<SentrydError>(10);

        {
            let error_handler_f = async move {
                if pagerduty_async.is_none() {
                    info!("set PAGERDUTY_INTEGRATION_KEY to send create alerts for errors");
                }

                while let Some(err) = error_receiver.recv().await {
                    log::log!(err.level, "check failed: {:#?}", err);

                    if matches!(err.level, log::Level::Error) {
                        let alert = pagerduty_alert(
                            Some(chain_id),
                            Some(err.class),
                            Some("web3-proxy-sentry".to_string()),
                            None,
                            None,
                            err.extra,
                            pagerduty_rs::types::Severity::Error,
                            None,
                            err.summary,
                            None,
                        );

                        if let Some(ref pagerduty_async) = pagerduty_async {
                            info!(
                                "sending to pagerduty: {:#}",
                                serde_json::to_string_pretty(&alert)?
                            );

                            if let Err(err) =
                                pagerduty_async.event(Event::AlertTrigger(alert)).await
                            {
                                error!("Failed sending to pagerduty: {:#?}", err);
                            }
                        }
                    }
                }

                Ok(())
            };

            handles.push(tokio::spawn(error_handler_f));
        }

        // spawn a bunch of health check loops that do their checks on an interval

        // check the main rpc's /health endpoint
        {
            let url = if primary_proxy.contains("/rpc/") {
                let x = primary_proxy.split("/rpc/").next().unwrap();

                format!("{}/health", x)
            } else {
                format!("{}/health", primary_proxy)
            };
            let error_sender = error_sender.clone();

            // TODO: what timeout?
            let timeout = Duration::from_secs(5);

            let loop_f = a_loop(
                "main /health",
                seconds,
                log::Level::Error,
                error_sender,
                move |error_builder| simple::main(error_builder, url.clone(), timeout),
            );

            handles.push(tokio::spawn(loop_f));
        }
        // check any other web3-proxy /health endpoints
        for other_web3_proxy in other_proxy.iter() {
            let url = if other_web3_proxy.contains("/rpc/") {
                let x = other_web3_proxy.split("/rpc/").next().unwrap();

                format!("{}/health", x)
            } else {
                format!("{}/health", other_web3_proxy)
            };

            let error_sender = error_sender.clone();

            // TODO: what timeout?
            let timeout = Duration::from_secs(5);

            let loop_f = a_loop(
                "other /health",
                seconds,
                log::Level::Warn,
                error_sender,
                move |error_builder| simple::main(error_builder, url.clone(), timeout),
            );

            handles.push(tokio::spawn(loop_f));
        }

        // compare the main web3-proxy head block to all web3-proxies and rpcs
        {
            let max_age = self.max_age;
            let max_lag = self.max_lag;
            let primary_proxy = primary_proxy.clone();
            let error_sender = error_sender.clone();

            let mut others = other_proxy.clone();

            others.extend(other_rpc);

            let loop_f = a_loop(
                "head block comparison",
                seconds,
                log::Level::Error,
                error_sender,
                move |error_builder| {
                    compare::main(
                        error_builder,
                        primary_proxy.clone(),
                        others.clone(),
                        max_age,
                        max_lag,
                    )
                },
            );

            handles.push(tokio::spawn(loop_f));
        }

        // wait for any returned values (if everything is working, they will all run forever)
        while let Some(x) = handles.next().await {
            // any errors that make it here will end the program
            x??;
        }

        Ok(())
    }
}

async fn a_loop<T>(
    class: &str,
    seconds: u64,
    error_level: log::Level,
    error_sender: mpsc::Sender<SentrydError>,
    f: impl Fn(SentrydErrorBuilder) -> T,
) -> anyhow::Result<()>
where
    T: Future<Output = SentrydResult> + Send + 'static,
{
    let error_builder = SentrydErrorBuilder {
        class: class.to_owned(),
        level: error_level,
    };

    let mut interval = interval(Duration::from_secs(seconds));

    // TODO: should we warn if there are delays?
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;

        if let Err(err) = f(error_builder.clone()).await {
            error_sender.send(err).await?;
        };
    }
}
