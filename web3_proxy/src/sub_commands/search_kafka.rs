use crate::{config::TopConfig, frontend::authorization::RpcSecretKey, relational_db::get_db};
use anyhow::Context;
use argh::FromArgs;
use entities::rpc_key;
use futures::TryStreamExt;
use migration::sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    ClientConfig, Message,
};
use std::num::NonZeroU64;
use tracing::info;
use uuid::Uuid;

/// Second subcommand.
#[derive(FromArgs, PartialEq, Debug, Eq)]
#[argh(subcommand, name = "search_kafka")]
pub struct SearchKafkaSubCommand {
    #[argh(positional)]
    /// topics to read
    topics: Vec<String>,
    #[argh(option)]
    /// optional group id
    group_id: Option<String>,
    #[argh(option)]
    /// rpc_key to search. Be careful when handling keys!
    rpc_key: Option<RpcSecretKey>,
    #[argh(option)]
    /// rpc_key_id to search
    rpc_key_id: Option<NonZeroU64>,
}

impl SearchKafkaSubCommand {
    pub async fn main(self, top_config: TopConfig) -> anyhow::Result<()> {
        let mut rpc_key_id = self.rpc_key_id.map(|x| x.get());

        if let Some(rpc_key) = self.rpc_key {
            let db_conn = get_db(top_config.app.db_url.unwrap(), 1, 1).await?;

            let rpc_key: Uuid = rpc_key.into();

            let x = rpc_key::Entity::find()
                .filter(rpc_key::Column::SecretKey.eq(rpc_key))
                .one(&db_conn)
                .await?
                .context("key not found")?;

            rpc_key_id = Some(x.id);
        }

        let wanted_kafka_key = rpc_key_id.map(|x| rmp_serde::to_vec(&x).unwrap());

        let wanted_kafka_key = wanted_kafka_key.as_ref().map(|x| &x[..]);

        let kafka_brokers = top_config
            .app
            .kafka_urls
            .context("top_config.app.kafka_urls is required")?;

        // TODO: sea-streamer instead of rdkafka?
        let mut consumer = ClientConfig::new();

        let security_protocol = &top_config.app.kafka_protocol;

        consumer
            .set("bootstrap.servers", &kafka_brokers)
            .set("enable.partition.eof", "false")
            .set("security.protocol", security_protocol)
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false");

        if let Some(group_id) = self.group_id {
            consumer.set("group.id", &group_id);
        }

        let consumer: StreamConsumer = consumer
            .create()
            .context("kafka consumer creation failed")?;

        let topics: Vec<&str> = self.topics.iter().map(String::as_ref).collect();

        // TODO: how should we set start/end timestamp for the consumer? i think we need to look at metadata
        consumer
            .subscribe(&topics)
            .expect("Can't subscribe to specified topic");

        let stream_processor = consumer.stream().try_for_each(|msg| async move {
            if msg.key() != wanted_kafka_key {
                return Ok(());
            }

            // TODO: filter by headers?

            info!("msg: {}", msg.offset());

            // TODO: now what?

            Ok(())
        });

        stream_processor.await?;

        Ok(())
    }
}
