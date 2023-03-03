use crate::frontend::authorization::Authorization;

use super::blockchain::Web3ProxyBlock;
use super::many::Web3Rpcs;
use super::one::Web3Rpc;
use anyhow::Context;
use ethers::prelude::{H256, U64};
use hashbrown::{HashMap, HashSet};
use log::{debug, trace, warn};
use moka::future::Cache;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tokio::time::Instant;

/// A collection of Web3Rpcs that are on the same block.
/// Serialize is so we can print it on our debug endpoint
#[derive(Clone, Serialize)]
pub struct ConsensusWeb3Rpcs {
    // TODO: tier should be an option, or we should have consensus be stored as an Option<ConsensusWeb3Rpcs>
    pub(super) tier: u64,
    pub(super) head_block: Web3ProxyBlock,
    // TODO: this should be able to serialize, but it isn't
    #[serde(skip_serializing)]
    pub(super) rpcs: Vec<Arc<Web3Rpc>>,
    pub(super) backups_voted: Option<Web3ProxyBlock>,
    pub(super) backups_needed: bool,
}

impl ConsensusWeb3Rpcs {
    pub fn num_conns(&self) -> usize {
        self.rpcs.len()
    }

    pub fn sum_soft_limit(&self) -> u32 {
        self.rpcs.iter().fold(0, |sum, rpc| sum + rpc.soft_limit)
    }

    // TODO: sum_hard_limit?
}

impl fmt::Debug for ConsensusWeb3Rpcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        // TODO: print the actual conns?
        f.debug_struct("ConsensusConnections")
            .field("head_block", &self.head_block)
            .field("num_conns", &self.rpcs.len())
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
            !consensus.rpcs.is_empty()
        } else {
            false
        }
    }

    pub fn num_synced_rpcs(&self) -> usize {
        let consensus = self.watch_consensus_rpcs_sender.borrow();

        if let Some(consensus) = consensus.as_ref() {
            consensus.rpcs.len()
        } else {
            0
        }
    }
}

type FirstSeenCache = Cache<H256, Instant, hashbrown::hash_map::DefaultHashBuilder>;

pub struct ConnectionsGroup {
    pub rpc_to_block: HashMap<Arc<Web3Rpc>, Web3ProxyBlock>,
    // TODO: what if there are two blocks with the same number?
    pub highest_block: Option<Web3ProxyBlock>,
    /// used to track rpc.head_latency. The same cache should be shared between all ConnectionsGroups
    first_seen: FirstSeenCache,
}

impl ConnectionsGroup {
    pub fn new(first_seen: FirstSeenCache) -> Self {
        Self {
            rpc_to_block: Default::default(),
            highest_block: Default::default(),
            first_seen,
        }
    }

    pub fn len(&self) -> usize {
        self.rpc_to_block.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rpc_to_block.is_empty()
    }

    fn remove(&mut self, rpc: &Arc<Web3Rpc>) -> Option<Web3ProxyBlock> {
        if let Some(removed_block) = self.rpc_to_block.remove(rpc) {
            match self.highest_block.as_mut() {
                None => {}
                Some(current_highest_block) => {
                    if removed_block.hash() == current_highest_block.hash() {
                        for maybe_highest_block in self.rpc_to_block.values() {
                            if maybe_highest_block.number() > current_highest_block.number() {
                                *current_highest_block = maybe_highest_block.clone();
                            };
                        }
                    }
                }
            }

            Some(removed_block)
        } else {
            None
        }
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

        // TODO: what about a reorg to the same height?
        if Some(block.number()) > self.highest_block.as_ref().map(|x| x.number()) {
            self.highest_block = Some(block.clone());
        }

        self.rpc_to_block.insert(rpc, block)
    }

