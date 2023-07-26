mod common;

use crate::common::influx::TestInflux;
use moka::future::Cache;
use tokio::sync::{broadcast, mpsc};
use web3_proxy::{caches::UserBalanceCache, stats::StatBuffer};

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_two_buffers() {
    let i = TestInflux::spawn().await;

    let billing_period_seconds = 86400 * 7;
    let chain_id = 999_001_999;
    let db_save_interval_seconds = 60;
    let influxdb_bucket = Some(i.bucket.clone());
    let influxdb_client = Some(i.client.clone());
    let rpc_secret_key_cache = Cache::builder().build();
    let tsdb_save_interval_seconds = 30;
    let user_balance_cache: UserBalanceCache = Cache::builder().build().into();

    let (shutdown_sender, shutdown_receiver_1) = broadcast::channel(1);
    let shutdown_receiver_2 = shutdown_sender.subscribe();

    let (flush_sender_1, flush_receiver_1) = mpsc::channel(1);
    let (flush_sender_2, flush_receiver_2) = mpsc::channel(1);

    let buffer_1 = StatBuffer::try_spawn(
        billing_period_seconds,
        chain_id,
        db_save_interval_seconds,
        influxdb_bucket.clone(),
        influxdb_client.clone(),
        rpc_secret_key_cache.clone(),
        user_balance_cache.clone(),
        shutdown_receiver_1,
        tsdb_save_interval_seconds,
        flush_sender_1,
        flush_receiver_1,
        1,
    )
    .unwrap()
    .unwrap();

    let buffer_2 = StatBuffer::try_spawn(
        billing_period_seconds,
        chain_id,
        db_save_interval_seconds,
        influxdb_bucket,
        influxdb_client,
        rpc_secret_key_cache,
        user_balance_cache,
        shutdown_receiver_2,
        tsdb_save_interval_seconds,
        flush_sender_2,
        flush_receiver_2,
        2,
    )
    .unwrap()
    .unwrap();

    // TODO: send things to the buffers

    shutdown_sender.send(()).unwrap();

    buffer_1.background_handle.await.unwrap().unwrap();
    buffer_2.background_handle.await.unwrap().unwrap();
}
