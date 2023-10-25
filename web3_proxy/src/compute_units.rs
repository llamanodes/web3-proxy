//! Compute Units based on median request latencies and sizes.
//! Designed to match Alchemy's system.
//! I'm sure there will be memes about us copying, but the user experience of consistency makes a lot of sense to me.
//! TODO? pricing based on latency and bytes and
//! TODO: rate limit on compute units
//! TODO: pricing on compute units
//! TODO: script that queries influx and calculates observed relative costs

use migration::sea_orm::prelude::Decimal;
use std::{ops::Add, str::FromStr};
use tracing::{trace, warn};

/// TODO: i don't like how we use this inside the config and also have it available publicly. we should only getting this value from the config
pub fn default_usd_per_cu(chain_id: u64) -> Decimal {
    match chain_id {
        999_001_999 => Decimal::from_str("0.10").unwrap(),
        1 | 31337 => Decimal::from_str("0.000000400000000000000").unwrap(),
        _ => Decimal::from_str("0.000000533333333333333").unwrap(),
    }
}

pub fn default_cu_per_byte(_chain_id: u64, method: &str) -> Decimal {
    if method.starts_with("debug_") {
        return Decimal::new(15245, 6);
    }

    match method {
        "eth_subscribe(newPendingTransactions)" => Decimal::new(16, 2),
        _ => Decimal::new(4, 2),
    }
}

#[derive(Debug)]
pub struct ComputeUnit(Decimal);

impl<T> Add<T> for ComputeUnit
where
    T: Into<Decimal>,
{
    type Output = Self;

    fn add(self, rhs: T) -> Self::Output {
        Self(self.0 + rhs.into())
    }
}

