//! Load balanced communication with a group of web3 providers
use super::many::Web3Rpcs;
use super::one::Web3Rpc;
use super::request::OpenRequestResult;
use crate::errors::Web3ProxyResult;
use crate::frontend::authorization::Authorization;
use ethers::prelude::{ProviderError, Transaction, TxHash};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, trace, Level};

// TODO: think more about TxState
#[derive(Clone)]
pub enum TxStatus {
    Pending(Transaction),
    Confirmed(Transaction),
    Orphaned(Transaction),
}

impl Web3Rpcs {
    async fn query_transaction_status(
        &self,
        authorization: &Arc<Authorization>,
        rpc: Arc<Web3Rpc>,
        pending_tx_id: TxHash,
    ) -> Result<Option<TxStatus>, ProviderError> {
        // TODO: there is a race here on geth. sometimes the rpc isn't yet ready to serve the transaction (even though they told us about it!)
        // TODO: might not be a race. might be a nonce thats higher than the current account nonce. geth discards chains
        // TODO: yearn devs have had better luck with batching these, but i think that's likely just adding a delay itself
        // TODO: if one rpc fails, try another?
        // TODO: try_request_handle, or wait_for_request_handle? I think we want wait here
        let tx: Transaction = match rpc
            .try_request_handle(authorization, Some(Level::WARN.into()))
            .await
        {
            Ok(OpenRequestResult::Handle(handle)) => {
                handle
                    .request("eth_getTransactionByHash", &(pending_tx_id,))
                    .await?
            }
            Ok(_) => {
                // TODO: actually retry?
                return Ok(None);
            }
            Err(err) => {
                trace!(
                    "cancelled funneling transaction {} from {}: {:?}",
                    pending_tx_id,
                    rpc,
                    err,
                );
                return Ok(None);
            }
        };

        match &tx.block_hash {
            Some(_block_hash) => {
                // the transaction is already confirmed. no need to save in the pending_transactions map
                Ok(Some(TxStatus::Confirmed(tx)))
            }
            None => Ok(Some(TxStatus::Pending(tx))),
        }
    }

    /// dedupe transaction and send them to any listening clients
    pub(super) async fn process_incoming_tx_id(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        rpc: Arc<Web3Rpc>,
        pending_tx_id: TxHash,
        pending_tx_sender: broadcast::Sender<TxStatus>,
    ) -> Web3ProxyResult<()> {
        // TODO: how many retries? until some timestamp is hit is probably better. maybe just loop and call this with a timeout
        // TODO: after more investigation, i don't think retries will help. i think this is because chains of transactions get dropped from memory
        // TODO: also check the "confirmed transactions" mapping? maybe one shared mapping with TxState in it?

        if pending_tx_sender.receiver_count() == 0 {
            // no receivers, so no point in querying to get the full transaction
            return Ok(());
        }

        // trace!(?pending_tx_id, "checking pending_transactions on {}", rpc);
        if self.pending_transaction_cache.get(&pending_tx_id).is_some() {
            // this transaction has already been processed
            return Ok(());
        }

        // query the rpc for this transaction
        // it is possible that another rpc is also being queried. thats fine. we want the fastest response
        match self
            .query_transaction_status(&authorization, rpc.clone(), pending_tx_id)
            .await
        {
            Ok(Some(tx_state)) => {
                let _ = pending_tx_sender.send(tx_state);

                trace!("sent tx {:?}", pending_tx_id);

                // we sent the transaction. return now. don't break looping because that gives a warning
                return Ok(());
            }
            Ok(None) => {}
            Err(err) => {
                trace!("failed fetching transaction {:?}: {:?}", pending_tx_id, err);
                // unable to update the entry. sleep and try again soon
                // TODO: retry with exponential backoff with jitter starting from a much smaller time
                // sleep(Duration::from_millis(100)).await;
            }
        }

        // warn is too loud. this is somewhat common
        // "There is a Pending txn with a lower account nonce. This txn can only be executed after confirmation of the earlier Txn Hash#"
        // sometimes it's been pending for many hours
        // sometimes it's maybe something else?
        debug!("txid {} not found on {}", pending_tx_id, rpc);
        Ok(())
    }
}
