mod block_watcher;
mod provider;
mod provider_tiers;

use futures::future;
use governor::clock::{Clock, QuantaClock};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio::time::sleep;
use tracing::{info, warn};
use warp::Filter;

// use crate::types::{BlockMap, ConnectionsMap, RpcRateLimiterMap};
use crate::block_watcher::BlockWatcher;
use crate::provider_tiers::{Web3ConnectionMap, Web3ProviderTier};

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

/// The application
struct Web3ProxyApp {
    block_watcher: Arc<BlockWatcher>,
    /// clock used for rate limiting
    /// TODO: use tokio's clock (will require a different ratelimiting crate)
    clock: QuantaClock,
    /// Send requests to the best server available
    balanced_rpc_tiers: Arc<Vec<Web3ProviderTier>>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    private_rpcs: Option<Arc<Web3ProviderTier>>,
    /// write lock on these when all rate limits are hit
    balanced_rpc_ratelimiter_lock: RwLock<()>,
    private_rpcs_ratelimiter_lock: RwLock<()>,
}

impl Web3ProxyApp {
    async fn try_new(
        balanced_rpc_tiers: Vec<Vec<(&str, u32)>>,
        private_rpcs: Vec<(&str, u32)>,
    ) -> anyhow::Result<Web3ProxyApp> {
        let clock = QuantaClock::default();

        // make a http shared client
        // TODO: how should we configure the connection pool?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(300))
            .user_agent(APP_USER_AGENT)
            .build()?;

        let block_watcher = Arc::new(BlockWatcher::new());

        let block_watcher_clone = Arc::clone(&block_watcher);

        // start the block_watcher
        tokio::spawn(async move { block_watcher_clone.run().await });

        let balanced_rpc_tiers = Arc::new(
            future::join_all(balanced_rpc_tiers.into_iter().map(|balanced_rpc_tier| {
                Web3ProviderTier::try_new(
                    balanced_rpc_tier,
                    Some(http_client.clone()),
                    block_watcher.clone(),
                    &clock,
                )
            }))
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<Web3ProviderTier>>>()?,
        );

        let private_rpcs = if private_rpcs.is_empty() {
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            None
        } else {
            Some(Arc::new(
                Web3ProviderTier::try_new(
                    private_rpcs,
                    Some(http_client),
                    block_watcher.clone(),
                    &clock,
                )
                .await?,
            ))
        };

        Ok(Web3ProxyApp {
            block_watcher,
            clock,
            balanced_rpc_tiers,
            private_rpcs,
            balanced_rpc_ratelimiter_lock: Default::default(),
            private_rpcs_ratelimiter_lock: Default::default(),
        })
    }

