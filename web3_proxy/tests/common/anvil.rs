// TODO: option to spawn in a dedicated thread?
// TODO: option to subscribe to another anvil and copy blocks

use ethers::utils::{Anvil, AnvilInstance};
use tracing::info;

/// on drop, the anvil instance will be shut down
pub struct TestAnvil {
    pub instance: AnvilInstance,
}

impl TestAnvil {
    pub async fn spawn(chain_id: u64) -> Self {
        info!(?chain_id);

        // TODO: configurable rpc and block
        let instance = Anvil::new()
            .chain_id(chain_id)
            // .fork("https://polygon.llamarpc.com@44300000")
            .spawn();

        Self { instance }
    }
}
