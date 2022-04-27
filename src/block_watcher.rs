///! Track the head block of all the web3 providers
use dashmap::DashMap;
use ethers::prelude::{Block, TxHash};
use std::cmp;
use std::sync::atomic::{self, AtomicU64};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex};
use tracing::info;

// TODO: what type for the Item? String url works, but i don't love it
pub type NewHead = (String, Block<TxHash>);

pub type BlockWatcherSender = mpsc::UnboundedSender<NewHead>;
pub type BlockWatcherReceiver = mpsc::UnboundedReceiver<NewHead>;

pub struct BlockWatcher {
    sender: BlockWatcherSender,
    receiver: Mutex<BlockWatcherReceiver>,
    // TODO: i don't think we want a RwLock. we want an ArcSwap or something
    // TODO: should we just store the block number?
    block_numbers: DashMap<String, u64>,
    head_block_number: AtomicU64,
}

impl BlockWatcher {
    pub fn new() -> Self {
        // TODO: this also needs to return a reader for blocks
        let (sender, receiver) = mpsc::unbounded_channel();

        Self {
            sender,
            receiver: Mutex::new(receiver),
            block_numbers: Default::default(),
            head_block_number: Default::default(),
        }
    }

    pub fn clone_sender(&self) -> BlockWatcherSender {
        self.sender.clone()
    }

    pub async fn is_synced(&self, rpc: String, allowed_lag: u64) -> anyhow::Result<bool> {
        match (
            self.head_block_number.load(atomic::Ordering::SeqCst),
            self.block_numbers.get(&rpc),
        ) {
            (0, _) => Ok(false),
            (_, None) => Ok(false),
            (head_block_number, Some(rpc_block_number)) => {
                match head_block_number.cmp(&rpc_block_number) {
                    cmp::Ordering::Equal => Ok(true),
                    cmp::Ordering::Greater => {
                        // this probably won't happen, but it might if the block arrives at the exact wrong time
                        Ok(true)
                    }
                    cmp::Ordering::Less => {
                        // allow being some behind
                        // TODO: why do we need a clone here?
                        let lag = head_block_number - *rpc_block_number;
                        Ok(lag <= allowed_lag)
                    }
                }
            }
        }
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let mut receiver = self.receiver.lock().await;

        while let Some((rpc, new_block)) = receiver.recv().await {
            let new_block_number = new_block.number.unwrap().as_u64();

            {
                if let Some(rpc_block_number) = self.block_numbers.get(&rpc) {
                    // if we already have this block height
                    // this probably own't happen with websockets, but is likely with polling against http rpcs
                    // TODO: should we compare more than just height? hash too?
                    if *rpc_block_number == new_block_number {
                        continue;
                    }
                }
            }

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs() as i64;

            // save the block for this rpc
            // TODO:store the actual chain as a graph and then have self.blocks point to that?
            self.block_numbers.insert(rpc.clone(), new_block_number);

            let head_number = self.head_block_number.load(atomic::Ordering::SeqCst);

            let label_slow_heads = if head_number == 0 {
                self.head_block_number
                    .swap(new_block_number, atomic::Ordering::SeqCst);
                "+".to_string()
            } else {
                // TODO: what if they have the same number but different hashes?
                // TODO: alert if there is a large chain split?
                match (new_block_number).cmp(&head_number) {
                    cmp::Ordering::Equal => {
                        // this block is saved
                        "".to_string()
                    }
                    cmp::Ordering::Greater => {
                        // new_block is the new head_block
                        self.head_block_number
                            .swap(new_block_number, atomic::Ordering::SeqCst);
                        "+".to_string()
                    }
                    cmp::Ordering::Less => {
                        // TODO: include how many blocks behind?
                        let lag = new_block_number as i64 - head_number as i64;
                        lag.to_string()
                    }
                }
            };

            // TODO: include time since last update?
            info!(
                "{:?} = {}, {}, {} sec, {}",
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
