use crate::frontend::authorization::Authorization;

use super::blockchain::Web3ProxyBlock;
use super::many::Web3Rpcs;
use super::one::Web3Rpc;
use anyhow::Context;
use ethers::prelude::{H256, U64};
use hashbrown::{HashMap, HashSet};
use itertools::{Itertools, MinMaxResult};
use log::{trace, warn};
use moka::future::Cache;
use serde::Serialize;
use std::cmp::Reverse;
use std::fmt;
use std::sync::Arc;
use tokio::time::Instant;

/// A collection of Web3Rpcs that are on the same block.
/// Serialize is so we can print it on our debug endpoint
#[derive(Clone, Serialize)]
pub struct ConsensusWeb3Rpcs {
    pub(crate) tier: u64,
    pub(crate) head_block: Web3ProxyBlock,
    pub(crate) best_rpcs: Vec<Arc<Web3Rpc>>,
    // TODO: functions like "compare_backup_vote()"
    // pub(super) backups_voted: Option<Web3ProxyBlock>,
    pub(crate) backups_needed: bool,
}

impl ConsensusWeb3Rpcs {
    pub fn num_conns(&self) -> usize {
        self.best_rpcs.len()
    }

    // TODO: sum_hard_limit?
}

impl fmt::Debug for ConsensusWeb3Rpcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        // TODO: print the actual conns?
        f.debug_struct("ConsensusWeb3Rpcs")
            .field("head_block", &self.head_block)
            .field("num_conns", &self.best_rpcs.len())
            .finish_non_exhaustive()
    }
}

impl Web3Rpcs {
    // TODO: return a ref?
    pub fn head_block(&self) -> Option<Web3ProxyBlock> {
        self.watch_consensus_head_sender
            .as_ref()
            .and_then(|x| x.borrow().clone())
    }

    // TODO: return a ref?
    pub fn head_block_hash(&self) -> Option<H256> {
        self.head_block().map(|x| *x.hash())
    }

    // TODO: return a ref?
    pub fn head_block_num(&self) -> Option<U64> {
        self.head_block().map(|x| *x.number())
    }

    pub fn synced(&self) -> bool {
        let consensus = self.watch_consensus_rpcs_sender.borrow();

        if let Some(consensus) = consensus.as_ref() {
            !consensus.best_rpcs.is_empty()
        } else {
            false
        }
    }

    pub fn num_synced_rpcs(&self) -> usize {
        let consensus = self.watch_consensus_rpcs_sender.borrow();

        if let Some(consensus) = consensus.as_ref() {
            consensus.best_rpcs.len()
        } else {
            0
        }
    }
}

type FirstSeenCache = Cache<H256, Instant, hashbrown::hash_map::DefaultHashBuilder>;

/// A ConsensusConnections builder that tracks all connection heads across multiple groups of servers
pub struct ConsensusFinder {
    /// backups for all tiers are only used if necessary
    /// tiers[0] = only tier 0.
    /// tiers[1] = tier 0 and tier 1
    /// tiers[n] = tier 0..=n
    /// This is a BTreeMap and not a Vec because sometimes a tier is empty
    rpc_heads: HashMap<Arc<Web3Rpc>, Web3ProxyBlock>,
    /// never serve blocks that are too old
    max_block_age: Option<u64>,
    /// tier 0 will be prefered as long as the distance between it and the other tiers is <= max_tier_lag
    max_block_lag: Option<U64>,
    /// used to track rpc.head_latency. The same cache should be shared between all ConnectionsGroups
    first_seen: FirstSeenCache,
}

impl ConsensusFinder {
    pub fn new(max_block_age: Option<u64>, max_block_lag: Option<U64>) -> Self {
        // TODO: what's a good capacity for this? it shouldn't need to be very large
        // TODO: if we change Web3ProxyBlock to store the instance, i think we could use the block_by_hash cache
        let first_seen = Cache::builder()
            .max_capacity(16)
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // TODO: hard coding 0-9 isn't great, but its easier than refactoring this to be smart about config reloading
        let rpc_heads = HashMap::new();

        Self {
            rpc_heads,
            max_block_age,
            max_block_lag,
            first_seen,
        }
    }

    pub fn len(&self) -> usize {
        self.rpc_heads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rpc_heads.is_empty()
    }

    fn remove(&mut self, rpc: &Arc<Web3Rpc>) -> Option<Web3ProxyBlock> {
        self.rpc_heads.remove(rpc)
    }