    /// min_consensus_block_num keeps us from ever going backwards.
    /// TODO: think about min_consensus_block_num more. i think this might cause an outage if the chain is doing weird things. but 503s is probably better than broken data.
    pub(self) async fn consensus_head_connections(
        &self,
        authorization: &Arc<Authorization>,
        web3_rpcs: &Web3Rpcs,
        min_consensus_block_num: Option<U64>,
        tier: &u64,
    ) -> anyhow::Result<ConsensusWeb3Rpcs> {
        let mut maybe_head_block = match self.highest_block.clone() {
            None => return Err(anyhow::anyhow!("no blocks known")),
            Some(x) => x,
        };

        // TODO: take max_distance_consensus_to_highest as an argument?
        // TODO: what if someone's backup node is misconfigured and goes on a really fast forked chain?
        let max_lag_consensus_to_highest =
            if let Some(min_consensus_block_num) = min_consensus_block_num {
                maybe_head_block
                    .number()
                    .saturating_add(1.into())
                    .saturating_sub(min_consensus_block_num)
                    .as_u64()
            } else {
                10
            };

        trace!(
            "max_lag_consensus_to_highest: {}",
            max_lag_consensus_to_highest
        );

        let num_known = self.rpc_to_block.len();

        if num_known < web3_rpcs.min_head_rpcs {
            return Err(anyhow::anyhow!(
                "not enough rpcs connected: {}/{}",
                num_known,
                web3_rpcs.min_head_rpcs,
            ));
        }

        let mut primary_rpcs_voted: Option<Web3ProxyBlock> = None;
        let mut backup_rpcs_voted: Option<Web3ProxyBlock> = None;

        // track rpcs on this heaviest chain so we can build a new ConsensusConnections
        let mut primary_consensus_rpcs = HashSet::<&str>::new();
        let mut backup_consensus_rpcs = HashSet::<&str>::new();

        // a running total of the soft limits covered by the rpcs that agree on the head block
        let mut primary_sum_soft_limit: u32 = 0;
        let mut backup_sum_soft_limit: u32 = 0;

        // TODO: also track the sum of *available* hard_limits. if any servers have no hard limits, use their soft limit or no limit?

        // check the highest work block for a set of rpcs that can serve our request load
        // if it doesn't have enough rpcs for our request load, check the parent block
        // TODO: loop for how many parent blocks? we don't want to serve blocks that are too far behind. probably different per chain
        // TODO: this loop is pretty long. any way to clean up this code?
        for _ in 0..max_lag_consensus_to_highest {
            let maybe_head_hash = maybe_head_block.hash();

            // find all rpcs with maybe_head_hash as their current head
            for (rpc, rpc_head) in self.rpc_to_block.iter() {
                if rpc_head.hash() != maybe_head_hash {
                    // connection is not on the desired block
                    continue;
                }
                let rpc_name = rpc.name.as_str();
                if backup_consensus_rpcs.contains(rpc_name) {
                    // connection is on a later block in this same chain
                    continue;
                }
                if primary_consensus_rpcs.contains(rpc_name) {
                    // connection is on a later block in this same chain
                    continue;
                }

                if let Some(rpc) = web3_rpcs.by_name.read().get(rpc_name) {
                    if backup_rpcs_voted.is_some() {
                        // backups already voted for a head block. don't change it
                    } else {
                        backup_consensus_rpcs.insert(rpc_name);
                        backup_sum_soft_limit += rpc.soft_limit;
                    }
                    if !rpc.backup {
                        primary_consensus_rpcs.insert(rpc_name);
                        primary_sum_soft_limit += rpc.soft_limit;
                    }
                } else {
                    // i don't think this is an error. i think its just if a reconnect is currently happening
                    if web3_rpcs.synced() {
                        warn!("connection missing: {}", rpc_name);
                        debug!("web3_rpcs.by_name: {:#?}", web3_rpcs.by_name);
                    } else {
                        return Err(anyhow::anyhow!("not synced"));
                    }
                }
            }

            if primary_sum_soft_limit >= web3_rpcs.min_sum_soft_limit
                && primary_consensus_rpcs.len() >= web3_rpcs.min_head_rpcs
            {
                // we have enough servers with enough requests! yey!
                primary_rpcs_voted = Some(maybe_head_block.clone());
                break;
            }

            if backup_rpcs_voted.is_none()
                && backup_consensus_rpcs != primary_consensus_rpcs
                && backup_sum_soft_limit >= web3_rpcs.min_sum_soft_limit
                && backup_consensus_rpcs.len() >= web3_rpcs.min_head_rpcs
            {
                // if we include backup servers, we have enough servers with high enough limits
                backup_rpcs_voted = Some(maybe_head_block.clone());
            }

            // not enough rpcs on this block. check the parent block
            match web3_rpcs
                .block(authorization, maybe_head_block.parent_hash(), None)
                .await
            {
                Ok(parent_block) => {
                    // trace!(
                    //     child=%maybe_head_hash, parent=%parent_block.hash.unwrap(), "avoiding thundering herd. checking consensus on parent block",
                    // );
                    maybe_head_block = parent_block;
                    continue;
                }
                Err(err) => {
                    let soft_limit_percent = (primary_sum_soft_limit as f32
                        / web3_rpcs.min_sum_soft_limit as f32)
                        * 100.0;

                    let err_msg = format!("ran out of parents to check. rpcs {}/{}/{}. soft limit: {:.2}% ({}/{}). err: {:#?}",
                        primary_consensus_rpcs.len(),
                        num_known,
                        web3_rpcs.min_head_rpcs,
                        primary_sum_soft_limit,
                        web3_rpcs.min_sum_soft_limit,
                        soft_limit_percent,
                        err,
                    );

                    if backup_rpcs_voted.is_some() {
                        warn!("{}", err_msg);
                        break;
                    } else {
                        return Err(anyhow::anyhow!(err_msg));
                    }
                }
            }
        }

        // TODO: if consensus_head_rpcs.is_empty, try another method of finding the head block. will need to change the return Err above into breaks.

        // we've done all the searching for the heaviest block that we can
        if (primary_consensus_rpcs.len() < web3_rpcs.min_head_rpcs
            || primary_sum_soft_limit < web3_rpcs.min_sum_soft_limit)
            && backup_rpcs_voted.is_none()
        {
            // if we get here, not enough servers are synced. return an error
            let soft_limit_percent =
                (primary_sum_soft_limit as f32 / web3_rpcs.min_sum_soft_limit as f32) * 100.0;

            return Err(anyhow::anyhow!(
                "Not enough resources. rpcs {}/{}/{}. soft limit: {:.2}% ({}/{})",
                primary_consensus_rpcs.len(),
                num_known,
                web3_rpcs.min_head_rpcs,
                primary_sum_soft_limit,
                web3_rpcs.min_sum_soft_limit,
                soft_limit_percent,
            ));
        }

        // success! this block has enough soft limit and nodes on it (or on later blocks)
        let rpcs: Vec<Arc<Web3Rpc>> = primary_consensus_rpcs
            .into_iter()
            .filter_map(|conn_name| web3_rpcs.by_name.read().get(conn_name).cloned())
            .collect();

        #[cfg(debug_assertions)]
        {
            let _ = maybe_head_block.hash();
            let _ = maybe_head_block.number();
        }

        Ok(ConsensusWeb3Rpcs {
            tier: *tier,
            head_block: maybe_head_block,
            rpcs,
            backups_voted: backup_rpcs_voted,
            backups_needed: primary_rpcs_voted.is_none(),
        })
    }
}

