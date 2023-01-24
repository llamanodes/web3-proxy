mod compare;
mod simple;

use argh::FromArgs;
use futures::{
    stream::{FuturesUnordered, StreamExt},
    Future,
};
use log::{error, info};
use pagerduty_rs::{eventsv2async::EventsV2 as PagerdutyAsyncEventsV2, types::Event};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use web3_proxy::pagerduty::pagerduty_alert;

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Loop healthchecks and send pager duty alerts if any fail
#[argh(subcommand, name = "sentryd")]
pub struct SentrydSubCommand {
    #[argh(positional)]
    /// the main (HTTP only) web3-proxy being checked.
    web3_proxy: String,

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
struct Error {
    class: String,
    level: log::Level,
    anyhow: anyhow::Error,
}

impl SentrydSubCommand {
    pub async fn main(self, pagerduty_async: Option<PagerdutyAsyncEventsV2>) -> anyhow::Result<()> {
        // sentry logging should already be configured

        let seconds = self.seconds.unwrap_or(60);

        let mut handles = FuturesUnordered::new();

        // channels and a task for sending errors to logs/pagerduty
        let (error_sender, mut error_receiver) = mpsc::channel::<Error>(10);

        {
            let error_handler_f = async move {
                if pagerduty_async.is_none() {
                    info!("set PAGERDUTY_INTEGRATION_KEY to send create alerts for errors");
                }

                while let Some(err) = error_receiver.recv().await {
                    log::log!(err.level, "check failed: {:?}", err);

                    if matches!(err.level, log::Level::Error) {
                        let alert = pagerduty_alert(
                            None,
                            Some(err.class),
                            "web3-proxy-sentry".to_string(),
                            None,
                            None,
                            None::<()>,
                            Some("web3-proxy-sentry".to_string()),
                            pagerduty_rs::types::Severity::Error,
                            None,
                            format!("{}", err.anyhow),
                            None,
                        );

                        if let Some(pagerduty_async) = pagerduty_async.as_ref() {
                            info!(
                                "sending to pagerduty: {}",
                                serde_json::to_string_pretty(&alert)?
                            );

                            if let Err(err) =
                                pagerduty_async.event(Event::AlertTrigger(alert)).await
                            {
                                error!("Failed sending to pagerduty: {}", err);
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
            let url = format!("{}/health", self.web3_proxy);
            let error_sender = error_sender.clone();

            let loop_f = a_loop(
                "main /health",
                seconds,
                log::Level::Error,
                error_sender,
                move || simple::main(url.clone()),
            );

            handles.push(tokio::spawn(loop_f));
        }
        // check any other web3-proxy /health endpoints
        for other_web3_proxy in self.other_proxy.iter() {
            let url = format!("{}/health", other_web3_proxy);
            let error_sender = error_sender.clone();

            let loop_f = a_loop(
                "other /health",
                seconds,
                log::Level::Warn,
                error_sender,
                move || simple::main(url.clone()),
            );

            handles.push(tokio::spawn(loop_f));
        }

        // compare the main web3-proxy head block to all web3-proxies and rpcs
        {
            let max_age = self.max_age;
            let max_lag = self.max_lag;
            let rpc = self.web3_proxy.clone();
            let error_sender = error_sender.clone();

            let mut others = self.other_proxy.clone();

            others.extend(self.other_rpc.clone());

            let loop_f = a_loop(
                "head block comparison",
                seconds,
                log::Level::Error,
                error_sender,
                move || compare::main(rpc.clone(), others.clone(), max_age, max_lag),
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
    error_sender: mpsc::Sender<Error>,
    f: impl Fn() -> T,
) -> anyhow::Result<()>
where
    T: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let mut interval = interval(Duration::from_secs(seconds));

    // TODO: should we warn if there are delays?
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;

        if let Err(err) = f().await {
            let err = Error {
                class: class.to_string(),
                level: error_level,
                anyhow: err,
            };

            error_sender.send(err).await?;
        };
    }
}