impl ComputeUnit {
    /// costs can vary widely depending on method and chain
    pub fn new(method: &str, chain_id: u64, response_bytes: u64) -> Self {
        let cu = match (chain_id, method) {
            (1101, "zkevm_batchNumber") => 0,
            (1101, "zkevm_batchNumberByBlockNumber") => 0,
            (1101, "zkevm_consolidatedBlockNumber") => 0,
            (1101, "zkevm_getBatchByNumber") => 0,
            (1101, "zkevm_getBroadcastURI") => 0,
            (1101, "zkevm_isBlockConsolidated") => 0,
            (1101, "zkevm_isBlockVirtualized") => 0,
            (1101, "zkevm_verifiedBatchNumber") => 0,
            (1101, "zkevm_virtualBatchNumber") => 0,
            (137, "bor_getAuthor") => 10,
            (137, "bor_getCurrentProposer") => 10,
            (137, "bor_getCurrentValidators") => 10,
            (137, "bor_getRootHash") => 10,
            (137, "bor_getSignersAtHash") => 10,
            (_, "debug_traceBlockByHash") => {
                return Self::variable_price(chain_id, method, response_bytes) + 497
            }
            (_, "debug_traceBlockByNumber") => {
                return Self::variable_price(chain_id, method, response_bytes) + 497
            }
            (_, "debug_traceCall") => {
                return Self::variable_price(chain_id, method, response_bytes) + 309
            }
            (_, "debug_traceTransaction") => {
                return Self::variable_price(chain_id, method, response_bytes) + 309
            }
            (_, "erigon_forks") => 24,
            (_, "erigon_getHeaderByHash") => 24,
            (_, "erigon_getHeaderByNumber") => 24,
            (_, "erigon_getLogsByHash") => 24,
            (_, "erigon_issuance") => 24,
            (_, "eth_accounts") => 10,
            (_, "eth_blockNumber") => 10,
            (_, "eth_call") => 26,
            (_, "eth_chainId") => 0,
            (_, "eth_createAccessList") => 10,
            (_, "eth_estimateGas") => 87,
            (_, "eth_estimateUserOperationGas") => 500,
            (_, "eth_feeHistory") => 10,
            (_, "eth_gasPrice") => 19,
            (_, "eth_getBalance") => 19,
            (_, "eth_getBlockByHash") => 21,
            (_, "eth_getBlockByNumber") => 16,
            (_, "eth_getBlockReceipts") => 500,
            (_, "eth_getBlockTransactionCountByHash") => 20,
            (_, "eth_getBlockTransactionCountByNumber") => 20,
            (_, "eth_getCode") => 19,
            (_, "eth_getFilterChanges") => 20,
            (_, "eth_getFilterLogs") => 75,
            (_, "eth_getLogs") => 75,
            (_, "eth_getProof") => 21,
            (_, "eth_getStorageAt") => 17,
            (_, "eth_getTransactionByBlockHashAndIndex") => 15,
            (_, "eth_getTransactionByBlockNumberAndIndex") => 15,
            (_, "eth_getTransactionByHash") => 17,
            (_, "eth_getTransactionCount") => 26,
            (_, "eth_getTransactionReceipt") => 15,
            (_, "eth_getUncleByBlockHashAndIndex") => 15,
            (_, "eth_getUncleByBlockNumberAndIndex") => 15,
            (_, "eth_getUncleCountByBlockHash") => 15,
            (_, "eth_getUncleCountByBlockNumber") => 15,
            (_, "eth_getUserOperationByHash") => 17,
            (_, "eth_getUserOperationReceipt") => 15,
            (_, "eth_maxPriorityFeePerGas") => 10,
            (_, "eth_newBlockFilter") => {
                // TODO: 20
                return Self::unimplemented();
            }
            (_, "eth_newFilter") => {
                // TODO: 20
                return Self::unimplemented();
            }
            (_, "eth_newPendingTransactionFilter") => {
                // TODO: 20
                return Self::unimplemented();
            }
            (_, "eth_pollSubscriptions") => {
                return Self::unimplemented();
            }
            (_, "eth_protocolVersion") => 0,
            (_, "eth_sendRawTransaction") => 250,
            (_, "eth_sendUserOperation") => 1000,
            (_, "eth_subscribe") => 10,
            (_, "eth_supportedEntryPoints") => 5,
            (_, "eth_syncing") => 0,
            (_, "eth_uninstallFilter") => 10,
            (_, "eth_unsubscribe") => 10,
            (_, "net_listening") => 0,
            (_, "net_version") => 0,
            (_, "test") => 0,
            (_, "trace_block") => 24,
            (_, "trace_call") => 75,
            (_, "trace_callMany") => 75 * 3,
            (_, "trace_filter") => 75,
            (_, "trace_get") => 17,
            (_, "trace_rawTransaction") => 75,
            (_, "trace_replayBlockTransactions") => 2983,
            (_, "trace_replayTransaction") => 2983,
            (_, "trace_transaction") => 26,
            (_, "txpool_content") => {
                return Self::variable_price(chain_id, method, response_bytes) + 1000;
            }
            (_, "invalid_method") => 100,
            (_, "web3_clientVersion") => 15,
            (_, "web3_bundlerVersion") => 15,
            (_, "web3_sha3") => 15,
            (_, "ots_getInternalOperations") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_hasCode") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_getTransactionError") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_traceTransaction") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_getBlockDetails") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_getBlockDetailsByHash") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_getBlockTransactions") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_searchTransactionsBefore") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_searchTransactionsAfter") => {
                return Self::variable_price(chain_id, method, response_bytes) + 100
            }
            (_, "ots_getTransactionBySenderAndNonce") => 1000,
            (_, "ots_getContractCreator") => 1000,
            (_, method) => {
                // TODO: this works, but this is fragile. think of a better way to check the method is a subscription
                if method.ends_with(')') {
                    return Self::variable_price(chain_id, method, response_bytes);
                }

                if method.starts_with("admin_")
                    || method.starts_with("miner_")
                    || method == "personal_unlockAccount"
                {
                    // charge extra since they are doing things they aren't supposed to
                    return Self::unimplemented() * 10;
                }

                if method.starts_with("alchemy_")
                    || method.starts_with("personal_")
                    || method.starts_with("shh_")
                    || method.starts_with("db_")
                {
                    // maybe charge extra since they are doing things they aren't supposed to
                    return Self::unimplemented();

                    warn!(%response_bytes, "unknown method {}", method);
                    return Self::unimplemented()
                        + Self::variable_price(chain_id, method, response_bytes).0;
                }
            }
        };

        let cu = Decimal::from(cu);

        trace!(%cu);

        Self(cu)
    }

    /// notifications and subscription responses cost per-byte
    pub fn variable_price<D: Into<Decimal> + std::fmt::Debug>(
        chain_id: u64,
        method: &str,
        num_bytes: D,
    ) -> Self {
        let cu = num_bytes.into() * default_cu_per_byte(chain_id, method);

        Self(cu)
    }

    /// requesting an unimplemented function costs 2 CU
    pub fn unimplemented() -> Self {
        Self(2.into())
    }

    /// Compute cost per request
    /// All methods cost the same
    /// The number of bytes are based on input, and output bytes
    pub fn cost(
        &self,
        archive_request: bool,
        cache_hit: bool,
        error_response: bool,
        usd_per_cu: &Decimal,
    ) -> Decimal {
        if error_response {
            trace!("error responses are free");
            return 0.into();
        }

        let mut cost = self.0 * usd_per_cu;

        trace!(%cost, "base");

        if archive_request {
            // TODO: get from config
            cost *= Decimal::from_str("2.5").unwrap();

            trace!(%cost, "archive_request");
        }

        if cache_hit {
            // cache hits get a 25% discount
            // TODO: get from config
            cost *= Decimal::from_str("0.75").unwrap();

            trace!(%cost, "cache_hit");
        }

        trace!(%cost, "final");

        cost
    }
}
