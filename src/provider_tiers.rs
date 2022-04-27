/// Communicate with groups of web3 providers
use dashmap::DashMap;
use governor::clock::{QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::NotUntil;
use governor::RateLimiter;
use std::cmp;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, instrument};

use crate::block_watcher::{BlockWatcher, SyncStatus};
use crate::provider::Web3Connection;

type Web3RateLimiter =
    RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>;

type Web3RateLimiterMap = DashMap<String, Web3RateLimiter>;

pub type Web3ConnectionMap = DashMap<String, Web3Connection>;

/// Load balance to the rpc
#[derive(Debug)]
pub struct Web3ProviderTier {
    /// RPC urls sorted by active requests
    /// TODO: what type for the rpc? i think we want this to be the key for the provider and not the provider itself
    rpcs: RwLock<Vec<String>>,
    connections: Arc<Web3ConnectionMap>,
    ratelimiters: Web3RateLimiterMap,
}

impl Web3ProviderTier {
    pub async fn try_new(
        servers: Vec<(&str, u32)>,
        http_client: Option<reqwest::Client>,
        block_watcher: Arc<BlockWatcher>,
        clock: &QuantaClock,
    ) -> anyhow::Result<Web3ProviderTier> {
        let mut rpcs: Vec<String> = vec![];
        let connections = DashMap::new();
        let ratelimits = DashMap::new();

        for (s, limit) in servers.into_iter() {
            rpcs.push(s.to_string());

            let connection = Web3Connection::try_new(
                s.to_string(),
                http_client.clone(),
                block_watcher.clone_sender(),
            )
            .await?;

            connections.insert(s.to_string(), connection);

            if limit > 0 {
                let quota = governor::Quota::per_second(NonZeroU32::new(limit).unwrap());

                let rate_limiter = governor::RateLimiter::direct_with_clock(quota, clock);

                ratelimits.insert(s.to_string(), rate_limiter);
            }
        }

        Ok(Web3ProviderTier {
            rpcs: RwLock::new(rpcs),
            connections: Arc::new(connections),
            ratelimiters: ratelimits,
        })
    }

    pub fn clone_connections(&self) -> Arc<Web3ConnectionMap> {
        self.connections.clone()
    }

