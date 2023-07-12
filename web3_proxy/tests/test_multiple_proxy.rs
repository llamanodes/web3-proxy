mod common;

use crate::common::{anvil::TestAnvil, mysql::TestMysql, TestApp};
use web3_proxy::rpcs::blockchain::ArcBlock;

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_multiple_proxies_stats_add_up() {
    let a = TestAnvil::spawn(31337).await;

    let db = TestMysql::spawn().await;

    let x_1 = TestApp::spawn(&a, Some(&db)).await;
    let x_2 = TestApp::spawn(&a, Some(&db)).await;

    // test the basics
    let anvil_provider = &a.provider;
    let proxy_provider_1 = &x_1.proxy_provider;
    let proxy_provider_2 = &x_2.proxy_provider;

    let anvil_result = anvil_provider
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();
    let proxy_1_result = proxy_provider_1
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();
    let proxy_2_result = proxy_provider_2
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(anvil_result, proxy_1_result);
    assert_eq!(anvil_result, proxy_2_result);

    todo!()
}
