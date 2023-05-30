use crate::app::Web3ProxyApp;
// use crate::frontend::errors::Web3ProxyError;
use ethers::providers::{JsonRpcClient, ProviderError};
use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Debug;
use std::sync::Arc;

/// use the app as an ether's JsonRpcClient
#[derive(Debug)]
struct Web3ProxyAppClient(Arc<Web3ProxyApp>);

#[async_trait::async_trait]
impl JsonRpcClient for Web3ProxyAppClient {
    type Error = ProviderError;

    async fn request<T, R>(&self, method: &str, params: T) -> Result<R, Self::Error>
    where
        T: Debug + Serialize + Send + Sync,
        R: DeserializeOwned + Send,
    {
        todo!("figure out traits");
        // match self.0.internal_request(method, &params).await {
        //     Ok(x) => Ok(x),
        //     Err(Web3ProxyError::EthersProvider(err)) => Err(err),
        //     Err(err) => Err(ProviderError::CustomError(format!("{}", err))),
        // }
    }
}
