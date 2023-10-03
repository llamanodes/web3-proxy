use crate::app::Web3ProxyApp;
use crate::frontend::authorization::{Authorization, RequestOrMethod};
use core::fmt;
use ethers::types::U64;
use rdkafka::message::{Header as KafkaHeader, OwnedHeaders as KafkaOwnedHeaders, OwnedMessage};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::util::Timeout as KafkaTimeout;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::error;
use ulid::Ulid;

pub struct KafkaDebugLogger {
    topic: String,
    key: Vec<u8>,
    headers: KafkaOwnedHeaders,
    producer: FutureProducer,
    num_requests: AtomicUsize,
    num_responses: AtomicUsize,
}

impl fmt::Debug for KafkaDebugLogger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KafkaDebugLogger")
            .field("topic", &self.topic)
            .finish_non_exhaustive()
    }
}

type KafkaLogResult = Result<(i32, i64), (rdkafka::error::KafkaError, OwnedMessage)>;

impl KafkaDebugLogger {
    pub fn try_new(
        app: &Web3ProxyApp,
        authorization: Arc<Authorization>,
        head_block_num: Option<U64>,
        kafka_topic: &str,
        request_ulid: Ulid,
    ) -> Option<Arc<Self>> {
        let kafka_producer = app.kafka_producer.clone()?;

        let kafka_topic = kafka_topic.to_string();

        let rpc_secret_key_id = authorization
            .checks
            .rpc_secret_key_id
            .map(|x| x.get())
            .unwrap_or_default();

        let kafka_key =
            rmp_serde::to_vec(&rpc_secret_key_id).expect("ids should always serialize with rmp");

        let chain_id = app.config.chain_id;

        let head_block_num = head_block_num.or_else(|| app.balanced_rpcs.head_block_num());

        // TODO: would be nice to have the block hash too

        // another item is added with the response, so initial_capacity is +1 what is needed here
        let kafka_headers = KafkaOwnedHeaders::new_with_capacity(6)
            .insert(KafkaHeader {
                key: "rpc_secret_key_id",
                value: authorization
                    .checks
                    .rpc_secret_key_id
                    .map(|x| x.to_string())
                    .as_ref(),
            })
            .insert(KafkaHeader {
                key: "ip",
                value: Some(&authorization.ip.to_string()),
            })
            .insert(KafkaHeader {
                key: "request_ulid",
                value: Some(&request_ulid.to_string()),
            })
            .insert(KafkaHeader {
                key: "head_block_num",
                value: head_block_num.map(|x| x.to_string()).as_ref(),
            })
            .insert(KafkaHeader {
                key: "chain_id",
                value: Some(&chain_id.to_le_bytes()),
            });

        // save the key and headers for when we log the response
        let x = Self {
            topic: kafka_topic,
            key: kafka_key,
            headers: kafka_headers,
            producer: kafka_producer,
            num_requests: 0.into(),
            num_responses: 0.into(),
        };

        let x = Arc::new(x);

        Some(x)
    }

    fn background_log(&self, payload: Vec<u8>) -> JoinHandle<KafkaLogResult> {
        let topic = self.topic.clone();
        let key = self.key.clone();
        let producer = self.producer.clone();
        let headers = self.headers.clone();

        let f = async move {
            let record = FutureRecord::to(&topic)
                .key(&key)
                .payload(&payload)
                .headers(headers);

            let produce_future =
                producer.send(record, KafkaTimeout::After(Duration::from_secs(5 * 60)));

            let kafka_response = produce_future.await;

            if let Err((err, msg)) = kafka_response.as_ref() {
                error!("produce kafka request: {} - {:?}", err, msg);
                // TODO: re-queue the msg? log somewhere else like a file on disk?
                // TODO: this is bad and should probably trigger an alarm
            };

            kafka_response
        };

        tokio::spawn(f)
    }

    /// for opt-in debug usage, log the request to kafka
    /// TODO: generic type for request
    pub fn log_debug_request(&self, request: &RequestOrMethod) -> JoinHandle<KafkaLogResult> {
        // TODO: is rust message pack a good choice? try rkyv instead
        let payload =
            rmp_serde::to_vec(&request).expect("requests should always serialize with rmp");

        self.num_requests.fetch_add(1, atomic::Ordering::Relaxed);

        self.background_log(payload)
    }

    pub fn log_debug_response<R>(&self, response: &R) -> JoinHandle<KafkaLogResult>
    where
        R: serde::Serialize,
    {
        let payload =
            rmp_serde::to_vec(&response).expect("requests should always serialize with rmp");

        self.num_responses.fetch_add(1, atomic::Ordering::Relaxed);

        self.background_log(payload)
    }
}
