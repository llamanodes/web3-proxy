use argh::FromArgs;
use gethostname::gethostname;
use log::{error, info};
use pagerduty_rs::{
    eventsv2async::EventsV2 as PagerdutyAsyncEventsV2,
    types::{AlertTrigger, AlertTriggerPayload, Event},
};
use web3_proxy::config::TopConfig;

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
        &self,
        pagerduty_async: Option<PagerdutyAsyncEventsV2>,
        top_config: Option<TopConfig>,
    ) -> anyhow::Result<()> {
        let client = top_config
            .as_ref()
            .map(|top_config| format!("web3-proxy chain #{}", top_config.app.chain_id))
            .unwrap_or_else(|| format!("web3-proxy w/o chain"));

        let client_url = top_config
            .as_ref()
            .and_then(|x| x.app.redirect_public_url.clone());

        let hostname = gethostname().into_string().unwrap_or("unknown".to_string());

        let payload = AlertTriggerPayload {
            severity: pagerduty_rs::types::Severity::Error,
            summary: self.summary.clone(),
            source: hostname,
            timestamp: None,
            component: None,
            group: Some(self.group.clone()),
            class: self.class.clone(),
            custom_details: None::<()>,
        };

        let event = AlertTrigger {
            payload,
            dedup_key: None,
            images: None,
            links: None,
            client: Some(client),
            client_url: client_url,
        };

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
