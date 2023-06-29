use crate::config::TopConfig;
use pagerduty_rs::eventsv2sync::EventsV2 as PagerdutySyncEventsV2;
use pagerduty_rs::types::{AlertTrigger, AlertTriggerPayload, Event};
use serde::Serialize;
use std::backtrace::Backtrace;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    panic::PanicInfo,
};
use time_03::OffsetDateTime;
use tracing::{debug, error, warn};

/*

        let client = top_config
            .as_ref()
            .map(|top_config| format!("web3-proxy chain #{}", top_config.app.chain_id))
            .unwrap_or_else(|| format!("web3-proxy w/o chain"));

        let client_url = top_config
            .as_ref()
            .and_then(|x| x.app.redirect_public_url.clone());

        panic::set_hook(Box::new(move |x| {
            let hostname = hostname.get().into_string().unwrap_or("unknown".to_string());
            let panic_msg = format!("{} {:?}", x, x);

            if panic_msg.starts_with("panicked at 'WS Server panic") {
                info!("Underlying library {}", panic_msg);
            } else {
                error!("sending panic to pagerduty: {}", panic_msg);

                let mut s = DefaultHasher::new();
                panic_msg.hash(&mut s);
                panic_msg.hash(&mut s);
                let dedup_key = s.finish().to_string();

                let payload = AlertTriggerPayload {
                    severity: pagerduty_rs::types::Severity::Error,
                    summary: panic_msg,
                    source: hostname,
                    timestamp: None,
                    component: None,
                    group: Some("web3-proxy".to_string()),
                    class: Some("panic".to_string()),
                    custom_details: None::<()>,
                };

                let event = Event::AlertTrigger(AlertTrigger {
                    payload,
                    dedup_key: Some(dedup_key),
                    images: None,
                    links: None,
                    client: Some(client.clone()),
                    client_url: client_url.clone(),
                });

                if let Err(err) = pagerduty_sync.event(event) {
                    error!("Failed sending panic to pagerduty: {}", err);
                }
            }
        }));

*/

pub fn panic_handler(
    top_config: Option<TopConfig>,
    pagerduty_sync: &PagerdutySyncEventsV2,
    panic_info: &PanicInfo,
) {
    let summary = format!("{}", panic_info);

    let backtrace = Backtrace::force_capture();

    // TODO: try to send to sentry and then put the sentry link into the page
    let details = format!("{:#?}\n{:#?}", panic_info, backtrace);

    if summary.starts_with("panicked at 'WS Server panic") {
        // the ethers-rs library panics when websockets disconnect. this isn't a panic we care about reporting
        debug!("Underlying library {}", details);
        return;
    }

    let class = Some("panic".to_string());

    let alert = if let Some(top_config) = top_config {
        pagerduty_alert_for_config(
            class,
            None,
            Some(details),
            pagerduty_rs::types::Severity::Critical,
            summary,
            None,
            top_config,
        )
    } else {
        pagerduty_alert(
            None,
            class,
            None,
            None,
            None,
            Some(details),
            pagerduty_rs::types::Severity::Critical,
            None,
            summary,
            None,
        )
    };

    let event = Event::AlertTrigger(alert);

    if let Err(err) = pagerduty_sync.event(event) {
        error!(?err, "Failed sending alert to pagerduty!");
    }
}

pub fn pagerduty_alert_for_config<T: Serialize>(
    class: Option<String>,
    component: Option<String>,
    custom_details: Option<T>,
    severity: pagerduty_rs::types::Severity,
    summary: String,
    timestamp: Option<OffsetDateTime>,
    top_config: TopConfig,
) -> AlertTrigger<T> {
    let chain_id = top_config.app.chain_id;

    let client_url = top_config.app.redirect_public_url;

    pagerduty_alert(
        Some(chain_id),
        class,
        None,
        client_url,
        component,
        custom_details,
        severity,
        None,
        summary,
        timestamp,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn pagerduty_alert<T: Serialize>(
    chain_id: Option<u64>,
    class: Option<String>,
    client: Option<String>,
    client_url: Option<String>,
    component: Option<String>,
    custom_details: Option<T>,
    severity: pagerduty_rs::types::Severity,
    source: Option<String>,
    summary: String,
    timestamp: Option<OffsetDateTime>,
) -> AlertTrigger<T> {
    let client = client.unwrap_or_else(|| "web3-proxy".to_string());

    let group = chain_id.map(|x| format!("chain #{}", x));

    let source = source.unwrap_or_else(|| {
        hostname::get()
            .unwrap()
            .into_string()
            .unwrap_or_else(|err| {
                warn!(?err, "unable to handle hostname");
                "unknown".to_string()
            })
    });

    let mut s = DefaultHasher::new();
    // TODO: include severity here?
    summary.hash(&mut s);
    client.hash(&mut s);
    client_url.hash(&mut s);
    component.hash(&mut s);
    group.hash(&mut s);
    class.hash(&mut s);
    let dedup_key = s.finish().to_string();

    let payload = AlertTriggerPayload {
        severity,
        summary,
        source,
        timestamp,
        component,
        group,
        class,
        custom_details,
    };

    AlertTrigger {
        payload,
        dedup_key: Some(dedup_key),
        images: None,
        links: None,
        client: Some(client),
        client_url,
    }
}
