use super::{many::Web3Rpcs, one::Web3Rpc};
use ethers::providers::{JsonRpcClient, ProviderError};
use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Debug;
