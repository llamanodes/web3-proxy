///! Track the head block of all the web3 providers
use ethers::prelude::{Block, TxHash};
use std::cmp;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{self, AtomicU64};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch, Mutex};
use tracing::info;

// TODO: what type for the Item? String url works, but i don't love it
pub type NewHead = (String, Block<TxHash>);

pub type BlockWatcherSender = mpsc::UnboundedSender<NewHead>;
pub type BlockWatcherReceiver = mpsc::UnboundedReceiver<NewHead>;

// TODO: ethers has a similar SyncingStatus
#[derive(Eq)]
pub enum SyncStatus {
    Synced(u64),
    Behind(u64),
    Unknown,
}

impl Ord for SyncStatus {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        match (self, other) {
            (SyncStatus::Synced(a), SyncStatus::Synced(b)) => a.cmp(b),
            (SyncStatus::Synced(_), SyncStatus::Unknown) => cmp::Ordering::Greater,
            (SyncStatus::Unknown, SyncStatus::Synced(_)) => cmp::Ordering::Less,
            (SyncStatus::Unknown, SyncStatus::Unknown) => cmp::Ordering::Equal,
            (SyncStatus::Synced(_), SyncStatus::Behind(_)) => cmp::Ordering::Greater,
            (SyncStatus::Behind(_), SyncStatus::Synced(_)) => cmp::Ordering::Less,
            (SyncStatus::Behind(_), SyncStatus::Unknown) => cmp::Ordering::Greater,
            (SyncStatus::Behind(a), SyncStatus::Behind(b)) => a.cmp(b),
            (SyncStatus::Unknown, SyncStatus::Behind(_)) => cmp::Ordering::Less,
        }
    }
}

impl PartialOrd for SyncStatus {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SyncStatus {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == cmp::Ordering::Equal
    }
}

pub struct BlockWatcher {
    sender: BlockWatcherSender,
    /// this Mutex is locked over awaits, so we want an async lock
    receiver: Mutex<BlockWatcherReceiver>,
    // TODO: better key
    block_numbers: HashMap<String, AtomicU64>,
    head_block_number: AtomicU64,
}

impl fmt::Debug for BlockWatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        write!(f, "BlockWatcher(...)")
    }
}

impl BlockWatcher {
    pub fn new(rpcs: Vec<String>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let block_numbers = rpcs.into_iter().map(|rpc| (rpc, 0.into())).collect();

        Self {
            sender,
            receiver: Mutex::new(receiver),
            block_numbers,
            head_block_number: Default::default(),
        }
    }

    pub fn clone_sender(&self) -> BlockWatcherSender {
        self.sender.clone()
    }

    pub fn sync_status(&self, rpc: &str, allowed_lag: u64) -> SyncStatus {
        match (
            self.head_block_number.load(atomic::Ordering::Acquire),
            self.block_numbers.get(rpc),
        ) {
            (0, _) => SyncStatus::Unknown,
            (_, None) => SyncStatus::Unknown,
            (head_block_number, Some(rpc_block_number)) => {
                let rpc_block_number = rpc_block_number.load(atomic::Ordering::Acquire);

                match head_block_number.cmp(&rpc_block_number) {
                    cmp::Ordering::Equal => SyncStatus::Synced(0),
                    cmp::Ordering::Greater => {
                        // this probably won't happen, but it might if the block arrives at the exact wrong time
                        // TODO: should this be negative?
                        SyncStatus::Synced(0)
                    }
                    cmp::Ordering::Less => {
                        // allow being some behind
                        let lag = head_block_number - rpc_block_number;

                        if lag <= allowed_lag {
                            SyncStatus::Synced(lag)
                        } else {
                            SyncStatus::Behind(lag)
                        }
                    }
                }
            }
        }
    }

    pub async fn run(
        self: Arc<Self>,
        new_block_sender: watch::Sender<String>,
    ) -> anyhow::Result<()> {
        let mut receiver = self.receiver.lock().await;

        while let Some((rpc, new_block)) = receiver.recv().await {
            let new_block_number = new_block.number.unwrap().as_u64();

            {
                if let Some(rpc_block_number) = self.block_numbers.get(&rpc) {
                    let rpc_block_number = rpc_block_number.load(atomic::Ordering::Acquire);

                    // if we already have this block height
                    // this probably own't happen with websockets, but is likely with polling against http rpcs
                    // TODO: should we compare more than just height? hash too?
                    if rpc_block_number == new_block_number {
                        continue;
                    }
                }
            }

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs() as i64;

            // save the block for this rpc
            // TODO: store the actual chain as a graph and then have self.blocks point to that?
            self.block_numbers
                .get(&rpc)
                .unwrap()
                .swap(new_block_number, atomic::Ordering::Release);

            let head_number = self.head_block_number.load(atomic::Ordering::Acquire);

            let label_slow_heads = if head_number == 0 {
                // first block seen
                self.head_block_number
                    .swap(new_block_number, atomic::Ordering::AcqRel);
                ", +".to_string()
            } else {
                // TODO: what if they have the same number but different hashes?
                // TODO: alert if there is a large chain split?
                match (new_block_number).cmp(&head_number) {
                    cmp::Ordering::Equal => {
                        // this block is already saved as the head
                        "".to_string()
                    }
                    cmp::Ordering::Greater => {
                        // new_block is the new head_block
                        self.head_block_number
                            .swap(new_block_number, atomic::Ordering::AcqRel);
                        ", +".to_string()
                    }
                    cmp::Ordering::Less => {
                        // this rpc is behind
                        let lag = new_block_number as i64 - head_number as i64;

                        let mut s = ", ".to_string();

                        s.push_str(&lag.to_string());

                        s
                    }
                }
            };

            // have the provider tiers update_synced_rpcs
            new_block_sender.send(rpc.clone())?;

            // TODO: include time since last update?
            info!(
                "{:?} = {}, {}, {} sec{}",
                new_block.hash.unwrap(),
                new_block.number.unwrap(),
                rpc,
                now - new_block.timestamp.as_u64() as i64,
                label_slow_heads
            );
        }

        Ok(())
    }
}
