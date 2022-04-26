///! Track the head block of all the web3 providers
use ethers::prelude::{Block, TxHash};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::info;

// TODO: what type for the Item? String url works, but i don't love it
pub type NewHead = (String, Block<TxHash>);

pub type BlockWatcherSender = mpsc::UnboundedSender<NewHead>;
pub type BlockWatcherReceiver = mpsc::UnboundedReceiver<NewHead>;

pub struct BlockWatcher {
    receiver: BlockWatcherReceiver,
    /// TODO: i don't think we want a hashmap. we want a left-right or some other concurrent map
    blocks: HashMap<String, Block<TxHash>>,
    latest_block: Option<Block<TxHash>>,
}

impl BlockWatcher {
    pub fn new() -> (BlockWatcher, BlockWatcherSender) {
        // TODO: this also needs to return a reader for blocks
        let (sender, receiver) = mpsc::unbounded_channel();

        let watcher = Self {
            receiver,
            blocks: Default::default(),
            latest_block: None,
        };

        (watcher, sender)
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        while let Some((rpc, block)) = self.receiver.recv().await {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs() as i64;

            let current_block = self.blocks.get(&rpc);

            if current_block == Some(&block) {
                // we already have this block
                continue;
            }

            let label_slow_blocks = if self.latest_block.is_none() {
                self.latest_block = Some(block.clone());
                "+"
            } else {
                let latest_block = self.latest_block.as_ref().unwrap();

                // TODO: what if they have the same number but different hashes? or aren't on the same chain?
                match block.number.cmp(&latest_block.number) {
                    Ordering::Equal => "",
                    Ordering::Greater => {
                        self.latest_block = Some(block.clone());
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
            self.blocks.insert(rpc, block);
        }

        Ok(())
    }
}
