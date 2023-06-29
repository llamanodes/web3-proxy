mod common;

use crate::common::TestApp;
use ethers::prelude::U256;
use std::time::Duration;
use tokio::time::{sleep, Instant};
use web3_proxy::rpcs::blockchain::ArcBlock;

#[test_log::test(tokio::test)]
async fn it_starts_and_stops() {
    let x = TestApp::spawn().await;

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
            if proxy_result.number != Some(first_block_num) {
                break;
            }
        }

        sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(anvil_result, proxy_result.unwrap());

    // most tests won't need to wait, but we should wait here to be sure all the shutdown logic works properly
    x.wait().await;
}
