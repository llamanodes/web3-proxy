// TODO: option to spawn in a dedicated thread?
// TODO: option to subscribe to another anvil and copy blocks

use crate::rpcs::provider::EthersHttpProvider;
use ethers::{
    signers::LocalWallet,
    utils::{Anvil, AnvilInstance},
};
use tracing::info;

/// on drop, the anvil instance will be shut down
pub struct TestAnvil {
    pub instance: AnvilInstance,
    pub provider: EthersHttpProvider,
}

impl TestAnvil {
    pub async fn new(chain_id: Option<u64>, fork_rpc: Option<&str>) -> Self {
        info!(?chain_id);

        let mut instance = Anvil::new();

        if let Some(chain_id) = chain_id {
            instance = instance.chain_id(chain_id);
        }

        if let Some(fork_rpc) = fork_rpc {
            instance = instance.fork(fork_rpc);
        }

        let instance = instance.spawn();

        let provider = EthersHttpProvider::try_from(instance.endpoint()).unwrap();

        Self { instance, provider }
    }

    pub async fn spawn(chain_id: u64) -> Self {
        Self::new(Some(chain_id), None).await
    }

    pub async fn spawn_fork(fork_rpc: &str) -> Self {
        Self::new(None, Some(fork_rpc)).await
    }

    pub fn wallet(&self, id: usize) -> LocalWallet {
        self.instance.keys()[id].clone().into()
    }
}
