use dashmap::DashMap;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use warp::Filter;

/// Load balance to the least-connection rpc
struct BalancedRpcs {
    rpcs: RwLock<Vec<String>>,
    connections: DashMap<String, u32>,
    // TODO: what type? store with connections?
    // ratelimits: DashMap<String, u32>,
}

// TODO: also pass rate limits to this?
impl Into<BalancedRpcs> for Vec<&str> {
    fn into(self) -> BalancedRpcs {
        let mut rpcs: Vec<String> = vec![];
        let connections = DashMap::new();
        // let ratelimits = DashMap::new();

        // TODO: i'm sure there is a better way to do this with more iterator things like collect, but this works
        for s in self.into_iter() {
            rpcs.push(s.to_string());
            connections.insert(s.to_string(), 0);
            // ratelimits.insert(s.to_string(), 0);
        }

        BalancedRpcs {
            rpcs: RwLock::new(rpcs),
            connections,
            // ratelimits,
        }
    }
}

impl BalancedRpcs {
    async fn get_upstream_server(&self) -> Option<String> {
        let mut balanced_rpcs = self.rpcs.write().await;

        balanced_rpcs.sort_unstable_by(|a, b| {
            self.connections
                .get(a)
                .unwrap()
                .cmp(&self.connections.get(b).unwrap())
        });

        // TODO: don't just grab the first. check rate limits
        if let Some(selected_rpc) = balanced_rpcs.first() {
            let mut connections = self.connections.get_mut(selected_rpc).unwrap();
            *connections += 1;

            return Some(selected_rpc.clone());
        }

        None
    }
}

/// Send to all the Rpcs
struct LoudRpcs {
    rpcs: Vec<String>,
    // TODO: what type? store with connections?
    // ratelimits: DashMap<String, u32>,
}

impl Into<LoudRpcs> for Vec<&str> {
    fn into(self) -> LoudRpcs {
        let mut rpcs: Vec<String> = vec![];
        // let ratelimits = DashMap::new();

        // TODO: i'm sure there is a better way to do this with more iterator things like collect, but this works
        for s in self.into_iter() {
            rpcs.push(s.to_string());
            // ratelimits.insert(s.to_string(), 0);
        }

        LoudRpcs {
            rpcs,
            // ratelimits,
        }
    }
}

impl LoudRpcs {
    async fn get_upstream_servers(&self) -> Vec<String> {
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
    fn new(balanced_rpc_tiers: Vec<Vec<&str>>, private_rpcs: Vec<&str>) -> Web3ProxyState {
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

            if let Ok(result) = self.try_send_requests(upstream_servers, &json_body).await {
                return Ok(result);
            }
        } else {
            // this is not a private transaction (or no private relays are configured)
            for balanced_rpcs in self.balanced_rpc_tiers.iter() {
                if let Some(upstream_server) = balanced_rpcs.get_upstream_server().await {
                    // TODO: capture any errors. at least log them
                    if let Ok(result) = self
                        .try_send_requests(vec![upstream_server], &json_body)
                        .await
                    {
                        return Ok(result);
                    }
                }
            }
        }

        return Err(anyhow::anyhow!("all servers failed"));
    }

    async fn try_send_requests(
        &self,
        upstream_servers: Vec<String>,
        json_body: &serde_json::Value,
    ) -> anyhow::Result<String> {
        // send the query to all the servers
        let mut future_responses = FuturesUnordered::new();
        for upstream_server in upstream_servers.into_iter() {
            let f = self.client.post(upstream_server).json(&json_body).send();

            future_responses.push(f);
        }

        // start loading text responses
        let mut future_text = FuturesUnordered::new();
        while let Some(request) = future_responses.next().await {
            if let Ok(request) = request {
                let f = request.text();

                future_text.push(f);
            }
        }

        // return the first response
        while let Some(text) = future_text.next().await {
            if let Ok(text) = text {
                // TODO: if "no block with that header", skip this response (maybe retry)
                return Ok(text);
            }
            // TODO: capture errors
        }

        Err(anyhow::anyhow!("no successful responses"))
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
            vec!["https://10.11.12.16:8545"],
            // paid nodes
            // TODO: add them
            // free nodes
            vec!["https://main-rpc.linkpool.io", "https://rpc.ankr.com/eth"],
        ],
        vec!["https://api.edennetwork.io/v1/beta"],
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
