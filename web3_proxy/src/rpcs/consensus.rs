use super::blockchain::Web3ProxyBlock;
use super::many::Web3Rpcs;
use super::one::Web3Rpc;
use crate::frontend::authorization::Authorization;
use crate::frontend::errors::{Web3ProxyErrorContext, Web3ProxyResult};
use derive_more::Constructor;
use ethers::prelude::{H256, U64};
use hashbrown::{HashMap, HashSet};
use itertools::{Itertools, MinMaxResult};
use log::{debug, trace, warn};
use quick_cache_ttl::Cache;
use serde::Serialize;
use std::cmp::{Ordering, Reverse};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use tokio::time::Instant;

#[derive(Clone, Serialize)]
struct RpcData {
    head_block_num: U64,
    // TODO: this is too simple. erigon has 4 prune levels (hrct)
    oldest_block_num: U64,
}

impl RpcData {
    fn new(rpc: &Web3Rpc, head: &Web3ProxyBlock) -> Self {
        let head_block_num = *head.number();

        let block_data_limit = rpc.block_data_limit();

        let oldest_block_num = head_block_num.saturating_sub(block_data_limit);

        Self {
            head_block_num,
            oldest_block_num,
        }
    }

    // TODO: take an enum for the type of data (hrtc)
    fn data_available(&self, block_num: &U64) -> bool {
        *block_num >= self.oldest_block_num && *block_num <= self.head_block_num
    }
}

#[derive(Constructor, Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RpcRanking {
    tier: u64,
    backup: bool,
    head_num: Option<U64>,
}

impl RpcRanking {
    pub fn add_offset(&self, offset: u64) -> Self {
        Self {
            tier: self.tier + offset,
            backup: self.backup,
            head_num: self.head_num,
        }
    }

    pub fn default_with_backup(backup: bool) -> Self {
        Self {
            backup,
            ..Default::default()
        }
    }

    fn sort_key(&self) -> (u64, bool, Reverse<Option<U64>>) {
        // TODO: add soft_limit here? add peak_ewma here?
        (self.tier, !self.backup, Reverse(self.head_num))
    }
}

impl Ord for RpcRanking {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for RpcRanking {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub type RankedRpcMap = BTreeMap<RpcRanking, Vec<Arc<Web3Rpc>>>;

pub enum ShouldWaitForBlock {
    Ready,
    Wait { current: Option<U64> },
    NeverReady,
}

/// A collection of Web3Rpcs that are on the same block.
/// Serialize is so we can print it on our /status endpoint
#[derive(Clone, Serialize)]
pub struct ConsensusWeb3Rpcs {
    pub(crate) tier: u64,
    pub(crate) backups_needed: bool,

    // TODO: this is already inside best_rpcs. Don't skip, instead make a shorter serialize
    #[serde(skip_serializing)]
    pub(crate) head_block: Web3ProxyBlock,

    // TODO: smaller serialize
    pub(crate) head_rpcs: Vec<Arc<Web3Rpc>>,

    // TODO: make this work. the key needs to be a string. I think we need `serialize_with`
    #[serde(skip_serializing)]
    pub(crate) other_rpcs: RankedRpcMap,

    // TODO: make this work. the key needs to be a string. I think we need `serialize_with`
    #[serde(skip_serializing)]
    rpc_data: HashMap<Arc<Web3Rpc>, RpcData>,
}

impl ConsensusWeb3Rpcs {
    #[inline]
    pub fn num_consensus_rpcs(&self) -> usize {
        self.head_rpcs.len()
    }

    /// will tell you if you should wait for a block
    /// TODO: also include method (or maybe an enum representing the different prune types)
    pub fn should_wait_for_block(
        &self,
        needed_block_num: Option<&U64>,
        skip_rpcs: &[Arc<Web3Rpc>],
    ) -> ShouldWaitForBlock {
        // TODO: i think checking synced is always a waste of time. though i guess there could be a race
        if self
            .head_rpcs
            .iter()
            .any(|rpc| self.rpc_will_work_eventually(rpc, needed_block_num, skip_rpcs))
        {
            let head_num = self.head_block.number();

            if Some(head_num) >= needed_block_num {
                trace!("best (head) block: {}", head_num);
                return ShouldWaitForBlock::Ready;
            }
        }

        // all of the head rpcs are skipped

        let mut best_num = None;

        // iterate the other rpc tiers to find the next best block
        for (next_ranking, next_rpcs) in self.other_rpcs.iter() {
            if !next_rpcs
                .iter()
                .any(|rpc| self.rpc_will_work_eventually(rpc, needed_block_num, skip_rpcs))
            {
                trace!("everything in this ranking ({:?}) is skipped", next_ranking);
                continue;
            }

            let next_head_num = next_ranking.head_num.as_ref();

            if next_head_num >= needed_block_num {
                trace!("best (head) block: {:?}", next_head_num);
                return ShouldWaitForBlock::Ready;
            }

            best_num = next_head_num;
        }

        // TODO: this seems wrong
        if best_num.is_some() {
            trace!("best (old) block: {:?}", best_num);
            ShouldWaitForBlock::Wait {
                current: best_num.copied(),
            }
        } else {
            trace!("never ready");
            ShouldWaitForBlock::NeverReady
        }
    }

