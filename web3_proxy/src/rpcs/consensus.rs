use super::blockchain::Web3ProxyBlock;
use super::many::Web3Rpcs;
use super::one::Web3Rpc;
use super::transactions::TxStatus;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::frontend::authorization::Authorization;
use base64::engine::general_purpose;
use derive_more::Constructor;
use ethers::prelude::{H256, U64};
use hashbrown::{HashMap, HashSet};
use hdrhistogram::serialization::{Serializer, V2DeflateSerializer};
use hdrhistogram::Histogram;
use itertools::{Itertools, MinMaxResult};
use moka::future::Cache;
use serde::Serialize;
use std::cmp::{Ordering, Reverse};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tracing::{debug, enabled, trace, warn, Level};

#[derive(Clone, Serialize)]
struct ConsensusRpcData {
    head_block_num: U64,
    // TODO: this is too simple. erigon has 4 prune levels (hrct)
    oldest_block_num: U64,
}

impl ConsensusRpcData {
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
    tier: u32,
    backup: bool,
    head_num: Option<U64>,
}

impl RpcRanking {
    pub fn default_with_backup(backup: bool) -> Self {
        Self {
            backup,
            ..Default::default()
        }
    }

    fn sort_key(&self) -> (bool, u32, Reverse<Option<U64>>) {
        // TODO: add soft_limit here? add peak_ewma here?
        // TODO: should backup or tier be checked first? now that tiers are automated, backups
        // TODO: should we include a random number in here?
        // TODO: should we include peak_ewma_latency or weighted_peak_ewma_latency?
        (!self.backup, self.tier, Reverse(self.head_num))
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
    // BackupReady,
    Wait { current: Option<U64> },
    // WaitForBackup { current: Option<U64> },
    NeverReady,
}

/// A collection of Web3Rpcs that are on the same block.
/// Serialize is so we can print it on our /status endpoint
/// TODO: remove head_block/head_rpcs/tier and replace with one RankedRpcMap
/// TODO: add `best_rpc(method_data_kind, min_block_needed, max_block_needed, include_backups)`
#[derive(Clone, Serialize)]
pub struct ConsensusWeb3Rpcs {
    pub(crate) tier: u32,
    pub(crate) backups_needed: bool,

    // TODO: Don't skip serializing, instead make a shorter serialize
    #[serde(skip_serializing)]
    pub(crate) head_block: Web3ProxyBlock,

    // TODO: make a shorter serialize
    pub(crate) head_rpcs: Vec<Arc<Web3Rpc>>,

    // TODO: make serializing work. the key needs to be a string. I think we need `serialize_with`
    #[serde(skip_serializing)]
    pub(crate) other_rpcs: RankedRpcMap,

    // TODO: make serializing work. the key needs to be a string. I think we need `serialize_with`
    #[serde(skip_serializing)]
    rpc_data: HashMap<Arc<Web3Rpc>, ConsensusRpcData>,
}

impl ConsensusWeb3Rpcs {
    #[inline]
    pub fn num_consensus_rpcs(&self) -> usize {
        self.head_rpcs.len()
    }

