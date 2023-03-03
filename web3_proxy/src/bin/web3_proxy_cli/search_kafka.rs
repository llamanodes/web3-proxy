use anyhow::Context;
use argh::FromArgs;
use futures::TryStreamExt;
use log::info;
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    ClientConfig, Message,
};
use std::num::NonZeroU64;
use web3_proxy::{config::TopConfig, frontend::authorization::RpcSecretKey};

/// Second subcommand.
#[derive(FromArgs, PartialEq, Debug, Eq)]
#[argh(subcommand, name = "search_kafka")]
pub struct SearchKafkaSubCommand {
    #[argh(positional)]
    group_id: String,
    #[argh(positional)]
    input_topic: String,
    #[argh(option)]
    /// rpc_key to search. Be careful when handling keys!
    rpc_key: Option<RpcSecretKey>,
    #[argh(option)]
    /// rpc_key_id to search
    rpc_key_id: Option<NonZeroU64>,
}

impl SearchKafkaSubCommand {
    pub async fn main(self, top_config: TopConfig) -> anyhow::Result<()> {
        let brokers = top_config
            .app
            .kafka_urls
            .context("top_config.app.kafka_urls is required")?;

        // TODO: headers
        // TODO: headers
        let consumer: StreamConsumer = ClientConfig::new()
            .set("group.id", &self.group_id)
            .set("bootstrap.servers", &brokers)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .create()
            .context("Consumer creation failed")?;

        consumer
            .subscribe(&[&self.input_topic])
            .expect("Can't subscribe to specified topic");

        let stream_processor = consumer.stream().try_for_each(|msg| async move {
            info!("Message received: {}", msg.offset());

            Ok(())
        });

        stream_processor.await?;

        Ok(())
    }
}