    pub fn has_block_data(&self, rpc: &Web3Rpc, block_num: &U64) -> bool {
        self.rpc_data
            .get(rpc)
            .map(|x| x.data_available(block_num))
            .unwrap_or(false)
    }

    // TODO: take method as a param, too. mark nodes with supported methods (maybe do it optimistically? on)
    pub fn rpc_will_work_eventually(
        &self,
        rpc: &Arc<Web3Rpc>,
        needed_block_num: Option<&U64>,
        skip_rpcs: &[Arc<Web3Rpc>],
    ) -> bool {
        if skip_rpcs.contains(rpc) {
            // if rpc is skipped, it must have already been determined it is unable to serve the request
            return false;
        }

        if let Some(needed_block_num) = needed_block_num {
            if let Some(rpc_data) = self.rpc_data.get(rpc) {
                match rpc_data.head_block_num.cmp(needed_block_num) {
                    Ordering::Less => {
                        trace!("{} is behind. let it catch up", rpc);
                        // TODO: what if this is a pruned rpc that is behind by a lot, and the block is old, too?
                        return true;
                    }
                    Ordering::Greater | Ordering::Equal => {
                        // rpc is synced past the needed block. make sure the block isn't too old for it
                        if self.has_block_data(rpc, needed_block_num) {
                            trace!("{} has {}", rpc, needed_block_num);
                            return true;
                        } else {
                            trace!("{} does not have {}", rpc, needed_block_num);
                            return false;
                        }
                    }
                }
            }

            // no rpc data for this rpc. thats not promising
            false
        } else {
            // if no needed_block_num was specified, then this should work
            true
        }
    }

    // TODO: better name for this
    // TODO: this should probably be on the rpcs as "can_serve_request"
    // TODO: this should probably take the method, too
    pub fn rpc_will_work_now(
        &self,
        skip: &[Arc<Web3Rpc>],
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        rpc: &Arc<Web3Rpc>,
    ) -> bool {
        if skip.contains(rpc) {
            trace!("skipping {}", rpc);
            return false;
        }

        if let Some(min_block_needed) = min_block_needed {
            if !self.has_block_data(rpc, min_block_needed) {
                trace!(
                    "{} is missing min_block_needed ({}). skipping",
                    rpc,
                    min_block_needed,
                );
                return false;
            }
        }

        if let Some(max_block_needed) = max_block_needed {
            if !self.has_block_data(rpc, max_block_needed) {
                trace!(
                    "{} is missing max_block_needed ({}). skipping",
                    rpc,
                    max_block_needed,
                );
                return false;
            }
        }

        // TODO: this might be a big perf hit. benchmark
        if let Some(x) = rpc.hard_limit_until.as_ref() {
            if *x.borrow() > Instant::now() {
                trace!("{} is rate limited. skipping", rpc,);
                return false;
            }
        }

        true
    }

    // TODO: sum_hard_limit?
}

impl fmt::Debug for ConsensusWeb3Rpcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        // TODO: print the actual conns?
        f.debug_struct("ConsensusWeb3Rpcs")
            .field("head_block", &self.head_block)
            .field("num_conns", &self.head_rpcs.len())
            .finish_non_exhaustive()
    }
}

// TODO: refs for all of these. borrow on a Sender is cheap enough
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
            !consensus.head_rpcs.is_empty()
        } else {
            false
        }
    }

    pub fn num_synced_rpcs(&self) -> usize {
        let consensus = self.watch_consensus_rpcs_sender.borrow();

        if let Some(consensus) = consensus.as_ref() {
            consensus.head_rpcs.len()
        } else {
            0
        }
    }
}

type FirstSeenCache = Cache<H256, Instant>;

/// A ConsensusConnections builder that tracks all connection heads across multiple groups of servers
pub struct ConsensusFinder {
    /// backups for all tiers are only used if necessary
    /// `tiers[0] = only tier 0`
    /// `tiers[1] = tier 0 and tier 1`
    /// `tiers[n] = tier 0..=n`
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
        let first_seen = Cache::new(16);

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
            .get_or_insert_async::<Infallible>(block.hash(), async { Ok(Instant::now()) })
            .await
            .expect("this cache get is infallible");

