use serde_json::Value;
use std::{str::FromStr, time::Duration};
use tokio::{task::yield_now, time::sleep};
use tracing::{info, warn};
use web3_proxy::prelude::ethers::{
    prelude::{Block, Transaction, TxHash, H256, U256, U64},
    providers::{Http, JsonRpcClient, Quorum, QuorumProvider, WeightedProvider},
    types::{transaction::eip2718::TypedTransaction, Address, Bytes, Eip1559TransactionRequest},
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
    x.flush_stats_and_wait().await.unwrap();

    // drop x first to avoid spurious warnings about anvil/influx/mysql shutting down before the app
    drop(x);
}

#[test_log::test(tokio::test)]
async fn it_starts_and_stops() {
    let a = TestAnvil::spawn(31337).await;

    let x = TestApp::spawn(&a, None, None, None).await;

    let anvil_provider = &a.provider;
    let proxy_provider = &x.proxy_provider;

    // check the /health page
    let proxy_url = x.proxy_provider.url();
    let health_response = reqwest::get(format!("{}health", proxy_url)).await;
    dbg!(&health_response);
    assert_eq!(health_response.unwrap().status(), StatusCode::OK);

    // check the /status page
    let status_response = reqwest::get(format!("{}status", proxy_url)).await;
    dbg!(&status_response);
    assert_eq!(status_response.unwrap().status(), StatusCode::OK);

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

    yield_now().await;

    let mut proxy_result = None;

    for _ in 0..10 {
        proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap();

        if let Some(ref proxy_result) = proxy_result {
            if proxy_result.number == Some(second_block_num) {
                break;
            }
        }

        warn!(?proxy_result, ?second_block_num);

        sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(anvil_result, proxy_result.unwrap());

    // this won't do anything since stats aren't tracked when there isn't a db
    let flushed = x.flush_stats_and_wait().await.unwrap();
    assert_eq!(flushed.relational, 0);
    assert_eq!(flushed.timeseries, 0);

    // most tests won't need to wait, but we should wait here to be sure all the shutdown logic works properly
    x.wait_for_stop();
}

/// TODO: have another test that queries mainnet so the state is more interesting
/// TODO: have another test that makes sure error codes match
#[test_log::test(tokio::test)]
async fn it_matches_anvil() {
    let chain_id = 31337;

    let a = TestAnvil::spawn(chain_id).await;

    a.provider.request::<_, U64>("evm_mine", ()).await.unwrap();

    let x = TestApp::spawn(&a, None, None, None).await;

    let proxy_provider = Http::from_str(x.proxy_provider.url().as_str()).unwrap();

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
            (block_with_tx.as_ref().unwrap().author.unwrap(), "latest"),
        )
        .await
        .unwrap();
    info!(%balance);

    let singleton_deploy_from: Address = "0xBb6e024b9cFFACB947A71991E386681B1Cd1477D"
        .parse()
        .unwrap();

    let wallet = a.wallet(0);

    let x = quorum_provider
        .request::<_, Option<Transaction>>(
            "eth_getTransactionByHash",
            ["0x803351deb6d745e91545a6a3e1c0ea3e9a6a02a1a4193b70edfcd2f40f71a01c"],
        )
        .await
        .unwrap();
    assert!(x.is_none());

    let gas_price: U256 = quorum_provider.request("eth_gasPrice", ()).await.unwrap();

    let tx = TypedTransaction::Eip1559(Eip1559TransactionRequest {
        chain_id: Some(chain_id),
        to: Some(singleton_deploy_from.into()),
        gas: Some(21000.into()),
        value: Some("24700000000000000".parse().unwrap()),
        max_fee_per_gas: Some(gas_price * U256::from(2)),
        ..Default::default()
    });

    let sig = wallet.sign_transaction_sync(&tx).unwrap();

    let raw_tx = tx.rlp_signed(&sig);

    // fund singleton deployer
    // TODO: send through the quorum provider. it should detect that its already confirmed
    let fund_tx_hash: H256 = proxy_provider
        .request("eth_sendRawTransaction", [raw_tx])
        .await
        .unwrap();
    info!(%fund_tx_hash);

    // deploy singleton deployer
    // TODO: send through the quorum provider. it should detect that its already confirmed
    let deploy_tx: H256 = proxy_provider.request("eth_sendRawTransaction", ["0xf9016c8085174876e8008303c4d88080b90154608060405234801561001057600080fd5b50610134806100206000396000f3fe6080604052348015600f57600080fd5b506004361060285760003560e01c80634af63f0214602d575b600080fd5b60cf60048036036040811015604157600080fd5b810190602081018135640100000000811115605b57600080fd5b820183602082011115606c57600080fd5b80359060200191846001830284011164010000000083111715608d57600080fd5b91908080601f016020809104026020016040519081016040528093929190818152602001838380828437600092019190915250929550509135925060eb915050565b604080516001600160a01b039092168252519081900360200190f35b6000818351602085016000f5939250505056fea26469706673582212206b44f8a82cb6b156bfcc3dc6aadd6df4eefd204bc928a4397fd15dacf6d5320564736f6c634300060200331b83247000822470"]).await.unwrap();
    assert_eq!(
        deploy_tx,
        "0x803351deb6d745e91545a6a3e1c0ea3e9a6a02a1a4193b70edfcd2f40f71a01c"
            .parse()
            .unwrap()
    );

    let code: Bytes = quorum_provider
        .request(
            "eth_getCode",
            ("0xce0042B868300000d44A59004Da54A005ffdcf9f", "latest"),
        )
        .await
        .unwrap();
    info!(%code);

    let deploy_tx = quorum_provider
        .request::<_, Option<Transaction>>(
            "eth_getTransactionByHash",
            ["0x803351deb6d745e91545a6a3e1c0ea3e9a6a02a1a4193b70edfcd2f40f71a01c"],
        )
        .await
        .unwrap()
        .unwrap();
    info!(?deploy_tx);

    let head_block_num: U64 = quorum_provider
        .request("eth_blockNumber", ())
        .await
        .unwrap();

    let future_block: Option<ArcBlock> = quorum_provider
        .request("eth_getBlockByNumber", (head_block_num + U64::one(), false))
        .await
        .unwrap();
    assert!(future_block.is_none());

    // todo!("lots more requests");

    // todo!("compare batch requests");
}