    /// get the best available rpc server
    #[instrument]
    pub async fn next_upstream_server(
        &self,
        allowed_lag: u64,
        block_watcher: Arc<BlockWatcher>,
    ) -> Result<String, NotUntil<QuantaInstant>> {
        let mut available_rpcs = self.rpcs.write().await;

        // sort rpcs by their active connections
        available_rpcs.sort_unstable_by(|a, b| {
            self.connections
                .get(a)
                .unwrap()
                .cmp(&self.connections.get(b).unwrap())
        });

        // sort rpcs by their block height
        available_rpcs.sort_unstable_by(|a, b| {
            let a_synced = block_watcher.sync_status(a, allowed_lag);
            let b_synced = block_watcher.sync_status(b, allowed_lag);

            match (a_synced, b_synced) {
                (SyncStatus::Synced(a), SyncStatus::Synced(b)) => {
                    if a != b {
                        return a.cmp(&b);
                    }
                    // else they are equal and we want to compare on active connections
                }
                (SyncStatus::Synced(_), SyncStatus::Unknown) => {
                    return cmp::Ordering::Greater;
                }
                (SyncStatus::Unknown, SyncStatus::Synced(_)) => {
                    return cmp::Ordering::Less;
                }
                (SyncStatus::Unknown, SyncStatus::Unknown) => {
                    // neither rpc is synced
                    // this means neither will have connections
                    return cmp::Ordering::Equal;
                }
                (SyncStatus::Synced(_), SyncStatus::Behind(_)) => {
                    return cmp::Ordering::Greater;
                }
                (SyncStatus::Behind(_), SyncStatus::Synced(_)) => {
                    return cmp::Ordering::Less;
                }
                (SyncStatus::Behind(_), SyncStatus::Unknown) => {
                    return cmp::Ordering::Greater;
                }
                (SyncStatus::Behind(a), SyncStatus::Behind(b)) => {
                    if a != b {
                        return a.cmp(&b);
                    }
                    // else they are equal and we want to compare on active connections
                }
                (SyncStatus::Unknown, SyncStatus::Behind(_)) => {
                    return cmp::Ordering::Less;
                }
            }

            // sort on active connections
            self.connections
                .get(a)
                .unwrap()
                .cmp(&self.connections.get(b).unwrap())
        });

        let mut earliest_not_until = None;

        for selected_rpc in available_rpcs.iter() {
            // check current block number
            // TODO: i don't like that we fetched sync_status above and then do it again here. cache?
            if let SyncStatus::Synced(_) = block_watcher.sync_status(selected_rpc, allowed_lag) {
                // rpc is synced
            } else {
                // skip this rpc because it is not synced
                // TODO: make a NotUntil here?
                // TODO: include how many blocks behind
                // TODO: better log
                info!("{} is not synced", selected_rpc);
                // we sorted on block height. so if this one isn't synced, none of the later ones will be either
                break;
            }

            // check rate limits
            if let Some(ratelimiter) = self.ratelimiters.get(selected_rpc) {
                match ratelimiter.check() {
                    Ok(_) => {
                        // rate limit succeeded
                    }
                    Err(not_until) => {
                        // rate limit failed
                        // save the smallest not_until. if nothing succeeds, return an Err with not_until in it
                        // TODO: use tracing better
                        info!("Exhausted rate limit on {}: {}", selected_rpc, not_until);

                        if earliest_not_until.is_none() {
                            earliest_not_until = Some(not_until);
                        } else {
                            let earliest_possible =
                                earliest_not_until.as_ref().unwrap().earliest_possible();
                            let new_earliest_possible = not_until.earliest_possible();

                            if earliest_possible > new_earliest_possible {
                                earliest_not_until = Some(not_until);
                            }
                        }
                        continue;
                    }
                }
            };

            // increment our connection counter
            self.connections
                .get_mut(selected_rpc)
                .unwrap()
                .inc_active_requests();

            // return the selected RPC
            return Ok(selected_rpc.clone());
        }

        // return the smallest not_until
        if let Some(not_until) = earliest_not_until {
            Err(not_until)
        } else {
            unimplemented!();
        }
    }

    /// get all available rpc servers
    pub async fn get_upstream_servers(
        &self,
        allowed_lag: u64,
        block_watcher: Arc<BlockWatcher>,
    ) -> Result<Vec<String>, NotUntil<QuantaInstant>> {
        let mut earliest_not_until = None;

        let mut selected_rpcs = vec![];

        for selected_rpc in self.rpcs.read().await.iter() {
            if let SyncStatus::Synced(_) = block_watcher.sync_status(selected_rpc, allowed_lag) {
                // rpc is synced
            } else {
                // skip this rpc because it is not synced
                continue;
            }

            // check rate limits
            match self.ratelimiters.get(selected_rpc).unwrap().check() {
                Ok(_) => {
                    // rate limit succeeded
                }
                Err(not_until) => {
                    // rate limit failed
                    // save the smallest not_until. if nothing succeeds, return an Err with not_until in it
                    if earliest_not_until.is_none() {
                        earliest_not_until = Some(not_until);
                    } else {
                        let earliest_possible =
                            earliest_not_until.as_ref().unwrap().earliest_possible();
                        let new_earliest_possible = not_until.earliest_possible();

                        if earliest_possible > new_earliest_possible {
                            earliest_not_until = Some(not_until);
                        }
                    }
                    continue;
                }
            };

            // increment our connection counter
            self.connections
                .get_mut(selected_rpc)
                .unwrap()
                .inc_active_requests();

            // this is rpc should work
            selected_rpcs.push(selected_rpc.clone());
        }

        if !selected_rpcs.is_empty() {
            return Ok(selected_rpcs);
        }

        // return the earliest not_until
        if let Some(not_until) = earliest_not_until {
            Err(not_until)
        } else {
            Ok(vec![])
        }
    }
}
