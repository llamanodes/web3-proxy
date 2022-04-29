mod block_watcher;
mod provider;
mod provider_tiers;

use futures::future;
use governor::clock::{Clock, QuantaClock};
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;
use tracing::warn;
use warp::Filter;

use crate::block_watcher::BlockWatcher;
use crate::provider::JsonRpcRequest;
use crate::provider_tiers::Web3ProviderTier;

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

/// The application
// TODO: this debug impl is way too verbose. make something smaller
struct Web3ProxyApp {
    /// clock used for rate limiting
    /// TODO: use tokio's clock (will require a different ratelimiting crate)
    clock: QuantaClock,
    /// Send requests to the best server available
    balanced_rpc_tiers: Arc<Vec<Web3ProviderTier>>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    private_rpcs: Option<Arc<Web3ProviderTier>>,
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}

impl Web3ProxyApp {
    async fn try_new(
        allowed_lag: u64,
        balanced_rpc_tiers: Vec<Vec<(&str, u32, Option<u32>)>>,
        private_rpcs: Vec<(&str, u32, Option<u32>)>,
    ) -> anyhow::Result<Web3ProxyApp> {
        let clock = QuantaClock::default();

        let mut rpcs = vec![];
        for balanced_rpc_tier in balanced_rpc_tiers.iter() {
            for rpc_data in balanced_rpc_tier {
                let rpc = rpc_data.0.to_string();

                rpcs.push(rpc);
            }
        }
        for rpc_data in private_rpcs.iter() {
            let rpc = rpc_data.0.to_string();

            rpcs.push(rpc);
        }

        let block_watcher = Arc::new(BlockWatcher::new(rpcs));

        // make a http shared client
        // TODO: how should we configure the connection pool?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = reqwest::ClientBuilder::new()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(300))
            .user_agent(APP_USER_AGENT)
            .build()?;

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
            // TODO: instead of None, set it to a list of all the rpcs from balanced_rpc_tiers. that way we broadcast very loudly
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

        let (new_block_sender, mut new_block_receiver) = watch::channel::<String>("".to_string());

        {
            // TODO: spawn this later?
            // spawn a future for the block_watcher
            let block_watcher = block_watcher.clone();
            tokio::spawn(async move { block_watcher.run(new_block_sender).await });
        }

        {
            // spawn a future for sorting our synced rpcs
            // TODO: spawn this later?
            let balanced_rpc_tiers = balanced_rpc_tiers.clone();
            let private_rpcs = private_rpcs.clone();
            let block_watcher = block_watcher.clone();

            tokio::spawn(async move {
                let mut tier_map = HashMap::new();
                let mut private_map = HashMap::new();

                for balanced_rpc_tier in balanced_rpc_tiers.iter() {
                    for rpc in balanced_rpc_tier.clone_rpcs() {
                        tier_map.insert(rpc, balanced_rpc_tier);
                    }
                }

                if let Some(private_rpcs) = private_rpcs {
                    for rpc in private_rpcs.clone_rpcs() {
                        private_map.insert(rpc, private_rpcs.clone());
                    }
                }

                while new_block_receiver.changed().await.is_ok() {
                    let updated_rpc = new_block_receiver.borrow().clone();

                    if let Some(tier) = tier_map.get(&updated_rpc) {
                        tier.update_synced_rpcs(block_watcher.clone(), allowed_lag)
                            .unwrap();
                    } else if let Some(tier) = private_map.get(&updated_rpc) {
                        tier.update_synced_rpcs(block_watcher.clone(), allowed_lag)
                            .unwrap();
                    } else {
                        panic!("howd this happen");
                    }
                }
            });
        }

        Ok(Web3ProxyApp {
            clock,
            balanced_rpc_tiers,
            private_rpcs,
        })
    }