        // calculate elapsed time before trying to lock
        let latency = first_seen.elapsed();

        // record the time behind the fastest node
        rpc.head_latency.write().record(latency);

        // update the local mapping of rpc -> block
        self.rpc_heads.insert(rpc, block)
    }

    /// Update our tracking of the rpc and return true if something changed
    pub(crate) async fn update_rpc(
        &mut self,
        rpc_head_block: Option<Web3ProxyBlock>,
        rpc: Arc<Web3Rpc>,
        // we need this so we can save the block to caches. i don't like it though. maybe we should use a lazy_static Cache wrapper that has a "save_block" method?. i generally dislike globals but i also dislike all the types having to pass eachother around
        web3_connections: &Web3Rpcs,
    ) -> Web3ProxyResult<bool> {
        // add the rpc's block to connection_heads, or remove the rpc from connection_heads
        let changed = match rpc_head_block {
            Some(mut rpc_head_block) => {
                // we don't know if its on the heaviest chain yet
                rpc_head_block = web3_connections
                    .try_cache_block(rpc_head_block, false)
                    .await
                    .web3_context("failed caching block")?;

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
    ) -> Web3ProxyResult<Option<ConsensusWeb3Rpcs>> {
        let minmax_block = self.rpc_heads.values().minmax_by_key(|&x| x.number());

        let (lowest_block, highest_block) = match minmax_block {
            MinMaxResult::NoElements => return Ok(None),
            MinMaxResult::OneElement(x) => (x, x),
            MinMaxResult::MinMax(min, max) => (min, max),
        };

        let highest_block_number = highest_block.number();

        trace!("highest_block_number: {}", highest_block_number);

        trace!("lowest_block_number: {}", lowest_block.number());

        let max_lag_block_number =
            highest_block_number.saturating_sub(self.max_block_lag.unwrap_or_else(|| U64::from(5)));

        trace!("max_lag_block_number: {}", max_lag_block_number);

        let lowest_block_number = lowest_block.number().max(&max_lag_block_number);

        trace!("safe lowest_block_number: {}", lowest_block_number);

        let num_known = self.rpc_heads.len();

        if num_known < web3_rpcs.min_head_rpcs {
            // this keeps us from serving requests when the proxy first starts
            trace!("not enough servers known");
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

        // trace!("first_tier: {}", current_tier);

        // trace!("rpc_heads_by_tier: {:#?}", rpc_heads_by_tier);

        // loop over all the rpc heads (grouped by tier) and their parents to find consensus
        // TODO: i'm sure theres a lot of shortcuts that could be taken, but this is simplest to implement
        for (rpc, rpc_head) in rpc_heads_by_tier.into_iter() {
            if current_tier != rpc.tier {
                // we finished processing a tier. check for primary results
                if let Some(consensus) = self.count_votes(&primary_votes, web3_rpcs) {
                    trace!("found enough votes on tier {}", current_tier);
                    return Ok(Some(consensus));
                }

                // only set backup consensus once. we don't want it to keep checking on worse tiers if it already found consensus
                if backup_consensus.is_none() {
                    if let Some(consensus) = self.count_votes(&backup_votes, web3_rpcs) {
                        trace!("found backup votes on tier {}", current_tier);
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
                        warn!(
                            "Problem fetching parent block of {:?} during consensus finding: {:#?}",
                            block_to_check.hash(),
                            err
                        );
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
            .into_iter()
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

            let mut other_rpcs = BTreeMap::new();

            for (x, x_head) in self
                .rpc_heads
                .iter()
                .filter(|(k, _)| !consensus_rpcs.contains(k))
            {
                let x_head_num = *x_head.number();

                let key: RpcRanking = RpcRanking::new(x.tier, x.backup, Some(x_head_num));

                other_rpcs
                    .entry(key)
                    .or_insert_with(Vec::new)
                    .push(x.clone());
            }

            // TODO: how should we populate this?
            let mut rpc_data = HashMap::with_capacity(self.rpc_heads.len());

            for (x, x_head) in self.rpc_heads.iter() {
                let y = RpcData::new(x, x_head);

                rpc_data.insert(x.clone(), y);
            }

            let consensus = ConsensusWeb3Rpcs {
                tier,
                head_block: maybe_head_block.clone(),
                head_rpcs: consensus_rpcs,
                other_rpcs,
                backups_needed,
                rpc_data,
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
