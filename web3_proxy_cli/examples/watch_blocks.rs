/// poll an http rpc
use std::{str::FromStr, time::Duration};
use web3_proxy::prelude::anyhow;
use web3_proxy::prelude::ethers::prelude::*;
use web3_proxy::prelude::fdlimit;
use web3_proxy::prelude::tokio;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fdlimit::raise_fd_limit();

    // erigon does not support most filters
    // let url = "http://10.11.12.16:8545";
    // geth
    let url = "http://10.11.12.16:8545";

    println!("Watching blocks from {:?}", url);

    let provider = Http::from_str(url)?;

    let provider = Provider::new(provider).interval(Duration::from_secs(1));

    let mut stream = provider.watch_blocks().await?;
    while let Some(block_number) = stream.next().await {
        let block = provider.get_block(block_number).await?.unwrap();
        println!(
            "{:?} = Ts: {:?}, block number: {}",
            block.hash.unwrap(),
            block.timestamp,
            block.number.unwrap(),
        );
    }

    Ok(())
}
