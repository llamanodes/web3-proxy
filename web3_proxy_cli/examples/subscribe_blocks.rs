/// subscribe to a websocket rpc
use std::time::Duration;
use web3_proxy::prelude::anyhow;
use web3_proxy::prelude::ethers::prelude::*;
use web3_proxy::prelude::fdlimit;
use web3_proxy::prelude::tokio;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // install global collector configured based on RUST_LOG env var.
    fdlimit::raise_fd_limit();

    // erigon
    let url = "ws://10.11.12.16:8548";
    // geth
    // let url = "ws://10.11.12.16:8546";

    println!("Subscribing to blocks from {}", url);

    let provider = Ws::connect(url).await?;

    let provider = Provider::new(provider).interval(Duration::from_secs(1));

    let mut stream = provider.subscribe_blocks().await?;
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
