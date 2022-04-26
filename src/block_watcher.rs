use ethers::prelude::{Block, TxHash};
use governor::clock::{Clock, QuantaClock, QuantaInstant};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{info, warn};

// TODO: what type for the Item? String url works, but i don't love it
// TODO: think about boxing this
#[derive(Debug)]
pub enum BlockWatcherItem {
    NewHead((String, Block<TxHash>)),
    SubscribeHttp(String),
    Interval,
}

pub type BlockWatcherSender = mpsc::UnboundedSender<BlockWatcherItem>;
pub type BlockWatcherReceiver = mpsc::UnboundedReceiver<BlockWatcherItem>;

pub struct BlockWatcher {
    clock: QuantaClock,
    receiver: BlockWatcherReceiver,
    last_update: QuantaInstant,
    /// TODO: i don't think we want a hashmap. we want a left-right or some other concurrent map
    blocks: HashMap<String, Block<TxHash>>,
}

impl BlockWatcher {
    pub fn new(clock: QuantaClock) -> (BlockWatcher, BlockWatcherSender) {
        // TODO: this also needs to return a reader for blocks
        let (sender, receiver) = mpsc::unbounded_channel();

        let last_update = clock.now();

        let watcher = Self {
            clock,
            last_update,
            receiver,
            blocks: Default::default(),
        };

        (watcher, sender)
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        // TODO:

        while let Some(x) = self.receiver.recv().await {
            match x {
                BlockWatcherItem::Interval => {
                    // TODO: we got an interval. if we haven't updated the blocks recently,
                }
                BlockWatcherItem::NewHead((rpc, block)) => {
                    info!(
                        "{:?} = {} Ts: {:?}, block number: {}",
                        block.hash.unwrap(),
                        rpc,
                        block.timestamp,
                        block.number.unwrap(),
                    );
                    self.blocks.insert(rpc, block);

                    self.last_update = self.clock.now();
                }
                BlockWatcherItem::SubscribeHttp(rpc) => {
                    warn!("subscribing to {} is not yet supported", rpc);
                }
            }
        }

        Ok(())
    }
}

/*
pub async fn save_block(
    &self,
    url: String,
    block_watcher_sender: BlockWatcherSender,
    block: Block<TxHash>,
) -> anyhow::Result<()> {
    // TODO: include the block age (compared to local time) in this, too
    // TODO: use tracing properly

}
*/
