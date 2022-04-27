///! Track the head block of all the web3 providers
use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use ethers::prelude::{Block, TxHash};
use std::cmp::Ordering;
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
    blocks: DashMap<String, Arc<Block<TxHash>>>,
    head_block: ArcSwapOption<Block<TxHash>>,
}

impl BlockWatcher {
    pub fn new() -> Self {
        // TODO: this also needs to return a reader for blocks
        let (sender, receiver) = mpsc::unbounded_channel();

        Self {
            sender,
            receiver: Mutex::new(receiver),
            blocks: Default::default(),
            head_block: Default::default(),
        }
    }

    pub fn clone_sender(&self) -> BlockWatcherSender {
        self.sender.clone()
    }

    pub async fn is_synced(&self, rpc: String, allowed_lag: u64) -> anyhow::Result<bool> {
        match self.head_block.load().as_ref() {
            None => Ok(false),
            Some(latest_block) => {
                match self.blocks.get(&rpc) {
                    None => Ok(false),
                    Some(rpc_block) => {
                        match latest_block.number.cmp(&rpc_block.number) {
                            Ordering::Equal => Ok(true),
                            Ordering::Greater => {
                                // this probably won't happen, but it might if the block arrives at the exact wrong time
                                Ok(true)
                            }
                            Ordering::Less => {
                                // allow being some behind
                                let lag = latest_block.number.expect("no block")
                                    - rpc_block.number.expect("no block");
                                Ok(lag.as_u64() <= allowed_lag)
                            }
                        }
                    }
                }
            }
        }
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let mut receiver = self.receiver.lock().await;

        while let Some((rpc, new_block)) = receiver.recv().await {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs() as i64;

            {
                if let Some(current_block) = self.blocks.get(&rpc) {
                    // if we already have this block height
                    // TODO: should we compare more than just height? hash too?
                    if current_block.number == new_block.number {
                        continue;
                    }
                }
            }

            let rpc_block = Arc::new(new_block);

            // save the block for this rpc
            // TODO: this could be more efficient. store the actual chain as a graph and then have self.blocks point to that
            self.blocks.insert(rpc.clone(), rpc_block.clone());

            let head_number = self.head_block.load();

            let label_slow_heads = if head_number.is_none() {
                self.head_block.swap(Some(rpc_block.clone()));
                "+"
            } else {
                // TODO: what if they have the same number but different hashes?
                // TODO: alert if there is a large chain split?
                let head_number = head_number
                    .as_ref()
                    .expect("should be impossible")
                    .number
                    .expect("should be impossible");

                let rpc_number = rpc_block.number.expect("should be impossible");

                match rpc_number.cmp(&head_number) {
                    Ordering::Equal => {
                        // this block is saved
                        ""
                    }
                    Ordering::Greater => {
                        // new_block is the new head_block
                        self.head_block.swap(Some(rpc_block.clone()));
                        "+"
                    }
                    Ordering::Less => {
                        // TODO: include how many blocks behind?
                        "-"
                    }
                }
            };

            // TODO: include time since last update?
            info!(
                "{:?} = {} Ts: {:?}, block number: {}, age: {}s {}",
                rpc_block.hash.unwrap(),
                rpc,
                // TODO: human readable time?
                rpc_block.timestamp,
                rpc_block.number.unwrap(),
                now - rpc_block.timestamp.as_u64() as i64,
                label_slow_heads
            );
        }

        Ok(())
    }
}
