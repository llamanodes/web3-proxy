use ethers::prelude::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // let ws = Ws::connect("ws://10.11.12.16:8545").await?;
    let ws = Ws::connect("ws://10.11.12.16:8946").await?;
    let provider = Provider::new(ws).interval(Duration::from_secs(1));
    let mut stream = provider.watch_blocks().await?.take(5);
    while let Some(block_number) = stream.next().await {
        let block = provider.get_block(block_number).await?.unwrap();
        println!(
            "Ts: {:?}, block number: {} -> {:?}",
            block.timestamp,
            block.number.unwrap(),
            block.hash.unwrap()
        );
    }

    Ok(())
}
