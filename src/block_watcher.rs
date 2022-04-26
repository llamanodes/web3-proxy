use ethers::prelude::{Block, TxHash};
use governor::clock::{Clock, QuantaClock, QuantaInstant};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{info, warn};

// TODO: what type for the Item? String url works, but i don't love it
// TODO: think about boxing this
#[derive(Debug)]
pub enum BlockWatcherItem {
    NewHead(Box<(String, Block<TxHash>)>),
    SubscribeHttp(String),
    Interval,
}

pub type BlockWatcherSender = mpsc::UnboundedSender<BlockWatcherItem>;
pub type BlockWatcherReceiver = mpsc::UnboundedReceiver<BlockWatcherItem>;

pub struct BlockWatcher {
    clock: QuantaClock,
    sender: BlockWatcherSender,
    receiver: BlockWatcherReceiver,
    last_poll: QuantaInstant,
    /// TODO: i don't think we want a hashmap. we want a left-right or some other concurrent map
    blocks: HashMap<String, Block<TxHash>>,
}

impl BlockWatcher {
    pub fn new(clock: QuantaClock) -> (BlockWatcher, BlockWatcherSender) {
        // TODO: this also needs to return a reader for blocks
        let (sender, receiver) = mpsc::unbounded_channel();

        let last_poll = clock.now();

        let watcher = Self {
            clock,
            last_poll,
            sender: sender.clone(),
            receiver,
            blocks: Default::default(),
        };

        (watcher, sender)
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        // TODO: we should probably set this to something like blocktime / 3
        let mut poll_interval = interval(Duration::from_secs(2));
        // TODO: think more about what missed tick behavior we want
        // interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // TODO: only do this if we have http providers to watch
        let interval_sender = self.sender.clone();
        tokio::spawn(async move {
            loop {
                poll_interval.tick().await;

                interval_sender
                    .send(BlockWatcherItem::Interval)
                    .expect("sending BlockWatcherItem::Interval failed");
            }
        });

        // don't poll faster than every second
        let min_wait_nanos = 1_000_000_000.into();

        while let Some(x) = self.receiver.recv().await {
            match x {
                BlockWatcherItem::Interval => {
                    if self.clock.now() < self.last_poll + min_wait_nanos {
                        // we already updated recently
                        continue;
                    }

                    // TODO: we got an interval. if we haven't updated the blocks recently,
                    info!("TODO: query all the subscribed http providers")
                }
                BlockWatcherItem::NewHead(new_head) => {
                    let (rpc, block) = *new_head;
                    info!(
                        "{:?} = {} Ts: {:?}, block number: {}",
                        block.hash.unwrap(),
                        rpc,
                        block.timestamp,
                        block.number.unwrap(),
                    );
                    self.blocks.insert(rpc, block);

                    self.sender
                        .send(BlockWatcherItem::Interval)
                        .expect("sending BlockWatcherItem::Interval failed");
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
