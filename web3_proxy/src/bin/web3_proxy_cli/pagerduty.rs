use argh::FromArgs;
use log::{error, info};
use pagerduty_rs::{eventsv2async::EventsV2 as PagerdutyAsyncEventsV2, types::Event};
use web3_proxy::{
    config::TopConfig,
    pagerduty::{pagerduty_alert, pagerduty_event_for_config},
};

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Quickly create a pagerduty alert
#[argh(subcommand, name = "pagerduty")]
pub struct PagerdutySubCommand {
    #[argh(positional)]
    /// short description of the alert
    summary: String,

    #[argh(option)]
    /// the class/type of the event
    class: Option<String>,

    #[argh(option)]
    /// the component of the event
    component: Option<String>,

    #[argh(option)]
    /// deduplicate alerts based on this key.
    /// If there are no open incidents with this key, a new incident will be created.
    /// If there is an open incident with a matching key, the new event will be appended to that incident's Alerts log as an additional Trigger log entry.
    dedup_key: Option<String>,

    #[argh(option, default = "\"web3-proxy\".to_string()")]
    /// a cluster or grouping of sources.
    /// For example, sources "ethereum-proxy" and "polygon-proxy" might both be part of "web3-proxy".
    group: String,
}

impl PagerdutySubCommand {
    pub async fn main(
        self,
        pagerduty_async: Option<PagerdutyAsyncEventsV2>,
        top_config: Option<TopConfig>,
    ) -> anyhow::Result<()> {
        // TODO: allow customizing severity
        let event = top_config
            .map(|top_config| {
                pagerduty_event_for_config(
                    self.class.clone(),
                    self.component.clone(),
                    None::<()>,
                    Some(self.group.clone()),
                    pagerduty_rs::types::Severity::Error,
                    self.summary.clone(),
                    None,
                    top_config,
                )
            })
            .unwrap_or_else(|| {
                pagerduty_alert(
                    None,
                    self.class,
                    "web3-proxy".to_string(),
                    None,
                    self.component,
                    None::<()>,
                    Some(self.group),
                    pagerduty_rs::types::Severity::Error,
                    None,
                    self.summary,
                    None,
                )
            });

        if let Some(pagerduty_async) = pagerduty_async {
            info!(
                "sending to pagerduty: {}",
                serde_json::to_string_pretty(&event)?
            );

            if let Err(err) = pagerduty_async.event(Event::AlertTrigger(event)).await {
                error!("Failed sending to pagerduty: {}", err);
            }
        } else {
            info!(
                "would send to pagerduty if PAGERDUTY_INTEGRATION_KEY were set: {}",
                serde_json::to_string_pretty(&event)?
            );
        }

        Ok(())
    }
}
