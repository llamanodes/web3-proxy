// TODO: option to spawn in a dedicated thread?
// TODO: option to subscribe to another anvil and copy blocks

use ethers::{
    signers::LocalWallet,
    utils::{Anvil, AnvilInstance},
};
use tracing::info;
use web3_proxy::rpcs::provider::EthersHttpProvider;

/// on drop, the anvil instance will be shut down
pub struct TestAnvil {
    pub instance: AnvilInstance,
    pub provider: EthersHttpProvider,
}

impl TestAnvil {
    #[allow(unused)]
    pub async fn spawn(chain_id: u64) -> Self {
        info!(?chain_id);

        // TODO: configurable rpc and block
        let instance = Anvil::new()
            .chain_id(chain_id)
            // .fork("https://polygon.llamarpc.com@44300000")
            .spawn();

        let provider = EthersHttpProvider::try_from(instance.endpoint()).unwrap();

        Self { instance, provider }
    }

    #[allow(unused)]
    pub fn wallet(&self, id: usize) -> LocalWallet {
        self.instance.keys()[id].clone().into()
    }
}
