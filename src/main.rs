use dashmap::DashMap;
use futures::future;
use governor::clock::{QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::{NotUntil, RateLimiter};
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::RwLock;
// use tokio::time::{sleep, Duration};
use warp::Filter;

type RpcRateLimiter =
    RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>;

type RateLimiterMap = DashMap<String, RpcRateLimiter>;
type ConnectionsMap = DashMap<String, u32>;

/// Load balance to the least-connection rpc
struct BalancedRpcs {
    rpcs: RwLock<Vec<String>>,
    connections: ConnectionsMap,
    ratelimits: RateLimiterMap,
}

// TODO: also pass rate limits to this?
impl Into<BalancedRpcs> for Vec<(&str, u32)> {
    fn into(self) -> BalancedRpcs {
        let mut rpcs: Vec<String> = vec![];
        let connections = DashMap::new();
        let ratelimits = DashMap::new();

        for (s, limit) in self.into_iter() {
            rpcs.push(s.to_string());
            connections.insert(s.to_string(), 0);

            if limit > 0 {
                let quota = governor::Quota::per_second(NonZeroU32::new(limit).unwrap());

                let rate_limiter = governor::RateLimiter::direct(quota);

                ratelimits.insert(s.to_string(), rate_limiter);
            }
        }

        BalancedRpcs {
            rpcs: RwLock::new(rpcs),
            connections,
            ratelimits,
        }
    }
}

impl BalancedRpcs {
    async fn get_upstream_server(&self) -> Result<String, NotUntil<QuantaInstant>> {
        let mut balanced_rpcs = self.rpcs.write().await;

        balanced_rpcs.sort_unstable_by(|a, b| {
            self.connections
                .get(a)
                .unwrap()
                .cmp(&self.connections.get(b).unwrap())
        });

        let mut earliest_not_until = None;

        for selected_rpc in balanced_rpcs.iter() {
            // check rate limits
            match self.ratelimits.get(selected_rpc).unwrap().check() {
                Ok(_) => {
                    // rate limit succeeded
                }
                Err(not_until) => {
                    // rate limit failed
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
                    continue;
                }
            };

            // increment our connection counter
            let mut connections = self.connections.get_mut(selected_rpc).unwrap();
            *connections += 1;

            // return the selected RPC
            return Ok(selected_rpc.clone());
        }

        // return the smallest not_until
        if let Some(not_until) = earliest_not_until {
            return Err(not_until);
        } else {
            unimplemented!();
        }
    }
}

/// Send to all the Rpcs
/// Unlike BalancedRpcs, there is no tracking of connections
/// We do still track rate limits
struct LoudRpcs {
    rpcs: Vec<String>,
    // TODO: what type? store with connections?
    ratelimits: RateLimiterMap,
}

impl Into<LoudRpcs> for Vec<(&str, u32)> {
    fn into(self) -> LoudRpcs {
        let mut rpcs: Vec<String> = vec![];
        let ratelimits = RateLimiterMap::new();

        for (s, limit) in self.into_iter() {
            rpcs.push(s.to_string());

            if limit > 0 {
                let quota = governor::Quota::per_second(NonZeroU32::new(limit).unwrap());

                let rate_limiter = governor::RateLimiter::direct(quota);

                ratelimits.insert(s.to_string(), rate_limiter);
            }
        }

        LoudRpcs { rpcs, ratelimits }
    }
}

impl LoudRpcs {
    async fn get_upstream_servers(&self) -> Vec<String> {
        //
        self.rpcs.clone()
    }

    fn as_bool(&self) -> bool {
        self.rpcs.len() > 0
    }
}

struct Web3ProxyState {
    client: reqwest::Client,
    balanced_rpc_tiers: Vec<BalancedRpcs>,
    private_rpcs: LoudRpcs,
}

impl Web3ProxyState {
    fn new(
        balanced_rpc_tiers: Vec<Vec<(&str, u32)>>,
        private_rpcs: Vec<(&str, u32)>,
    ) -> Web3ProxyState {
        // TODO: warn if no private relays
        Web3ProxyState {
            client: reqwest::Client::new(),
            balanced_rpc_tiers: balanced_rpc_tiers.into_iter().map(Into::into).collect(),
            private_rpcs: private_rpcs.into(),
        }
    }

    /// send the request to the approriate RPCs
    async fn proxy_web3_rpc(
        self: Arc<Web3ProxyState>,
        json_body: serde_json::Value,
    ) -> anyhow::Result<impl warp::Reply> {
        let eth_send_raw_transaction =
            serde_json::Value::String("eth_sendRawTransaction".to_string());

        if self.private_rpcs.as_bool() && json_body.get("method") == Some(&eth_send_raw_transaction)
        {
            // there are private rpcs configured and the request is eth_sendSignedTransaction. send to all private rpcs
            let upstream_servers = self.private_rpcs.get_upstream_servers().await;

            if let Ok(result) = self
                .try_send_requests(upstream_servers, None, &json_body)
                .await
            {
                return Ok(result);
            }
        } else {
            // this is not a private transaction (or no private relays are configured)
            for balanced_rpcs in self.balanced_rpc_tiers.iter() {
                if let Ok(upstream_server) = balanced_rpcs.get_upstream_server().await {
                    // TODO: capture any errors. at least log them
                    if let Ok(result) = self
                        .try_send_requests(
                            vec![upstream_server],
                            Some(&balanced_rpcs.connections),
                            &json_body,
                        )
                        .await
                    {
                        return Ok(result);
                    }
                } else {
                    // TODO: if we got an error. save the ratelimit NotUntil so we can sleep until then before trying again
                }
            }
        }

        return Err(anyhow::anyhow!("all servers failed"));
    }

    async fn try_send_requests(
        &self,
        upstream_servers: Vec<String>,
        connections: Option<&ConnectionsMap>,
        json_body: &serde_json::Value,
    ) -> anyhow::Result<String> {
        // send the query to all the servers
        let bodies = future::join_all(upstream_servers.into_iter().map(|url| {
            let client = self.client.clone();
            let json_body = json_body.clone();
            tokio::spawn(async move {
                // TODO: there has to be a better way to do this map and map_err
                client
                    .post(&url)
                    .json(&json_body)
                    .send()
                    .await
                    // add the url to the error so that we can decrement
                    .map_err(|e| (url.clone(), e))?
                    .text()
                    .await
                    // add the url to the result and the error so that we can decrement
                    .map(|t| (url.clone(), t))
                    .map_err(|e| (url, e))
            })
        }))
        .await;

        // we are going to collect successes and failures
        let mut oks = vec![];
        let mut errs = vec![];

        // TODO: parallel?
        for b in bodies {
            match b {
                Ok(Ok((url, b))) => {
                    // reduce connection counter
                    if let Some(connections) = connections {
                        *connections.get_mut(&url).unwrap() -= 1;
                    }

                    // TODO: if "no block with that header", skip this response (maybe retry)
                    oks.push(b);
                }
                Ok(Err((url, e))) => {
                    // reduce connection counter
                    if let Some(connections) = connections {
                        *connections.get_mut(&url).unwrap() -= 1;
                    }

                    // TODO: better errors
                    eprintln!("Got a reqwest::Error: {}", e);
                    errs.push(anyhow::anyhow!("Got a reqwest::Error"));
                }
                Err(e) => {
                    // TODO: better errors
                    eprintln!("Got a tokio::JoinError: {}", e);
                    errs.push(anyhow::anyhow!("Got a tokio::JoinError"));
                }
            }
        }

        // TODO: which response should we use?
        if oks.len() > 0 {
            return Ok(oks.pop().unwrap());
        } else if errs.len() > 0 {
            return Err(errs.pop().unwrap());
        } else {
            return Err(anyhow::anyhow!("no successful responses"));
        }
    }
}

#[tokio::main]
async fn main() {
    // TODO: load the config from yaml instead of hard coding
    // TODO: support multiple chains in one process. then we could just point "chain.stytt.com" at this and caddy wouldn't need anything else
    // TODO: i kind of want to make use of caddy's load balancing and health checking and such though
    let listen_port = 8445;
    // TODO: be smart about about using archive nodes?
    let state = Web3ProxyState::new(
        vec![
            // local nodes
            vec![("https://10.11.12.16:8545", 0)],
            // paid nodes
            // TODO: add paid nodes (with rate limits)
            // free nodes
            // TODO: add rate limits
            vec![
                ("https://main-rpc.linkpool.io", 0),
                ("https://rpc.ankr.com/eth", 0),
            ],
        ],
        vec![
            ("https://api.edennetwork.io/v1/beta", 0),
            ("https://api.edennetwork.io/v1/", 0),
        ],
    );

    let state: Arc<Web3ProxyState> = Arc::new(state);

    let proxy_rpc_filter = warp::any()
        .and(warp::post())
        .and(warp::body::json())
        .then(move |json_body| state.clone().proxy_web3_rpc(json_body))
        .map(handle_anyhow_errors);

    println!("Listening on 0.0.0.0:{}", listen_port);

    warp::serve(proxy_rpc_filter)
        .run(([0, 0, 0, 0], listen_port))
        .await;
}

/// convert result into an http response. use this at the end of your warp filter
pub fn handle_anyhow_errors<T: warp::Reply>(res: anyhow::Result<T>) -> Box<dyn warp::Reply> {
    match res {
        Ok(r) => Box::new(r.into_response()),
        // TODO: json error?
        Err(e) => Box::new(warp::reply::with_status(
            format!("{}", e),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}
