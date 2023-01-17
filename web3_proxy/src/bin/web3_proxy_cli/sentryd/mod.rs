mod compare;
mod simple;

use argh::FromArgs;
use futures::{
    stream::{FuturesUnordered, StreamExt},
    Future,
};
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Loop healthchecks and send pager duty alerts if any fail
#[argh(subcommand, name = "sentryd")]
pub struct SentrydSubCommand {
    #[argh(positional)]
    /// a descriptive name for this node (probably the hostname)
    location: String,

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

impl SentrydSubCommand {
    pub async fn main(self) -> anyhow::Result<()> {
        // sentry logging should already be configured

        let seconds = self.seconds.unwrap_or(60);

        let mut handles = FuturesUnordered::new();

        // spawn a bunch of health check loops that do their checks on an interval

        // check the main rpc's /health endpoint
        {
            let url = format!("{}/health", self.web3_proxy);

            let loop_f = a_loop(seconds, log::Level::Error, move || {
                simple::main(url.clone())
            });

            handles.push(tokio::spawn(loop_f));
        }
        // check any other web3-proxy /health endpoints
        for other_web3_proxy in self.other_proxy.iter() {
            let url = format!("{}/health", other_web3_proxy);

            let loop_f = a_loop(seconds, log::Level::Warn, move || simple::main(url.clone()));

            handles.push(tokio::spawn(loop_f));
        }

        // compare the main web3-proxy head block to all web3-proxies and rpcs
        {
            let max_age = self.max_age;
            let max_lag = self.max_lag;
            let rpc = self.web3_proxy.clone();

            let mut others = self.other_proxy.clone();

            others.extend(self.other_rpc.clone());

            let loop_f = a_loop(seconds, log::Level::Error, move || {
                compare::main(rpc.clone(), others.clone(), max_age, max_lag)
            });

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

async fn a_loop<T>(seconds: u64, error_level: log::Level, f: impl Fn() -> T) -> anyhow::Result<()>
where
    T: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let mut interval = interval(Duration::from_secs(seconds));

    // TODO: should we warn if there are delays?
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;

        if let Err(err) = f().await {
            log::log!(error_level, "check failed: {:?}", err);
        };
    }
}
