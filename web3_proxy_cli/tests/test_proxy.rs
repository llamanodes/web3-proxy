use serde_json::Value;
use std::{str::FromStr, time::Duration};
use tokio::{
    task::yield_now,
    time::{sleep, Instant},
};
use tracing::info;
use web3_proxy::prelude::ethers::{
    prelude::{Block, Transaction, TxHash, U256, U64},
    providers::{Http, JsonRpcClient, Quorum, QuorumProvider, WeightedProvider},
};
use web3_proxy::prelude::http::StatusCode;
use web3_proxy::prelude::reqwest;
use web3_proxy::rpcs::blockchain::ArcBlock;
use web3_proxy_cli::test_utils::{TestAnvil, TestApp, TestMysql};

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn it_migrates_the_db() {
    let a = TestAnvil::spawn(31337).await;
    let db = TestMysql::spawn().await;

    let x = TestApp::spawn(&a, Some(&db), None, None).await;

    // we call flush stats more to be sure it works than because we expect it to save any stats
    x.flush_stats().await.unwrap();

    // drop x first to avoid spurious warnings about anvil/influx/mysql shutting down before the app
    drop(x);
}

#[test_log::test(tokio::test)]
async fn it_starts_and_stops() {
    let a = TestAnvil::spawn(31337).await;

    let x = TestApp::spawn(&a, None, None, None).await;

    let anvil_provider = &a.provider;
    let proxy_provider = &x.proxy_provider;

    let anvil_result = anvil_provider
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();
    let proxy_result = proxy_provider
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(anvil_result, proxy_result);

    // check the /health page
    let proxy_url = x.proxy_provider.url();
    let health_response = reqwest::get(format!("{}health", proxy_url)).await;
    dbg!(&health_response);
    assert_eq!(health_response.unwrap().status(), StatusCode::OK);

    // check the /status page
    let status_response = reqwest::get(format!("{}status", proxy_url)).await;
    dbg!(&status_response);
    assert_eq!(status_response.unwrap().status(), StatusCode::OK);

    let first_block_num = anvil_result.number.unwrap();

    // mine a block
    let _: U256 = anvil_provider.request("evm_mine", ()).await.unwrap();

    // make sure the block advanced
    let anvil_result = anvil_provider
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();

    let second_block_num = anvil_result.number.unwrap();

    assert_eq!(first_block_num, second_block_num - 1);

    yield_now().await;

    let mut proxy_result;
    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(1) {
            panic!("took too long to sync!");
        }

        proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap();

        if let Some(ref proxy_result) = proxy_result {
            if proxy_result.number == Some(second_block_num) {
                break;
            }
        }

        sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(anvil_result, proxy_result.unwrap());

    // this won't do anything since stats aren't tracked when there isn't a db
    let flushed = x.flush_stats().await.unwrap();
    assert_eq!(flushed.relational, 0);
    assert_eq!(flushed.timeseries, 0);

    // most tests won't need to wait, but we should wait here to be sure all the shutdown logic works properly
    x.wait_for_stop();
}

/// TODO: have another test that queries mainnet so the state is more interesting
/// TODO: have another test that makes sure error codes match
#[test_log::test(tokio::test)]
async fn it_matches_anvil() {
    let a = TestAnvil::spawn(31337).await;

    // TODO: send some test transactions

    a.provider.request::<_, U64>("evm_mine", ()).await.unwrap();

    let x = TestApp::spawn(&a, None, None, None).await;

    let weighted_anvil_provider =
        WeightedProvider::new(Http::from_str(&a.instance.endpoint()).unwrap());
    let weighted_proxy_provider =
        WeightedProvider::new(Http::from_str(x.proxy_provider.url().as_str()).unwrap());

    let quorum_provider = QuorumProvider::builder()
        .add_providers([weighted_anvil_provider, weighted_proxy_provider])
        .quorum(Quorum::All)
        .build();

    let chain_id: U64 = quorum_provider.request("eth_chainId", ()).await.unwrap();
    info!(%chain_id);

    let block_number: U64 = quorum_provider
        .request("eth_blockNumber", ())
        .await
        .unwrap();
    info!(%block_number);

    let block_without_tx: Option<Block<TxHash>> = quorum_provider
        .request("eth_getBlockByNumber", (block_number, false))
        .await
        .unwrap();
    info!(?block_without_tx);

    let block_with_tx: Option<Block<Transaction>> = quorum_provider
        .request("eth_getBlockByNumber", (block_number, true))
        .await
        .unwrap();
    info!(?block_with_tx);

    let fee_history: Value = quorum_provider
        .request("eth_feeHistory", (4, "latest", [25, 75]))
        .await
        .unwrap();
    info!(?fee_history);

    let gas_price: U256 = quorum_provider.request("eth_gasPrice", ()).await.unwrap();
    info!(%gas_price);

    let balance: U256 = quorum_provider
        .request(
            "eth_getBalance",
            (block_with_tx.unwrap().author.unwrap(), "latest"),
        )
        .await
        .unwrap();
    info!(%balance);

    // todo!("lots more requests");

    // todo!("compare batch requests");
}
