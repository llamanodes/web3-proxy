/// subscribe to a websocket rpc
use ethers::prelude::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // erigon
    let url = "ws://10.11.12.16:8545";
    // geth
    // let url = "ws://10.11.12.16:8946";

    println!("Subscribing to blocks from {}", url);

    let provider = Ws::connect(url).await?;

    let provider = Provider::new(provider).interval(Duration::from_secs(1));

    let mut stream = provider.subscribe_blocks().await?.take(3);
    while let Some(block) = stream.next().await {
        println!(
            "{:?} = Ts: {:?}, block number: {}",
            block.hash.unwrap(),
            block.timestamp,
            block.number.unwrap(),
        );
    }

    Ok(())
}