/// A ConsensusConnections builder that tracks all connection heads across multiple groups of servers
pub struct ConsensusFinder {
    /// backups for all tiers are only used if necessary
    /// tiers[0] = only tier 0.
    /// tiers[1] = tier 0 and tier 1
    /// tiers[n] = tier 0..=n
    /// This is a BTreeMap and not a Vec because sometimes a tier is empty
    tiers: BTreeMap<u64, ConnectionsGroup>,
    /// never serve blocks that are too old
    max_block_age: Option<u64>,
    /// tier 0 will be prefered as long as the distance between it and the other tiers is <= max_tier_lag
    max_block_lag: Option<U64>,
}

impl ConsensusFinder {
    pub fn new(max_block_age: Option<u64>, max_block_lag: Option<U64>) -> Self {
        // TODO: what's a good capacity for this? it shouldn't need to be very large
        // TODO: if we change Web3ProxyBlock to store the instance, i think we could use the block_by_hash cache
        let first_seen = Cache::builder()
            .max_capacity(16)
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // TODO: hard coding 0-9 isn't great, but its easier than refactoring this to be smart about config reloading
        let tiers = (0..10)
            .map(|x| (x, ConnectionsGroup::new(first_seen.clone())))
            .collect();

        Self {
            tiers,
            max_block_age,
            max_block_lag,
        }
    }

