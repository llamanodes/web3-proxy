/// Communicate with groups of web3 providers
use dashmap::DashMap;
use governor::clock::{QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::NotUntil;
use governor::RateLimiter;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::block_watcher::BlockWatcher;
use crate::provider::Web3Connection;

type Web3RateLimiter =
    RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>;

type Web3RateLimiterMap = DashMap<String, Web3RateLimiter>;

pub type Web3ConnectionMap = DashMap<String, Web3Connection>;

/// Load balance to the rpc
/// TODO: i'm not sure about having 3 locks here. can we share them?
pub struct Web3ProviderTier {
    /// RPC urls sorted by active requests
    /// TODO: what type for the rpc?
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
    pub async fn next_upstream_server(
        &self,
        block_watcher: Arc<BlockWatcher>,
    ) -> Result<String, NotUntil<QuantaInstant>> {
        let mut balanced_rpcs = self.rpcs.write().await;

        // sort rpcs by their active connections
        balanced_rpcs.sort_unstable_by(|a, b| {
            self.connections
                .get(a)
                .unwrap()
                .cmp(&self.connections.get(b).unwrap())
        });

        let mut earliest_not_until = None;

        for selected_rpc in balanced_rpcs.iter() {
            // TODO: check current block number. if too far behind, make our own NotUntil here
            if !block_watcher
                .is_synced(selected_rpc.clone(), 3)
                .await
                .expect("checking is_synced failed")
            {
                // skip this rpc because it is not synced
                continue;
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
        block_watcher: Arc<BlockWatcher>,
    ) -> Result<Vec<String>, NotUntil<QuantaInstant>> {
        let mut earliest_not_until = None;

        let mut selected_rpcs = vec![];

        for selected_rpc in self.rpcs.read().await.iter() {
            // check that the server is synced
            if !block_watcher
                .is_synced(selected_rpc.clone(), 1)
                .await
                .expect("checking is_synced failed")
            {
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
            // TODO: is this right?
            Ok(vec![])
        }
    }
}
