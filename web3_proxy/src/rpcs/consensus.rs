use super::blockchain::Web3ProxyBlock;
use super::many::Web3Rpcs;
use super::one::Web3Rpc;
use super::request::OpenRequestHandle;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::frontend::authorization::Web3Request;
use crate::rpcs::request::OpenRequestResult;
use async_stream::stream;
use base64::engine::general_purpose;
use derive_more::Constructor;
use ethers::prelude::{H256, U64};
use futures::stream::FuturesUnordered;
use futures::{Stream, StreamExt};
use hashbrown::{HashMap, HashSet};
use hdrhistogram::serialization::{Serializer, V2DeflateSerializer};
use hdrhistogram::Histogram;
use itertools::{Itertools, MinMaxResult};
use moka::future::Cache;
use serde::Serialize;
use std::cmp::{min_by_key, Ordering, Reverse};
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::select;
use tokio::task::yield_now;
use tokio::time::{sleep_until, Instant};
use tracing::{debug, enabled, error, info, trace, warn, Level};

#[derive(Clone, Debug, Serialize)]
struct ConsensusRpcData {
    head_block_num: U64,
    // TODO: this is too simple. erigon has 4 prune levels (hrct)
    oldest_block_num: U64,
}

impl ConsensusRpcData {
    fn new(rpc: &Web3Rpc, head: &Web3ProxyBlock) -> Self {
        let head_block_num = head.number();

        let block_data_limit = rpc.block_data_limit();

        let oldest_block_num = head_block_num.saturating_sub(block_data_limit);

        Self {
            head_block_num,
            oldest_block_num,
        }
    }

    // TODO: take an enum for the type of data (hrtc)
    fn data_available(&self, block_num: U64) -> bool {
        block_num >= self.oldest_block_num && block_num <= self.head_block_num
    }
}

#[derive(Constructor, Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RpcRanking {
    backup: bool,
    /// note: the servers in this tier might have blocks higher than this
    consensus_head_num: Option<U64>,
    tier: u32,
}

impl RpcRanking {
    pub fn default_with_backup(backup: bool) -> Self {
        Self {
            backup,
            ..Default::default()
        }
    }

