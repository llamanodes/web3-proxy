use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use crate::config::TopConfig;
use gethostname::gethostname;
use pagerduty_rs::types::{AlertTrigger, AlertTriggerPayload};
use serde::Serialize;
use time::OffsetDateTime;

pub fn pagerduty_event_for_config<T: Serialize>(
    class: Option<String>,
    component: Option<String>,
    custom_details: Option<T>,
    group: Option<String>,
    severity: pagerduty_rs::types::Severity,
    summary: String,
    timestamp: Option<OffsetDateTime>,
    top_config: TopConfig,
) -> AlertTrigger<T> {
    let chain_id = top_config.app.chain_id;

    let client_url = top_config.app.redirect_public_url.clone();

    pagerduty_alert(
        Some(chain_id),
        class,
        "web3-proxy".to_string(),
        client_url,
        component,
        custom_details,
        group,
        severity,
        None,
        summary,
        timestamp,
    )
}

pub fn pagerduty_alert<T: Serialize>(
    chain_id: Option<u64>,
    class: Option<String>,
    client: String,
    client_url: Option<String>,
    component: Option<String>,
    custom_details: Option<T>,
    group: Option<String>,
    severity: pagerduty_rs::types::Severity,
    source: Option<String>,
    summary: String,
    timestamp: Option<OffsetDateTime>,
) -> AlertTrigger<T> {
    let client = chain_id
        .map(|x| format!("{} chain #{}", client, x))
        .unwrap_or_else(|| format!("{} w/o chain", client));

    let source =
        source.unwrap_or_else(|| gethostname().into_string().unwrap_or("unknown".to_string()));

    let mut s = DefaultHasher::new();
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
        client_url: client_url,
    }
}
