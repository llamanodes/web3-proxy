use crate::{
    config::TopConfig,
    pagerduty::{pagerduty_alert, pagerduty_alert_for_config},
};
use argh::FromArgs;
use pagerduty_rs::{eventsv2async::EventsV2 as PagerdutyAsyncEventsV2, types::Event};
use serde_json::json;
use tracing::{error, info};

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Quickly create a pagerduty alert
#[argh(subcommand, name = "pagerduty")]
pub struct PagerdutySubCommand {
    #[argh(positional)]
    /// short description of the alert
    summary: String,

    /// the chain id to require. Only used if not using --config.
    #[argh(option)]
    chain_id: Option<u64>,

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
                pagerduty_alert_for_config(
                    self.class.clone(),
                    self.component.clone(),
                    None::<()>,
                    pagerduty_rs::types::Severity::Error,
                    self.summary.clone(),
                    None,
                    top_config,
                )
            })
            .unwrap_or_else(|| {
                pagerduty_alert(
                    self.chain_id,
                    self.class,
                    None,
                    None,
                    self.component,
                    None::<()>,
                    pagerduty_rs::types::Severity::Error,
                    None,
                    self.summary,
                    None,
                )
            });

        if let Some(pagerduty_async) = pagerduty_async {
            info!("sending to pagerduty: {:#}", json!(&event));

            if let Err(err) = pagerduty_async.event(Event::AlertTrigger(event)).await {
                error!("Failed sending to pagerduty: {}", err);
            }
        } else {
            info!(
                "would send to pagerduty if PAGERDUTY_INTEGRATION_KEY were set: {:#}",
                json!(&event)
            );
        }

        Ok(())
    }
}