    fn sort_key(&self) -> (bool, Reverse<Option<U64>>, u32) {
        // TODO: add sum_soft_limit here? add peak_ewma here?
        // TODO: should backup or tier be checked first? now that tiers are automated, backups should be more reliable, but still leave them last
        // while tier might give us better latency, giving requests to a server that is behind by a block will get in the way of it syncing. better to only query synced servers
        // TODO: should we include a random number in here?
        (!self.backup, Reverse(self.consensus_head_num), self.tier)
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

/// TODO: i think we can get rid of this in favor of
pub enum ShouldWaitForBlock {
    Ready,
    // BackupReady,
    /// how many blocks you will have to wait
    Wait(U64),
    // WaitForBackup { current: Option<U64> },
    NeverReady,
}

#[derive(Clone, Debug, Serialize)]
enum SortMethod {
    Shuffle,
    Sort,
}

/// A collection of Web3Rpcs that are on the same block.
/// Serialize is so we can print it on our /status endpoint
/// TODO: remove head_block/head_rpcs/tier and replace with one RankedRpcMap
/// TODO: add `best_rpc(method_data_kind, min_block_needed, max_block_needed, include_backups)`
/// TODO: make serializing work. the key needs to be a string. I think we need `serialize_with`
#[derive(Clone, Debug, Serialize)]
pub struct RankedRpcs {
    pub head_block: Web3ProxyBlock,
    pub num_synced: usize,
    pub backups_needed: bool,

    inner: Vec<Arc<Web3Rpc>>,

    // TODO: make serializing work. the key needs to be a string. I think we need `serialize_with`
    #[serde(skip_serializing)]
    rpc_data: HashMap<Arc<Web3Rpc>, ConsensusRpcData>,

    sort_mode: SortMethod,
}

// TODO: could these be refs? The owning RankedRpcs lifetime might work. `stream!` might make it complicated
pub struct RpcsForRequest {
    inner: Vec<Arc<Web3Rpc>>,
    outer: Vec<Arc<Web3Rpc>>,
    request: Arc<Web3Request>,
}

impl RankedRpcs {
    pub fn from_rpcs(rpcs: Vec<Arc<Web3Rpc>>, head_block: Option<Web3ProxyBlock>) -> Option<Self> {
        // we don't need to sort the rpcs now. we will sort them when a request neds them
        // TODO: the shame about this is that we lose just being able to compare 2 random servers

        let head_block = head_block?;

        let backups_needed = rpcs.iter().any(|x| x.backup);

        let num_synced = rpcs.len();

        let rpc_data = Default::default();
        // TODO: do we need real data in rpc_data? if we are calling from_rpcs, we probably don't even track their block
        // let mut rpc_data = HashMap::<Arc<Web3Rpc>, ConsensusRpcData>::with_capacity(num_synced);
        // for rpc in rpcs.iter().cloned() {
        //     let data = ConsensusRpcData::new(&rpc, &head_block);
        //     rpc_data.insert(rpc, data);
        // }

        let sort_mode = SortMethod::Shuffle;

        let ranked_rpcs = RankedRpcs {
            backups_needed,
            head_block,
            inner: rpcs,
            num_synced,
            rpc_data,
            sort_mode,
        };

        Some(ranked_rpcs)
    }

    pub fn from_votes(
        min_synced_rpcs: usize,
        min_sum_soft_limit: u32,
        max_lag_block: U64,
        votes: HashMap<Web3ProxyBlock, (HashSet<&Arc<Web3Rpc>>, u32)>,
        heads: HashMap<Arc<Web3Rpc>, Web3ProxyBlock>,
    ) -> Option<Self> {
        // find the blocks that meets our min_sum_soft_limit and min_synced_rpcs
        let mut votes: Vec<_> = votes
            .into_iter()
            .filter_map(|(block, (rpcs, sum_soft_limit))| {
                if block.number() < max_lag_block
                    || sum_soft_limit < min_sum_soft_limit
                    || rpcs.len() < min_synced_rpcs
                {
                    None
                } else {
                    Some((block, sum_soft_limit, rpcs))
                }
            })
            .collect();

        // sort the votes
        votes.sort_by_key(|(block, sum_soft_limit, _)| {
            (
                Reverse(block.number()),
                // TODO: block total difficulty (if we have it)
                Reverse(*sum_soft_limit),
                // TODO: median/peak latency here?
            )
        });

        // return the first result that exceededs confgured minimums (if any)
        if let Some((best_block, _, best_rpcs)) = votes.into_iter().next() {
            let mut ranked_rpcs: Vec<_> = best_rpcs.into_iter().map(Arc::clone).collect();
            let mut rpc_data = HashMap::new();

            let backups_needed = ranked_rpcs.iter().any(|x| x.backup);
            let num_synced = ranked_rpcs.len();

            // TODO: add all the unsynced rpcs
            for (x, x_head) in heads.iter() {
                let data = ConsensusRpcData::new(x, x_head);

                rpc_data.insert(x.clone(), data);

                if ranked_rpcs.contains(x) {
                    continue;
                }

                if x_head.number() < max_lag_block {
                    // server is too far behind
                    continue;
                }

                ranked_rpcs.push(x.clone());
            }

            // consensus found!
            trace!(?ranked_rpcs);

            let sort_mode = SortMethod::Sort;

            let consensus = RankedRpcs {
                backups_needed,
                head_block: best_block,
                rpc_data,
                sort_mode,
                inner: ranked_rpcs,
                num_synced,
            };

            return Some(consensus);
        }

        None
    }

    pub fn for_request(&self, web3_request: &Arc<Web3Request>) -> Option<RpcsForRequest> {
        if self.num_active_rpcs() == 0 {
            return None;
        }

        let head_block = self.head_block.number();

        // these are bigger than we need, but how much does that matter?
        let mut inner = Vec::with_capacity(self.num_active_rpcs());
        let mut outer = Vec::with_capacity(self.num_active_rpcs());

        let max_block_needed = web3_request.max_block_needed();

        match self.sort_mode {
            SortMethod::Shuffle => {
                // if we are shuffling, it is because we don't watch the head_blocks of the rpcs
                // clone all of the rpcs
                self.inner.clone_into(&mut inner);

                let mut rng = nanorand::tls_rng();

                // we use shuffle instead of sort. we will compare weights during `RpcsForRequest::to_stream`
                // TODO: use web3_request.start_instant? I think we want it to be as recent as possible
                let now = Instant::now();

                inner.sort_by_cached_key(|x| {
                    x.shuffle_for_load_balancing_on(max_block_needed, &mut rng, now)
                });
            }
            SortMethod::Sort => {
                // TODO: what if min is set to some future block?
                let min_block_needed = web3_request.min_block_needed();

                // TODO: max lag from config
                let recent_block_needed = head_block.saturating_sub(U64::from(5));

                for rpc in &self.inner {
                    if self.has_block_data(rpc, recent_block_needed) {
                        match self.rpc_will_work_eventually(rpc, min_block_needed, max_block_needed)
                        {
                            ShouldWaitForBlock::NeverReady => continue,
                            ShouldWaitForBlock::Ready => inner.push(rpc.clone()),
                            ShouldWaitForBlock::Wait { .. } => outer.push(rpc.clone()),
                        }
                    }
                }

                let now = Instant::now();

                // we use shuffle instead of sort. we will compare weights during `RpcsForRequest::to_stream`
                inner.sort_by_cached_key(|x| x.sort_for_load_balancing_on(max_block_needed, now));
                outer.sort_by_cached_key(|x| x.sort_for_load_balancing_on(max_block_needed, now));
            }
        }

        if inner.is_empty() && outer.is_empty() {
            warn!(?inner, ?outer, %web3_request, %head_block, "no rpcs for request");
            None
        } else {
            trace!(?inner, ?outer, %web3_request, "for_request");
            Some(RpcsForRequest {
                inner,
                outer,
                request: web3_request.clone(),
            })
        }
    }

    pub fn all(&self) -> &[Arc<Web3Rpc>] {
        &self.inner
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// TODO! we should also keep the number on the head block saved
    #[inline]
    pub fn num_active_rpcs(&self) -> usize {
        self.inner.len()
    }

    pub fn has_block_data(&self, rpc: &Web3Rpc, block_num: U64) -> bool {
        self.rpc_data
            .get(rpc)
            .map(|x| x.data_available(block_num))
            .unwrap_or(false)
    }

    // TODO: take method as a param, too. mark nodes with supported methods (maybe do it optimistically? on error mark them as not supporting it)
    pub fn rpc_will_work_eventually(
        &self,
        rpc: &Arc<Web3Rpc>,
        min_block_num: Option<U64>,
        max_block_num: Option<U64>,
    ) -> ShouldWaitForBlock {
        if !rpc.healthy.load(atomic::Ordering::Relaxed) {
            return ShouldWaitForBlock::NeverReady;
        }

        if let Some(min_block_num) = min_block_num {
            if !self.has_block_data(rpc, min_block_num) {
                trace!(
                    "{} is missing min_block_num ({}). will not work eventually",
                    rpc,
                    min_block_num,
                );
                return ShouldWaitForBlock::NeverReady;
            }
        }

        if let Some(needed_block_num) = max_block_num {
            if let Some(rpc_data) = self.rpc_data.get(rpc) {
                match rpc_data.head_block_num.cmp(&needed_block_num) {
                    Ordering::Less => {
                        trace!("{} is behind. let it catch up", rpc);
                        // TODO: what if this is a pruned rpc that is behind by a lot, and the block is old, too?
                        return ShouldWaitForBlock::Wait(
                            needed_block_num - rpc_data.head_block_num,
                        );
                    }
                    Ordering::Greater | Ordering::Equal => {
                        // rpc is synced past the needed block. make sure the block isn't too old for it
                        if self.has_block_data(rpc, needed_block_num) {
                            trace!("{} has {}", rpc, needed_block_num);
                            return ShouldWaitForBlock::Ready;
                        } else {
                            trace!("{} does not have {}", rpc, needed_block_num);
                            return ShouldWaitForBlock::NeverReady;
                        }
                    }
                }
            }
            warn!("no rpc data for this {}. thats not promising", rpc);
            ShouldWaitForBlock::NeverReady
        } else {
            // if no needed_block_num was specified, then this should work
            ShouldWaitForBlock::Ready
        }
    }

    // TODO: this should probably be on the rpcs as "can_serve_request"
    // TODO: and it should take the method into account, too
    pub fn rpc_will_work_now(
        &self,
        min_block_needed: Option<U64>,
        max_block_needed: Option<U64>,
        rpc: &Arc<Web3Rpc>,
    ) -> bool {
        if rpc.backup && !self.backups_needed {
            // skip backups unless backups are needed for ranked_rpcs to exist
            return false;
        }

        if !rpc.healthy.load(atomic::Ordering::Relaxed) {
            return false;
        }

        if let Some(min_block_needed) = min_block_needed {
            if !self.has_block_data(rpc, min_block_needed) {
                trace!(
                    "{} is missing min_block_needed ({}). will not work now",
                    rpc,
                    min_block_needed,
                );
                return false;
            }
        }

        if let Some(max_block_needed) = max_block_needed {
            if !self.has_block_data(rpc, max_block_needed) {
                trace!(
                    "{} is missing max_block_needed ({}). will not work now",
                    rpc,
                    max_block_needed,
                );
                return false;
            }
        }

        // rate limit are handled by sort order

        true
    }

    // TODO: sum_hard_limit?
}

// TODO: refs for all of these. borrow on a Sender is cheap enough
// TODO: move this to many.rs
impl Web3Rpcs {
    pub fn head_block(&self) -> Option<Web3ProxyBlock> {
        self.watch_head_block
            .as_ref()
            .and_then(|x| x.borrow().clone())
    }

    /// note: you probably want to use `head_block` instead
    /// TODO: return a ref?
    pub fn head_block_hash(&self) -> Option<H256> {
        self.head_block().map(|x| *x.hash())
    }

    /// note: you probably want to use `head_block` instead
    /// TODO: return a ref?
    pub fn head_block_num(&self) -> Option<U64> {
        self.head_block().map(|x| x.number())
    }

    pub fn synced(&self) -> bool {
        let consensus = self.watch_ranked_rpcs.borrow();

        if let Some(consensus) = consensus.as_ref() {
            !consensus.is_empty()
        } else {
            false
        }
    }

    pub fn num_synced_rpcs(&self) -> usize {
        let consensus = self.watch_ranked_rpcs.borrow();

        if let Some(consensus) = consensus.as_ref() {
            consensus.num_synced
        } else {
            0
        }
    }
}

type FirstSeenCache = Cache<H256, Instant>;

/// A ConsensusConnections builder that tracks all connection heads across multiple groups of servers
pub struct ConsensusFinder {
    rpc_heads: HashMap<Arc<Web3Rpc>, Web3ProxyBlock>,
    /// no consensus if the best known block is too old
    max_head_block_age: Option<Duration>,
    /// tier 0 will be prefered as long as the distance between it and the other tiers is <= max_tier_lag
    max_head_block_lag: Option<U64>,
    /// Block Hash -> First Seen Instant. used to track rpc.head_delay. The same cache should be shared between all ConnectionsGroups
    first_seen: FirstSeenCache,
}

impl ConsensusFinder {
    pub fn new(max_head_block_age: Option<Duration>, max_head_block_lag: Option<U64>) -> Self {
        // TODO: what's a good capacity for this? it shouldn't need to be very large
        let first_seen = Cache::new(16);

        let rpc_heads = HashMap::new();

        Self {
            rpc_heads,
            max_head_block_age,
            max_head_block_lag,
            first_seen,
        }
    }

    pub fn len(&self) -> usize {
        self.rpc_heads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rpc_heads.is_empty()
    }

    /// `connection_heads` is a mapping of rpc_names to head block hashes.
    /// self.blockchain_map is a mapping of hashes to the complete ArcBlock.
    /// TODO: return something?
    /// TODO: move this onto ConsensusFinder
    pub(super) async fn refresh(
        &mut self,
        web3_rpcs: &Web3Rpcs,
        rpc: Option<&Arc<Web3Rpc>>,
        new_block: Option<Web3ProxyBlock>,
    ) -> Web3ProxyResult<bool> {
        let new_ranked_rpcs = match self
            .find_consensus_connections(web3_rpcs)
            .await
            .web3_context("error while finding consensus head block!")?
        {
            None => return Ok(false),
            Some(x) => x,
        };

        trace!(?new_ranked_rpcs);

        let watch_consensus_head_sender = web3_rpcs.watch_head_block.as_ref().unwrap();
        // TODO: think more about the default for tiers
        let best_tier = self.best_tier().unwrap_or_default();
        let worst_tier = self.worst_tier().unwrap_or_default();
        let backups_needed = new_ranked_rpcs.backups_needed;
        let consensus_head_block = new_ranked_rpcs.head_block.clone();
        let num_consensus_rpcs = new_ranked_rpcs.num_active_rpcs();
        let num_active_rpcs = self.len();
        let total_rpcs = web3_rpcs.len();

        let new_ranked_rpcs = Arc::new(new_ranked_rpcs);

        let old_ranked_rpcs = web3_rpcs
            .watch_ranked_rpcs
            .send_replace(Some(new_ranked_rpcs.clone()));

        let backups_voted_str = if backups_needed { "B " } else { "" };

        let rpc_head_str = if let Some(rpc) = rpc.as_ref() {
            format!(
                "{}@{}",
                rpc,
                new_block
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "None".to_string()),
            )
        } else {
            "None".to_string()
        };

        match old_ranked_rpcs.as_ref() {
            None => {
                info!(
                    "first {}/{} {}{}/{}/{} block={}, rpc={}",
                    best_tier,
                    worst_tier,
                    backups_voted_str,
                    num_consensus_rpcs,
                    num_active_rpcs,
                    total_rpcs,
                    consensus_head_block,
                    rpc_head_str,
                );

                if backups_needed {
                    // TODO: what else should be in this error?
                    warn!("Backup RPCs are in use!");
                }

                // this should already be cached
                let consensus_head_block = web3_rpcs
                    .try_cache_block(consensus_head_block, true)
                    .await?;

                watch_consensus_head_sender
                    .send(Some(consensus_head_block))
                    .or(Err(Web3ProxyError::WatchSendError))
                    .web3_context(
                        "watch_consensus_head_sender failed sending first consensus_head_block",
                    )?;
            }
            Some(old_consensus_connections) => {
                let old_head_block = &old_consensus_connections.head_block;

                match consensus_head_block.number().cmp(&old_head_block.number()) {
                    Ordering::Equal => {
                        // multiple blocks with the same fork!
                        if consensus_head_block.hash() == old_head_block.hash() {
                            // no change in hash. no need to use watch_consensus_head_sender
                            // TODO: trace level if rpc is backup
                            debug!(
                                "con {}/{} {}{}/{}/{} con={} rpc={}",
                                best_tier,
                                worst_tier,
                                backups_voted_str,
                                num_consensus_rpcs,
                                num_active_rpcs,
                                total_rpcs,
                                consensus_head_block,
                                rpc_head_str,
                            )
                        } else {
                            // hash changed

                            debug!(
                                "unc {}/{} {}{}/{}/{} con={} old={} rpc={}",
                                best_tier,
                                worst_tier,
                                backups_voted_str,
                                num_consensus_rpcs,
                                num_active_rpcs,
                                total_rpcs,
                                consensus_head_block,
                                old_head_block,
                                rpc_head_str,
                            );

                            let consensus_head_block = web3_rpcs
                                .try_cache_block(consensus_head_block, true)
                                .await
                                .web3_context("save consensus_head_block as heaviest chain")?;

                            watch_consensus_head_sender
                                .send(Some(consensus_head_block))
                                .or(Err(Web3ProxyError::WatchSendError))
                                .web3_context("watch_consensus_head_sender failed sending uncled consensus_head_block")?;
                        }
                    }
                    Ordering::Less => {
                        // this is unlikely but possible
                        // TODO: better log that includes all the votes
                        warn!(
                            "chain rolled back {}/{} {}{}/{}/{} con={} old={} rpc={}",
                            best_tier,
                            worst_tier,
                            backups_voted_str,
                            num_consensus_rpcs,
                            num_active_rpcs,
                            total_rpcs,
                            consensus_head_block,
                            old_head_block,
                            rpc_head_str,
                        );

                        if backups_needed {
                            // TODO: what else should be in this error?
                            warn!("Backup RPCs are in use!");
                        }

                        // TODO: tell save_block to remove any higher block numbers from the cache. not needed because we have other checks on requested blocks being > head, but still seems like a good idea
                        let consensus_head_block = web3_rpcs
                            .try_cache_block(consensus_head_block, true)
                            .await
                            .web3_context(
                                "save_block sending consensus_head_block as heaviest chain",
                            )?;

                        watch_consensus_head_sender
                            .send(Some(consensus_head_block))
                            .or(Err(Web3ProxyError::WatchSendError))
                            .web3_context("watch_consensus_head_sender failed sending rollback consensus_head_block")?;
                    }
                    Ordering::Greater => {
                        info!(
                            "new {}/{} {}{}/{}/{} con={} rpc={}",
                            best_tier,
                            worst_tier,
                            backups_voted_str,
                            num_consensus_rpcs,
                            num_active_rpcs,
                            total_rpcs,
                            consensus_head_block,
                            rpc_head_str,
                        );

                        if backups_needed {
                            // TODO: what else should be in this error?
                            warn!("Backup RPCs are in use!");
                        }

                        let consensus_head_block = web3_rpcs
                            .try_cache_block(consensus_head_block, true)
                            .await?;

                        watch_consensus_head_sender.send(Some(consensus_head_block))
                            .or(Err(Web3ProxyError::WatchSendError))
                            .web3_context("watch_consensus_head_sender failed sending new consensus_head_block")?;
                    }
                }
            }
        }

        Ok(true)
    }

    pub(super) async fn process_block_from_rpc(
        &mut self,
        web3_rpcs: &Web3Rpcs,
        new_block: Option<Web3ProxyBlock>,
        rpc: Arc<Web3Rpc>,
    ) -> Web3ProxyResult<bool> {
        // TODO: how should we handle an error here?
        if !self
            .update_rpc(new_block.clone(), rpc.clone(), web3_rpcs)
            .await
            .web3_context("failed to update rpc")?
        {
            // nothing changed. no need to scan for a new consensus head
            // TODO: this should this be true if there is an existing consensus?
            return Ok(false);
        }

        self.refresh(web3_rpcs, Some(&rpc), new_block).await
    }

    fn remove(&mut self, rpc: &Arc<Web3Rpc>) -> Option<Web3ProxyBlock> {
        self.rpc_heads.remove(rpc)
    }

    async fn insert(&mut self, rpc: Arc<Web3Rpc>, block: Web3ProxyBlock) -> Option<Web3ProxyBlock> {
        let first_seen = self
            .first_seen
            .get_with_by_ref(block.hash(), async { Instant::now() })
            .await;

        // calculate elapsed time before trying to lock
        let latency = first_seen.elapsed();

        // record the time behind the fastest node
        rpc.head_delay
            .write()
            .await
            .record_secs(latency.as_secs_f32());

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

                if let Some(max_age) = self.max_head_block_age {
                    if rpc_head_block.age() > max_age {
                        warn!("rpc_head_block from {} is too old! {}", rpc, rpc_head_block);
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

    pub async fn update_tiers(&mut self) -> Web3ProxyResult<()> {
        match self.rpc_heads.len() {
            0 => {}
            1 => {
                for rpc in self.rpc_heads.keys() {
                    rpc.tier.store(1, atomic::Ordering::Relaxed)
                }
            }
            _ => {
                // iterate first to find bounds
                // min_latency_sec is actual min_median_latency_sec
                let mut min_median_latency_sec = f32::MAX;
                let mut max_median_latency_sec = f32::MIN;
                let mut median_latencies_sec = HashMap::new();
                for rpc in self.rpc_heads.keys() {
                    let median_latency_sec = rpc
                        .median_latency
                        .as_ref()
                        .map(|x| x.seconds())
                        .unwrap_or_default();

                    min_median_latency_sec = min_median_latency_sec.min(median_latency_sec);
                    max_median_latency_sec = min_median_latency_sec.max(median_latency_sec);

                    median_latencies_sec.insert(rpc, median_latency_sec);
                }

                // dev logging of a histogram
                if enabled!(Level::TRACE) {
                    // convert to ms because the histogram needs ints
                    let max_median_latency_ms = (max_median_latency_sec * 1000.0).ceil() as u64;

                    // create the histogram
                    // histogram requires high to be at least 2 x low
                    // using min_latency for low does not work how we want it though
                    // so just set the default range = 1ms..1s
                    let hist_low = 1;
                    let hist_high = max_median_latency_ms.max(1_000);
                    let mut hist_ms =
                        Histogram::<u32>::new_with_bounds(hist_low, hist_high, 3).unwrap();

                    // TODO: resize shouldn't be necessary, but i've seen it error
                    hist_ms.auto(true);

                    for median_sec in median_latencies_sec.values() {
                        let median_ms = (median_sec * 1000.0).round() as u64;

                        hist_ms.record(median_ms)?;
                    }

                    // print the histogram. see docs/histograms.txt for more info
                    let mut encoder =
                        base64::write::EncoderWriter::new(Vec::new(), &general_purpose::STANDARD);

                    V2DeflateSerializer::new()
                        .serialize(&hist_ms, &mut encoder)
                        .unwrap();

                    let encoded = encoder.finish().unwrap();

                    let encoded = String::from_utf8(encoded).unwrap();

                    trace!("weighted_latencies: {}", encoded);
                }

                trace!("median_latencies_sec: {:#?}", median_latencies_sec);

                trace!("min_median_latency_sec: {}", min_median_latency_sec);

                // TODO: get someone who is better at math to do something smarter. maybe involving stddev? maybe involving cutting the histogram at the troughs?
                // bucket sizes of the larger of 20ms or 1/2 the lowest latency
                // TODO: is 20ms an okay default? make it configurable?
                // TODO: does keeping the buckets the same size make sense?
                let tier_sec_size = 0.020f32.max(min_median_latency_sec / 2.0);

                trace!("tier_sec_size: {}", tier_sec_size);

                for (rpc, median_latency_sec) in median_latencies_sec.into_iter() {
                    let tier = (median_latency_sec - min_median_latency_sec) / tier_sec_size;

                    // start tiers at 1
                    let tier = (tier.floor() as u32).saturating_add(1);

                    trace!("{} - p50_sec: {}, tier {}", rpc, median_latency_sec, tier);

                    rpc.tier.store(tier, atomic::Ordering::Relaxed);
                }
            }
        }

        Ok(())
    }

    pub async fn find_consensus_connections(
        &mut self,
        web3_rpcs: &Web3Rpcs,
    ) -> Web3ProxyResult<Option<RankedRpcs>> {
        self.update_tiers().await?;

        let minmax_block = self.rpc_heads.values().minmax_by_key(|&x| x.number());

        let (lowest_block, highest_block) = match minmax_block {
            MinMaxResult::NoElements => return Ok(None),
            MinMaxResult::OneElement(x) => (x, x),
            MinMaxResult::MinMax(min, max) => (min, max),
        };

        let highest_block_number = highest_block.number();

        trace!("highest_block_number: {}", highest_block_number);

        trace!("lowest_block_number: {}", lowest_block.number());

        // TODO: move this default. should be in config, not here
        let max_lag_block_number = highest_block_number
            .saturating_sub(self.max_head_block_lag.unwrap_or_else(|| U64::from(5)));

        trace!("max_lag_block_number: {}", max_lag_block_number);

        let lowest_block_number = lowest_block.number().max(max_lag_block_number);

        // TODO: should lowest block number be set such that the rpc won't ever go backwards?
        trace!("safe lowest_block_number: {}", lowest_block_number);

        let num_known = self.rpc_heads.len();

        // TODO: also track the sum of *available* hard_limits? if any servers have no hard limits, use their soft limit or no limit?
        // TODO: struct for the value of the votes hashmap?
        let mut primary_votes: HashMap<Web3ProxyBlock, (HashSet<&Arc<Web3Rpc>>, u32)> =
            HashMap::with_capacity(num_known);
        let mut backup_votes: HashMap<Web3ProxyBlock, (HashSet<&Arc<Web3Rpc>>, u32)> =
            HashMap::with_capacity(num_known);

        for (rpc, rpc_head) in self.rpc_heads.iter() {
            let mut block_to_check = rpc_head.clone();

            while block_to_check.number() >= lowest_block_number {
                if let Some(max_age) = self.max_head_block_age {
                    if block_to_check.age() > max_age {
                        break;
                    }
                }

                if !rpc.backup {
                    // backup nodes are excluded from the primary voting
                    let entry = primary_votes.entry(block_to_check.clone()).or_default();

                    entry.0.insert(rpc);
                    entry.1 += rpc.soft_limit;
                }

                // both primary and backup rpcs get included in the backup voting
                let backup_entry = backup_votes.entry(block_to_check.clone()).or_default();

                backup_entry.0.insert(rpc);
                backup_entry.1 += rpc.soft_limit;

                let parent_hash = block_to_check.parent_hash();

                match web3_rpcs.block(parent_hash, Some(rpc), None).await {
                    Ok(parent_block) => block_to_check = parent_block,
                    Err(err) => {
                        debug!(
                            "Problem fetching {:?} (parent of {:?}) during consensus finding: {:#?}",
                            parent_hash,
                            block_to_check.hash(),
                            err
                        );
                        break;
                    }
                }
            }
        }

        // we finished processing all tiers. check for primary results (if anything but the last tier found consensus, we already returned above)
        if let Some(consensus) = RankedRpcs::from_votes(
            web3_rpcs.min_synced_rpcs,
            web3_rpcs.min_sum_soft_limit,
            max_lag_block_number,
            primary_votes,
            self.rpc_heads.clone(),
        ) {
            return Ok(Some(consensus));
        }

        // primary votes didn't work. hopefully backup tiers are synced
        Ok(RankedRpcs::from_votes(
            web3_rpcs.min_synced_rpcs,
            web3_rpcs.min_sum_soft_limit,
            max_lag_block_number,
            backup_votes,
            self.rpc_heads.clone(),
        ))
    }

    pub fn best_tier(&self) -> Option<u32> {
        self.rpc_heads
            .iter()
            .map(|(x, _)| x.tier.load(atomic::Ordering::Relaxed))
            .min()
    }

    pub fn worst_tier(&self) -> Option<u32> {
        self.rpc_heads
            .iter()
            .map(|(x, _)| x.tier.load(atomic::Ordering::Relaxed))
            .max()
    }
}

fn best_rpc<'a>(rpc_a: &'a Arc<Web3Rpc>, rpc_b: &'a Arc<Web3Rpc>) -> &'a Arc<Web3Rpc> {
    let now = Instant::now();

    let faster = min_by_key(rpc_a, rpc_b, |x| {
        (x.next_available(now), x.backup, x.weighted_peak_latency())
    });

    trace!("{} vs {} = {}", rpc_a, rpc_b, faster);

    faster
}

impl RpcsForRequest {
    pub fn to_stream(self) -> impl Stream<Item = OpenRequestHandle> {
        stream! {
            trace!("entered stream");
            // TODO: get error_handler out of the web3_request? probably the authorization
            // let error_handler = web3_request.authorization.error_handler;
            let error_handler = None;

            let max_len = self.inner.len() + self.outer.len();

            // TODO: do this without having 3 Vecs
            let mut filtered = Vec::with_capacity(max_len);
            let mut attempted = HashSet::with_capacity(max_len);
            let mut completed = HashSet::with_capacity(max_len);

            // todo!("be sure to set server_error if we exit without any rpcs!");
            #[allow(clippy::never_loop)]
            loop {
                // if self.request.connect_timeout() {
                //     break;
                // } else {
                //     yield_now().await;
                // }

                let mut earliest_retry_at = None;
                let mut wait_for_sync = FuturesUnordered::new();

                // first check the inners, then the outers
                // for rpcs in [&self.inner, &self.outer] {
                for rpcs in [&self.inner] {

                    attempted.clear();

                    while attempted.len() + completed.len() < rpcs.len() {
                        filtered.clear();

                        // TODO: i'd like to do this without the collect, but since we push into `attempted`, having `attempted.contains` causes issues
                        filtered.extend(rpcs.iter().filter(|x| !(attempted.contains(x) || completed.contains(x))));

                        // tuple_windows doesn't do anything for single item iters. make the code DRY by just having it compare itself
                        if filtered.len() == 1 {
                            filtered.push(filtered[0]);
                        }

                        for (rpc_a, rpc_b) in filtered.iter().tuple_windows() {
                            // TODO: ties within X% to the server with the smallest block_data_limit?
                            // find rpc with the lowest weighted peak latency. backups always lose. rate limits always lose
                            // TODO: should next_available be reversed?
                            // TODO: this is similar to sort_for_load_balancing_on, but at this point we don't want to prefer tiers
                            // TODO: move ethis to a helper function just so we can test it
                            // TODO: should x.next_available should be Reverse<_>?
                            let best_rpc = best_rpc(rpc_a, rpc_b);

                            match best_rpc
                                .try_request_handle(&self.request, error_handler, false)
                                .await
                            {
                                Ok(OpenRequestResult::Handle(handle)) => {
                                    trace!("opened handle: {}", best_rpc);
                                    completed.insert(best_rpc);
                                    yield handle;
                                }
                                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                                    trace!(
                                        "retry on {} @ {}",
                                        best_rpc,
                                        retry_at.duration_since(Instant::now()).as_secs_f32()
                                    );
                                    attempted.insert(best_rpc);
                                    earliest_retry_at = earliest_retry_at.min(Some(retry_at, ));
                                }
                                Ok(OpenRequestResult::Lagged(x)) => {
                                    // this will probably always be the same block, right?
                                    trace!("{} is lagged. will not work now", best_rpc);
                                    attempted.insert(best_rpc);
                                    wait_for_sync.push(x);
                                }
                                Ok(OpenRequestResult::Failed) => {
                                    // TODO: log a warning? emit a stat?
                                    trace!("best_rpc not ready: {}", best_rpc);
                                    completed.insert(best_rpc);
                                }
                                Err(err) => {
                                    trace!("No request handle for {}. err={:?}", best_rpc, err);
                                    completed.insert(best_rpc);
                                }
                            }
                        }

                        debug_assert!(!(attempted.is_empty() && completed.is_empty()));
                    }
                }

                // if we got this far, no inner or outer rpcs are ready. thats suprising since an inner should have been
                break;

                let min_wait_until = Instant::now() + Duration::from_millis(100);

                // clear earliest_retry_at if it is too far in the future to help us
                if let Some(retry_at) = earliest_retry_at {
                    // set a minimum of 100ms. this is probably actually a bug we should figure out.
                    earliest_retry_at = Some(retry_at.max(min_wait_until));
                }

                let retry_at = earliest_retry_at.min(Some(self.request.connect_timeout_at())).expect("retry_at always set");

                if wait_for_sync.is_empty() {
                    sleep_until(retry_at).await;
                } else {
                    select!{
                        x = wait_for_sync.next() => {
                            match x {
                                Some(Ok(rpc)) => {
                                    trace!(%rpc, "rpc ready. it might be used on the next loop");

                                    // TODO: i don't think this sleep should be necessary. but i just want the cpus to cool down
                                    sleep_until(min_wait_until).await;
                                },
                                Some(Err(err)) => {
                                    error!(?err, "problem while waiting for an rpc for a request");

                                    // TODO: break or continue?
                                    // TODO: i don't think this sleep should be necessary. but i just want the cpus to cool down
                                    sleep_until(min_wait_until).await;
                                },
                                None => {
                                    // this would only happen if we got to the end of wait_for_sync. but we stop after the first result
                                    warn!("wait_for_sync is empty. how'd this happen?");
                                    break;
                                }
                            }
                        },
                        _ = sleep_until(retry_at) => {
                            // we've waited long enough that trying again might work
                        },
                    }
                }
            }
        }

        // TODO: log that no servers were available. this might not be a server error. the user might have requested something in the far future (common when people mix up chains)
    }
}