    /// send the request to the approriate RPCs
    /// TODO: dry this up
    async fn proxy_web3_rpc(
        self: Arc<Web3ProxyApp>,
        json_body: JsonRpcRequest,
    ) -> anyhow::Result<impl warp::Reply> {
        if self.private_rpcs.is_some() && json_body.method == "eth_sendRawTransaction" {
            let private_rpcs = self.private_rpcs.as_ref().unwrap();

            // there are private rpcs configured and the request is eth_sendSignedTransaction. send to all private rpcs
            loop {
                // TODO: think more about this lock. i think it won't actually help the herd. it probably makes it worse if we have a tight lag_limit
                match private_rpcs.get_upstream_servers().await {
                    Ok(upstream_servers) => {
                        let (tx, mut rx) = mpsc::unbounded_channel();

                        let connections = private_rpcs.clone_connections();
                        let method = json_body.method.clone();
                        let params = json_body.params.clone();

                        tokio::spawn(async move {
                            connections
                                .try_send_requests(upstream_servers, method, params, tx)
                                .await
                        });

                        // wait for the first response
                        let response = rx
                            .recv()
                            .await
                            .ok_or_else(|| anyhow::anyhow!("no successful response"))?;

                        if let Ok(partial_response) = response {
                            let response = json!({
                                "jsonrpc": "2.0",
                                "id": json_body.id,
                                "result": partial_response
                            });
                            return Ok(warp::reply::json(&response));
                        }
                    }
                    Err(not_until) => {
                        // TODO: move this to a helper function
                        // sleep (with a lock) until our rate limits should be available
                        if let Some(not_until) = not_until {
                            let deadline = not_until.wait_time_from(self.clock.now());

                            sleep(deadline).await;
                        }
                    }
                };
            }
        } else {
            // this is not a private transaction (or no private relays are configured)
            // try to send to each tier, stopping at the first success
            loop {
                // there are multiple tiers. save the earliest not_until (if any). if we don't return, we will sleep until then and then try again
                let mut earliest_not_until = None;

                for balanced_rpcs in self.balanced_rpc_tiers.iter() {
                    // TODO: what allowed lag?
                    match balanced_rpcs.next_upstream_server().await {
                        Ok(upstream_server) => {
                            let connections = balanced_rpcs.connections();

                            let response = connections
                                .try_send_request(
                                    upstream_server,
                                    json_body.method.clone(),
                                    json_body.params.clone(),
                                )
                                .await;

                            let response = match response {
                                Ok(partial_response) => {
                                    // TODO: trace here was really slow with millions of requests.
                                    // info!("forwarding request from {}", upstream_server);

                                    json!({
                                        // TODO: re-use their jsonrpc?
                                        "jsonrpc": "2.0",
                                        "id": json_body.id,
                                        "result": partial_response
                                    })
                                }
                                Err(e) => {
                                    // TODO: what is the proper format for an error?
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": json_body.id,
                                        "error": format!("{}", e)
                                    })
                                }
                            };

                            return Ok(warp::reply::json(&response));
                        }
                        Err(None) => {
                            warn!("No servers in sync!");
                        }
                        Err(Some(not_until)) => {
                            // save the smallest not_until. if nothing succeeds, return an Err with not_until in it
                            // TODO: helper function for this
                            if earliest_not_until.is_none() {
                                earliest_not_until.replace(not_until);
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
                if let Some(earliest_not_until) = earliest_not_until {
                    let deadline = earliest_not_until.wait_time_from(self.clock.now());

                    sleep(deadline).await;
                } else {
                    // TODO: how long should we wait?
                    // TODO: max wait time?
                    sleep(Duration::from_millis(500)).await;
                };
            }
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
    // TODO: what should this be? 0 will cause a thundering herd
    let allowed_lag = 0;

    let state = Web3ProxyApp::try_new(
        allowed_lag,
        vec![
            // local nodes
            vec![
                ("ws://10.11.12.16:8545", 68_800, None),
                ("ws://10.11.12.16:8946", 152_138, None),
            ],
            // paid nodes
            // TODO: add paid nodes (with rate limits)
            // vec![
            //     // chainstack.com archive
            //     // moralis free (25/sec rate limit)
            // ],
            // free nodes
            // vec![
            //     // ("https://main-rpc.linkpool.io", 4_779, None), // linkpool is slow and often offline
            //     ("https://rpc.ankr.com/eth", 23_967, None),
            // ],
        ],
        vec![
            // ("https://api.edennetwork.io/v1/", 1_805, None),
            // ("https://api.edennetwork.io/v1/beta", 300, None),
            // ("https://rpc.ethermine.org/", 5_861, None),
            // ("https://rpc.flashbots.net", 7074, None),
        ],
    )
    .await
    .unwrap();

    let state: Arc<Web3ProxyApp> = Arc::new(state);

    let proxy_rpc_filter = warp::any()
        .and(warp::post())
        .and(warp::body::json())
        .then(move |json_body| state.clone().proxy_web3_rpc(json_body));

    // TODO: filter for displaying connections and their block heights

    // TODO: warp trace is super verbose. how do we make this more readable?
    // let routes = proxy_rpc_filter.with(warp::trace::request());
    let routes = proxy_rpc_filter.map(handle_anyhow_errors);

    warp::serve(routes).run(([0, 0, 0, 0], listen_port)).await;
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