    async fn insert(&mut self, rpc: Arc<Web3Rpc>, block: Web3ProxyBlock) -> Option<Web3ProxyBlock> {
        let first_seen = self
            .first_seen
            .get_with(*block.hash(), async move { Instant::now() })
            .await;

        // TODO: this should be 0 if we are first seen, but i think it will be slightly non-zero.
        // calculate elapsed time before trying to lock.
        let latency = first_seen.elapsed();

        rpc.head_latency.write().record(latency);

        self.rpc_heads.insert(rpc, block)
    }

    /// Update our tracking of the rpc and return true if something changed
    pub(crate) async fn update_rpc(
        &mut self,
        rpc_head_block: Option<Web3ProxyBlock>,
        rpc: Arc<Web3Rpc>,
        // we need this so we can save the block to caches. i don't like it though. maybe we should use a lazy_static Cache wrapper that has a "save_block" method?. i generally dislike globals but i also dislike all the types having to pass eachother around
        web3_connections: &Web3Rpcs,
    ) -> anyhow::Result<bool> {
        // add the rpc's block to connection_heads, or remove the rpc from connection_heads
        let changed = match rpc_head_block {
            Some(mut rpc_head_block) => {
                // we don't know if its on the heaviest chain yet
                rpc_head_block = web3_connections
                    .try_cache_block(rpc_head_block, false)
                    .await
                    .context("failed caching block")?;

                // if let Some(max_block_lag) = max_block_lag {
                //     if rpc_head_block.number() < ??? {
                //         trace!("rpc_head_block from {} is too far behind! {}", rpc, rpc_head_block);
                //         return Ok(self.remove(&rpc).is_some());
                //      }
                // }

                if let Some(max_age) = self.max_block_age {
                    if rpc_head_block.age() > max_age {
                        trace!("rpc_head_block from {} is too old! {}", rpc, rpc_head_block);
                        return Ok(self.remove(&rpc).is_some());
                    }
                }

                if let Some(prev_block) = self.insert(rpc, rpc_head_block.clone()).await {
                    // false if this block was already sent by this rpc
                    // true if new block for this rpc
                    prev_block.hash() != rpc_head_block.hash()
                } else {
                    // first block for this rpc
                    true
                }
            }
            None => {
                // false if this rpc was already removed
                // true if rpc head changed from being synced to not
                self.remove(&rpc).is_some()
            }
        };

        Ok(changed)
    }