    pub fn len(&self) -> usize {
        self.tiers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    /// get the ConnectionsGroup that contains all rpcs
    /// panics if there are no tiers
    pub fn all_rpcs_group(&self) -> Option<&ConnectionsGroup> {
        self.tiers.values().last()
    }

    /// get the mutable ConnectionsGroup that contains all rpcs
    pub fn all_mut(&mut self) -> Option<&mut ConnectionsGroup> {
        self.tiers.values_mut().last()
    }

    pub fn remove(&mut self, rpc: &Arc<Web3Rpc>) -> Option<Web3ProxyBlock> {
        let mut removed = None;

        for (i, tier_group) in self.tiers.iter_mut().rev() {
            if i < &rpc.tier {
                break;
            }
            let x = tier_group.remove(rpc);

            if removed.is_none() && x.is_some() {
                removed = x;
            }
        }

        removed
    }

    /// returns the block that the rpc was on before updating to the new_block
    pub async fn insert(
        &mut self,
        rpc: &Arc<Web3Rpc>,
        new_block: Web3ProxyBlock,
    ) -> Option<Web3ProxyBlock> {
        let mut old = None;

        // TODO: error if rpc.tier is not in self.tiers

        for (i, tier_group) in self.tiers.iter_mut().rev() {
            if i < &rpc.tier {
                break;
            }

            // TODO: should new_block be a ref?
            let x = tier_group.insert(rpc.clone(), new_block.clone()).await;

            if old.is_none() && x.is_some() {
                old = x;
            }
        }

        old
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

                if let Some(prev_block) = self.insert(&rpc, rpc_head_block.clone()).await {
                    // false if this block was already sent by this rpc. return early
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

    pub async fn best_consensus_connections(
        &mut self,
        authorization: &Arc<Authorization>,
        web3_connections: &Web3Rpcs,
    ) -> anyhow::Result<ConsensusWeb3Rpcs> {
        // TODO: attach context to these?
        let highest_known_block = self
            .all_rpcs_group()
            .context("no rpcs")?
            .highest_block
            .as_ref()
            .context("no highest block")?;

        trace!("highest_known_block: {}", highest_known_block);

        let min_block_num = self
            .max_block_lag
            .map(|x| highest_known_block.number().saturating_sub(x))
            // we also want to be sure we don't ever go backwards!
            .max(web3_connections.head_block_num());

        trace!("min_block_num: {:#?}", min_block_num);

        // TODO Should this be a Vec<Result<Option<_, _>>>?
        // TODO: how should errors be handled?
        // TODO: find the best tier with a connectionsgroup. best case, this only queries the first tier
        // TODO: do we need to calculate all of them? I think having highest_known_block included as part of min_block_num should make that unnecessary
        for (tier, x) in self.tiers.iter() {
            trace!("checking tier {}: {:#?}", tier, x.rpc_to_block);
            if let Ok(consensus_head_connections) = x
                .consensus_head_connections(authorization, web3_connections, min_block_num, tier)
                .await
            {
                trace!("success on tier {}", tier);
                // we got one! hopefully it didn't need to use any backups.
                // but even if it did need backup servers, that is better than going to a worse tier
                return Ok(consensus_head_connections);
            }
        }

        return Err(anyhow::anyhow!("failed finding consensus on all tiers"));
    }
}

#[cfg(test)]
mod test {
    // #[test]
    // fn test_simplest_case_consensus_head_connections() {
    //     todo!();
    // }
}
