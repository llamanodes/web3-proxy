mod common;

use crate::common::TestApp;
use ethers::prelude::U256;
use http::StatusCode;
use std::time::Duration;
use tokio::{
    task::yield_now,
    time::{sleep, Instant},
};
use web3_proxy::rpcs::blockchain::ArcBlock;

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn it_migrates_the_db() {
    let x = TestApp::spawn(31337, true).await;

    // we call flush stats more to be sure it works than because we expect it to save any stats
    x.flush_stats().await.unwrap();
}

#[test_log::test(tokio::test)]
async fn it_starts_and_stops() {
    let x = TestApp::spawn(31337, false).await;

    let anvil_provider = &x.anvil_provider;
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
    x.flush_stats().await.unwrap_err();

    // most tests won't need to wait, but we should wait here to be sure all the shutdown logic works properly
    x.wait().await;
}
