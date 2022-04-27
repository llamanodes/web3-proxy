///! Track the head block of all the web3 providers
use ethers::prelude::{Block, TxHash};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, RwLock};
use tracing::info;

// TODO: what type for the Item? String url works, but i don't love it
pub type NewHead = (String, Block<TxHash>);

pub type BlockWatcherSender = mpsc::UnboundedSender<NewHead>;
pub type BlockWatcherReceiver = mpsc::UnboundedReceiver<NewHead>;

pub struct BlockWatcher {
    sender: BlockWatcherSender,
    receiver: RwLock<BlockWatcherReceiver>,
    /// TODO: i don't think we want a hashmap. we want a left-right or some other concurrent map
    blocks: RwLock<HashMap<String, Block<TxHash>>>,
    latest_block: RwLock<Option<Block<TxHash>>>,
}

impl BlockWatcher {
    pub fn new() -> Self {
        // TODO: this also needs to return a reader for blocks
        let (sender, receiver) = mpsc::unbounded_channel();

        Self {
            sender,
            receiver: RwLock::new(receiver),
            blocks: Default::default(),
            latest_block: RwLock::new(None),
        }
    }

    pub fn clone_sender(&self) -> BlockWatcherSender {
        self.sender.clone()
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let mut receiver = self.receiver.write().await;

        while let Some((rpc, block)) = receiver.recv().await {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs() as i64;

            {
                let blocks = self.blocks.read().await;
                if blocks.get(&rpc) == Some(&block) {
                    // we already have this block
                    continue;
                }
            }

            // save the block for this rpc
            self.blocks.write().await.insert(rpc.clone(), block.clone());

            // TODO: we don't always need this to have a write lock
            let mut latest_block = self.latest_block.write().await;

            let label_slow_blocks = if latest_block.is_none() {
                *latest_block = Some(block.clone());
                "+"
            } else {
                // TODO: what if they have the same number but different hashes? or aren't on the same chain?
                match block.number.cmp(&latest_block.as_ref().unwrap().number) {
                    Ordering::Equal => "",
                    Ordering::Greater => {
                        *latest_block = Some(block.clone());
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
                block.hash.unwrap(),
                rpc,
                // TODO: human readable time?
                block.timestamp,
                block.number.unwrap(),
                now - block.timestamp.as_u64() as i64,
                label_slow_blocks
            );
        }

        Ok(())
    }
}