    /// send the request to the approriate RPCs
    /// TODO: dry this up
    async fn proxy_web3_rpc(
        self: Arc<Web3ProxyApp>,
        json_body: serde_json::Value,
    ) -> anyhow::Result<impl warp::Reply> {
        let eth_send_raw_transaction =
            serde_json::Value::String("eth_sendRawTransaction".to_string());

        if self.private_rpcs.is_some() && json_body.get("method") == Some(&eth_send_raw_transaction)
        {
            let private_rpcs = self.private_rpcs.clone().unwrap();

            // there are private rpcs configured and the request is eth_sendSignedTransaction. send to all private rpcs
            loop {
                let read_lock = self.private_rpcs_ratelimiter_lock.read().await;

                let json_body_clone = json_body.clone();

                match private_rpcs
                    .get_upstream_servers(1, self.block_watcher.clone())
                    .await
                {
                    Ok(upstream_servers) => {
                        let (tx, mut rx) =
                            mpsc::unbounded_channel::<anyhow::Result<serde_json::Value>>();

                        let clone = self.clone();
                        let connections = private_rpcs.clone_connections();

                        // check incoming_id before sending any requests
                        let incoming_id = json_body.as_object().unwrap().get("id").unwrap();

                        tokio::spawn(async move {
                            clone
                                .try_send_requests(
                                    upstream_servers,
                                    connections,
                                    json_body_clone,
                                    tx,
                                )
                                .await
                        });

                        let response = rx
                            .recv()
                            .await
                            .ok_or_else(|| anyhow::anyhow!("no successful response"))?;

                        if let Ok(partial_response) = response {
                            let response = json!({
                                "jsonrpc": "2.0",
                                "id": incoming_id,
                                "result": partial_response
                            });
                            return Ok(warp::reply::json(&response));
                        }
                    }
                    Err(not_until) => {
                        // TODO: move this to a helper function
                        // sleep (with a lock) until our rate limits should be available
                        drop(read_lock);

                        let write_lock = self.balanced_rpc_ratelimiter_lock.write().await;

                        let deadline = not_until.wait_time_from(self.clock.now());
                        sleep(deadline).await;

                        drop(write_lock);
                    }
                };
            }
        } else {
            // this is not a private transaction (or no private relays are configured)
            // try to send to each tier, stopping at the first success
            loop {
                let read_lock = self.balanced_rpc_ratelimiter_lock.read().await;

                // there are multiple tiers. save the earliest not_until (if any). if we don't return, we will sleep until then and then try again
                let mut earliest_not_until = None;

                // check incoming_id before sending any requests
                let incoming_id = json_body.as_object().unwrap().get("id").unwrap();

                for balanced_rpcs in self.balanced_rpc_tiers.iter() {
                    match balanced_rpcs
                        .next_upstream_server(1, self.block_watcher.clone())
                        .await
                    {
                        Ok(upstream_server) => {
                            // TODO: better type for this. right now its request (the full jsonrpc object), response (just the inner result)
                            let (tx, mut rx) =
                                mpsc::unbounded_channel::<anyhow::Result<serde_json::Value>>();

                            {
                                // clone things so we can move them into the future and still use them here
                                let clone = self.clone();
                                let connections = balanced_rpcs.clone_connections();
                                let json_body = json_body.clone();
                                let upstream_server = upstream_server.clone();

                                tokio::spawn(async move {
                                    clone
                                        .try_send_requests(
                                            vec![upstream_server],
                                            connections,
                                            json_body,
                                            tx,
                                        )
                                        .await
                                });
                            }

                            let response = rx
                                .recv()
                                .await
                                .ok_or_else(|| anyhow::anyhow!("no successful response"))?;

                            if let Ok(partial_response) = response {
                                info!("forwarding request from {}", upstream_server);

                                let response = json!({
                                    "jsonrpc": "2.0",
                                    "id": incoming_id,
                                    "result": partial_response
                                });
                                return Ok(warp::reply::json(&response));
                            }
                        }
                        Err(not_until) => {
                            // save the smallest not_until. if nothing succeeds, return an Err with not_until in it
                            if earliest_not_until.is_none() {
                                earliest_not_until = Some(not_until);
                            } else {
                                let earliest_possible =
                                    earliest_not_until.as_ref().unwrap().earliest_possible();
                                let new_earliest_possible = not_until.earliest_possible();

                                if earliest_possible > new_earliest_possible {
                                    earliest_not_until = Some(not_until);
                                }
                            }
                        }
                    }
                }

                // we haven't returned an Ok, sleep and try again
                // TODO: move this to a helper function
                drop(read_lock);
                let write_lock = self.balanced_rpc_ratelimiter_lock.write().await;

                // unwrap should be safe since we would have returned if it wasn't set
                let deadline = if let Some(earliest_not_until) = earliest_not_until {
                    earliest_not_until.wait_time_from(self.clock.now())
                } else {
                    // TODO: exponential backoff?
                    Duration::from_secs(1)
                };

                sleep(deadline).await;

                drop(write_lock);
            }
        }
    }