    pub async fn find_consensus_connections(
        &mut self,
        authorization: &Arc<Authorization>,
        web3_rpcs: &Web3Rpcs,
    ) -> anyhow::Result<Option<ConsensusWeb3Rpcs>> {
        let minmax_block = self.rpc_heads.values().minmax_by_key(|&x| x.number());

        let (lowest_block, highest_block) = match minmax_block {
            MinMaxResult::NoElements => return Ok(None),
            MinMaxResult::OneElement(x) => (x, x),
            MinMaxResult::MinMax(min, max) => (min, max),
        };

        let highest_block_number = highest_block.number();

        trace!("highest_block_number: {}", highest_block_number);

        trace!("lowest_block_number: {}", lowest_block.number());

        let max_lag_block_number = highest_block_number
            .saturating_sub(self.max_block_lag.unwrap_or_else(|| U64::from(10)));

        trace!("max_lag_block_number: {}", max_lag_block_number);

        let lowest_block_number = lowest_block.number().max(&max_lag_block_number);

        trace!("safe lowest_block_number: {}", lowest_block_number);

        let num_known = self.rpc_heads.len();

        if num_known < web3_rpcs.min_head_rpcs {
            // this keeps us from serving requests when the proxy first starts
            return Ok(None);
        }

        // TODO: also track the sum of *available* hard_limits? if any servers have no hard limits, use their soft limit or no limit?
        // TODO: struct for the value of the votes hashmap?
        let mut primary_votes: HashMap<Web3ProxyBlock, (HashSet<&str>, u32)> = Default::default();
        let mut backup_votes: HashMap<Web3ProxyBlock, (HashSet<&str>, u32)> = Default::default();

        let mut backup_consensus = None;

        let mut rpc_heads_by_tier: Vec<_> = self.rpc_heads.iter().collect();
        rpc_heads_by_tier.sort_by_cached_key(|(rpc, _)| rpc.tier);

        let current_tier = rpc_heads_by_tier
            .first()
            .expect("rpc_heads_by_tier should never be empty")
            .0
            .tier;

        // loop over all the rpc heads (grouped by tier) and their parents to find consensus
        // TODO: i'm sure theres a lot of shortcuts that could be taken, but this is simplest to implement
        for (rpc, rpc_head) in self.rpc_heads.iter() {
            if current_tier != rpc.tier {
                // we finished processing a tier. check for primary results
                if let Some(consensus) = self.count_votes(&primary_votes, web3_rpcs) {
                    return Ok(Some(consensus));
                }

                // only set backup consensus once. we don't want it to keep checking on worse tiers if it already found consensus
                if backup_consensus.is_none() {
                    if let Some(consensus) = self.count_votes(&backup_votes, web3_rpcs) {
                        backup_consensus = Some(consensus)
                    }
                }
            }

            let mut block_to_check = rpc_head.clone();

            while block_to_check.number() >= lowest_block_number {
                if !rpc.backup {
                    // backup nodes are excluded from the primary voting
                    let entry = primary_votes.entry(block_to_check.clone()).or_default();

                    entry.0.insert(&rpc.name);
                    entry.1 += rpc.soft_limit;
                }

                // both primary and backup rpcs get included in the backup voting
                let backup_entry = backup_votes.entry(block_to_check.clone()).or_default();

                backup_entry.0.insert(&rpc.name);
                backup_entry.1 += rpc.soft_limit;

                match web3_rpcs
                    .block(authorization, block_to_check.parent_hash(), Some(rpc))
                    .await
                {
                    Ok(parent_block) => block_to_check = parent_block,
                    Err(err) => {
                        warn!("Problem fetching parent block of {:#?} during consensus finding: {:#?}", block_to_check, err);
                        break;
                    }
                }
            }
        }

        // we finished processing all tiers. check for primary results (if anything but the last tier found consensus, we already returned above)
        if let Some(consensus) = self.count_votes(&primary_votes, web3_rpcs) {
            return Ok(Some(consensus));
        }

        // only set backup consensus once. we don't want it to keep checking on worse tiers if it already found consensus
        if let Some(consensus) = backup_consensus {
            return Ok(Some(consensus));
        }

        // count votes one last time
        Ok(self.count_votes(&backup_votes, web3_rpcs))
    }

    // TODO: have min_sum_soft_limit and min_head_rpcs on self instead of on Web3Rpcs
    fn count_votes(
        &self,
        votes: &HashMap<Web3ProxyBlock, (HashSet<&str>, u32)>,
        web3_rpcs: &Web3Rpcs,
    ) -> Option<ConsensusWeb3Rpcs> {
        // sort the primary votes ascending by tier and descending by block num
        let mut votes: Vec<_> = votes
            .iter()
            .map(|(block, (rpc_names, sum_soft_limit))| (block, sum_soft_limit, rpc_names))
            .collect();
        votes.sort_by_cached_key(|(block, sum_soft_limit, rpc_names)| {
            (
                Reverse(*block.number()),
                Reverse(*sum_soft_limit),
                Reverse(rpc_names.len()),
            )
        });

        // return the first result that exceededs confgured minimums (if any)
        for (maybe_head_block, sum_soft_limit, rpc_names) in votes {
            if *sum_soft_limit < web3_rpcs.min_sum_soft_limit {
                continue;
            }
            // TODO: different mins for backup vs primary
            if rpc_names.len() < web3_rpcs.min_head_rpcs {
                continue;
            }

            trace!("rpc_names: {:#?}", rpc_names);

            // consensus likely found! load the rpcs to make sure they all have active connections
            let consensus_rpcs: Vec<_> = rpc_names
                .into_iter()
                .filter_map(|x| web3_rpcs.get(x))
                .collect();

            if consensus_rpcs.len() < web3_rpcs.min_head_rpcs {
                continue;
            }
            // consensus found!

            let tier = consensus_rpcs
                .iter()
                .map(|x| x.tier)
                .max()
                .expect("there should always be a max");

            let backups_needed = consensus_rpcs.iter().any(|x| x.backup);

            let consensus = ConsensusWeb3Rpcs {
                tier,
                head_block: maybe_head_block.clone(),
                best_rpcs: consensus_rpcs,
                backups_needed,
            };

            return Some(consensus);
        }

        None
    }

    pub fn worst_tier(&self) -> Option<u64> {
        self.rpc_heads.iter().map(|(x, _)| x.tier).max()
    }
}

#[cfg(test)]
mod test {
    // #[test]
    // fn test_simplest_case_consensus_head_connections() {
    //     todo!();
    // }
}