    /// will tell you if waiting will eventually  should wait for a block
    /// TODO: return if backup will be needed to serve the request
    /// TODO: serve now if a backup server has the data
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
        authorization: &Arc<Authorization>,
        rpc: Option<&Arc<Web3Rpc>>,
        new_block: Option<Web3ProxyBlock>,
    ) -> Web3ProxyResult<()> {
        let new_consensus_rpcs = match self
            .find_consensus_connections(authorization, web3_rpcs)
            .await
        {
            Err(err) => {
                return Err(err).web3_context("error while finding consensus head block!");
            }
            Ok(None) => {
                return Err(Web3ProxyError::NoConsensusHeadBlock);
            }
            Ok(Some(x)) => x,
        };

        trace!("new_synced_connections: {:#?}", new_consensus_rpcs);

        let watch_consensus_head_sender = web3_rpcs.watch_consensus_head_sender.as_ref().unwrap();
        let consensus_tier = new_consensus_rpcs.tier;
        // TODO: think more about the default for total_tiers
        let total_tiers = self.worst_tier().unwrap_or_default();
        let backups_needed = new_consensus_rpcs.backups_needed;
        let consensus_head_block = new_consensus_rpcs.head_block.clone();
        let num_consensus_rpcs = new_consensus_rpcs.num_consensus_rpcs();
        let num_active_rpcs = self.len();
        let total_rpcs = web3_rpcs.len();

        let new_consensus_rpcs = Arc::new(new_consensus_rpcs);

        let old_consensus_head_connections = web3_rpcs
            .watch_consensus_rpcs_sender
            .send_replace(Some(new_consensus_rpcs.clone()));

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

        match old_consensus_head_connections.as_ref() {
            None => {
                debug!(
                    "first {}/{} {}{}/{}/{} block={}, rpc={}",
                    consensus_tier,
                    total_tiers,
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

                match consensus_head_block.number().cmp(old_head_block.number()) {
                    Ordering::Equal => {
                        // multiple blocks with the same fork!
                        if consensus_head_block.hash() == old_head_block.hash() {
                            // no change in hash. no need to use watch_consensus_head_sender
                            // TODO: trace level if rpc is backup
                            debug!(
                                "con {}/{} {}{}/{}/{} con={} rpc={}",
                                consensus_tier,
                                total_tiers,
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
                                consensus_tier,
                                total_tiers,
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
                            consensus_tier,
                            total_tiers,
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
                        debug!(
                            "new {}/{} {}{}/{}/{} con={} rpc={}",
                            consensus_tier,
                            total_tiers,
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

        Ok(())
    }

    pub(super) async fn process_block_from_rpc(
        &mut self,
        web3_rpcs: &Web3Rpcs,
        authorization: &Arc<Authorization>,
        new_block: Option<Web3ProxyBlock>,
        rpc: Arc<Web3Rpc>,
        _pending_tx_sender: &Option<broadcast::Sender<TxStatus>>,
    ) -> Web3ProxyResult<()> {
        // TODO: how should we handle an error here?
        if !self
            .update_rpc(new_block.clone(), rpc.clone(), web3_rpcs)
            .await
            .web3_context("failed to update rpc")?
        {
            // nothing changed. no need to scan for a new consensus head
            return Ok(());
        }

        self.refresh(web3_rpcs, authorization, Some(&rpc), new_block)
            .await
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
        rpc.head_delay.write().record_secs(latency.as_secs_f32());

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
        authorization: &Arc<Authorization>,
        web3_rpcs: &Web3Rpcs,
    ) -> Web3ProxyResult<Option<ConsensusWeb3Rpcs>> {
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

        let lowest_block_number = lowest_block.number().max(&max_lag_block_number);

        // TODO: should lowest block number be set such that the rpc won't ever go backwards?
        trace!("safe lowest_block_number: {}", lowest_block_number);

        let num_known = self.rpc_heads.len();

        if num_known < web3_rpcs.min_synced_rpcs {
            // this keeps us from serving requests when the proxy first starts
            trace!("not enough servers known");
            return Ok(None);
        }

        // TODO: also track the sum of *available* hard_limits? if any servers have no hard limits, use their soft limit or no limit?
        // TODO: struct for the value of the votes hashmap?
        let mut primary_votes: HashMap<Web3ProxyBlock, (HashSet<&Arc<Web3Rpc>>, u32)> =
            Default::default();
        let mut backup_votes: HashMap<Web3ProxyBlock, (HashSet<&Arc<Web3Rpc>>, u32)> =
            Default::default();

        for (rpc, rpc_head) in self.rpc_heads.iter() {
            let mut block_to_check = rpc_head.clone();

            while block_to_check.number() >= lowest_block_number {
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

                // we used to specify rpc on this, but it shouldn't be necessary
                let parent_hash = block_to_check.parent_hash();
                match web3_rpcs
                    .block(authorization, parent_hash, None, None)
                    .await
                {
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
        if let Some(consensus) = self.count_votes(&primary_votes, web3_rpcs) {
            return Ok(Some(consensus));
        }

        // primary votes didn't work. hopefully backup tiers are synced
        Ok(self.count_votes(&backup_votes, web3_rpcs))
    }

    // TODO: have min_sum_soft_limit and min_head_rpcs on self instead of on Web3Rpcs
    fn count_votes(
        &self,
        votes: &HashMap<Web3ProxyBlock, (HashSet<&Arc<Web3Rpc>>, u32)>,
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
            if rpc_names.len() < web3_rpcs.min_synced_rpcs {
                continue;
            }

            trace!("rpc_names: {:#?}", rpc_names);

            if rpc_names.len() < web3_rpcs.min_synced_rpcs {
                continue;
            }

            // consensus found!
            let consensus_rpcs: Vec<Arc<_>> = rpc_names.iter().map(|x| Arc::clone(x)).collect();

            let tier = consensus_rpcs
                .iter()
                .map(|x| x.tier.load(atomic::Ordering::Relaxed))
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

                let key: RpcRanking = RpcRanking::new(
                    x.tier.load(atomic::Ordering::Relaxed),
                    x.backup,
                    Some(x_head_num),
                );

                other_rpcs
                    .entry(key)
                    .or_insert_with(Vec::new)
                    .push(x.clone());
            }

            // TODO: how should we populate this?
            let mut rpc_data = HashMap::with_capacity(self.rpc_heads.len());

            for (x, x_head) in self.rpc_heads.iter() {
                let y = ConsensusRpcData::new(x, x_head);

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

    pub fn worst_tier(&self) -> Option<u32> {
        self.rpc_heads
            .iter()
            .map(|(x, _)| x.tier.load(atomic::Ordering::Relaxed))
            .max()
    }
}

#[cfg(test)]
mod test {
    // #[test]
    // fn test_simplest_case_consensus_head_connections() {
    //     todo!();
    // }
}