    async fn try_send_requests(
        &self,
        rpc_servers: Vec<String>,
        connections: Arc<Web3ConnectionMap>,
        json_request_body: serde_json::Value,
        // TODO: better type for this
        tx: mpsc::UnboundedSender<anyhow::Result<serde_json::Value>>,
    ) -> anyhow::Result<()> {
        // {"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}
        let method = json_request_body
            .get("method")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow::anyhow!("bad id"))?
            .to_string();
        let params = json_request_body
            .get("params")
            .ok_or_else(|| anyhow::anyhow!("no params"))?
            .to_owned();

        // send the query to all the servers
        let bodies = future::join_all(rpc_servers.into_iter().map(|rpc| {
            let connections = connections.clone();
            let method = method.clone();
            let params = params.clone();
            let tx = tx.clone();

            async move {
                // get the client for this rpc server
                let provider = connections.get(&rpc).unwrap().clone_provider();

                let response = provider.request(&method, params).await;

                connections.get_mut(&rpc).unwrap().dec_active_requests();

                let response = response?;

                // TODO: if "no block with that header" or some other jsonrpc errors, skip this response

                // send the first good response to a one shot channel. that way we respond quickly
                // drop the result because errors are expected after the first send
                let _ = tx.send(Ok(response));

                Ok::<(), anyhow::Error>(())
            }
        }))
        .await;

        // TODO: use iterators instead of pushing into a vec
        let mut errs = vec![];
        for x in bodies {
            match x {
                Ok(_) => {}
                Err(e) => {
                    // TODO: better errors
                    warn!("Got an error sending request: {}", e);
                    errs.push(e);
                }
            }
        }

        // get the first error (if any)
        let e: anyhow::Result<serde_json::Value> = if !errs.is_empty() {
            Err(errs.pop().unwrap())
        } else {
            Err(anyhow::anyhow!("no successful responses"))
        };

        // send the error to the channel
        if tx.send(e).is_ok() {
            // if we were able to send an error, then we never sent a success
            return Err(anyhow::anyhow!("no successful responses"));
        } else {
            // if sending the error failed. the other side must be closed (which means we sent a success earlier)
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() {
    // install global collector configured based on RUST_LOG env var.
    tracing_subscriber::fmt::init();

    // TODO: load the config from yaml instead of hard coding
    // TODO: support multiple chains in one process? then we could just point "chain.stytt.com" at this and caddy wouldn't need anything else
    // TODO: be smart about about using archive nodes? have a set that doesn't use archive nodes since queries to them are more valuable
    let listen_port = 8445;

    let state = Web3ProxyApp::try_new(
        vec![
            // local nodes
            vec![("ws://10.11.12.16:8545", 0), ("ws://10.11.12.16:8946", 0)],
            // paid nodes
            // TODO: add paid nodes (with rate limits)
            vec![
                // chainstack.com archive
                (
                    "wss://ws-nd-373-761-850.p2pify.com/106d73af4cebc487df5ba92f1ad8dee7",
                    0,
                ),
            ],
            // free nodes
            vec![
                // ("https://main-rpc.linkpool.io", 0), // linkpool is slow
                ("https://rpc.ankr.com/eth", 0),
            ],
        ],
        vec![
            ("https://api.edennetwork.io/v1/beta", 0),
            ("https://api.edennetwork.io/v1/", 0),
        ],
    )
    .await
    .unwrap();

    let state: Arc<Web3ProxyApp> = Arc::new(state);

    let proxy_rpc_filter = warp::any()
        .and(warp::post())
        .and(warp::body::json())
        .then(move |json_body| state.clone().proxy_web3_rpc(json_body))
        .map(handle_anyhow_errors);

    warp::serve(proxy_rpc_filter)
        .run(([0, 0, 0, 0], listen_port))
        .await;
}

/// convert result into an http response. use this at the end of your warp filter
pub fn handle_anyhow_errors<T: warp::Reply>(res: anyhow::Result<T>) -> Box<dyn warp::Reply> {
    match res {
        Ok(r) => Box::new(r.into_response()),
        Err(e) => Box::new(warp::reply::with_status(
            format!("{}", e),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}
