use dashmap::DashMap;
use futures::future;
use futures::future::{AbortHandle, Abortable};
use futures::SinkExt;
use futures::StreamExt;
use governor::clock::{Clock, QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::{NotUntil, RateLimiter};
use regex::Regex;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite};
use warp::Filter;

type RateLimiterMap = DashMap<String, RpcRateLimiter>;
type ConnectionsMap = DashMap<String, u32>;

type RpcRateLimiter =
    RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>;

/// Load balance to the least-connection rpc
struct BalancedRpcs {
    rpcs: RwLock<Vec<String>>,
    connections: ConnectionsMap,
    ratelimits: RateLimiterMap,
    new_heads_handles: Vec<AbortHandle>,
}

impl Drop for BalancedRpcs {
    fn drop(&mut self) {
        for handle in self.new_heads_handles.iter() {
            handle.abort();
        }
    }
}

async fn handle_new_head_message(message: tungstenite::Message) -> anyhow::Result<()> {
    // TODO: move this to a "handle_new_head_message" function so that we can use the ? helper
    let data: serde_json::Value = serde_json::from_str(message.to_text().unwrap()).unwrap();

    // TODO: parse the message as json and get out the block data. then update a map for this rpc
    println!("now what? {:?}", data);

    unimplemented!();
}

impl BalancedRpcs {
    fn new(servers: Vec<(&str, u32)>, clock: &QuantaClock) -> BalancedRpcs {
        let mut rpcs: Vec<String> = vec![];
        let connections = DashMap::new();
        let ratelimits = DashMap::new();

        for (s, limit) in servers.into_iter() {
            rpcs.push(s.to_string());
            connections.insert(s.to_string(), 0);

            if limit > 0 {
                let quota = governor::Quota::per_second(NonZeroU32::new(limit).unwrap());

                let rate_limiter = governor::RateLimiter::direct_with_clock(quota, clock);

                ratelimits.insert(s.to_string(), rate_limiter);
            }
        }

        // TODO: subscribe to new_heads
        let new_heads_handles = rpcs
            .clone()
            .into_iter()
            .map(|rpc| {
                // start the subscription inside an abort handler. this way, dropping this BalancedRpcs will close these connections
                let (abort_handle, abort_registration) = AbortHandle::new_pair();

                tokio::spawn(Abortable::new(
                    async move {
                        // replace "http" at the start with "ws"
                        // TODO: this is fragile. some nodes use different ports, too. use proper config
                        // TODO: maybe we should use this websocket for more than just the new heads subscription. we could send all our requests over it (but would need to modify ids)
                        let re = Regex::new("^http").expect("bad regex");
                        let ws_rpc = re.replace(&rpc, "ws");

                        // TODO: if websocket not supported, use polling?
                        let ws_rpc = url::Url::parse(&ws_rpc).expect("invalid websocket url");

                        // loop so that if it disconnects, we reconnect
                        loop {
                            match connect_async(&ws_rpc).await {
                                Ok((ws_stream, _)) => {
                                    let (mut write, mut read) = ws_stream.split();

                                    // TODO: send eth_subscribe New Heads
                                    if (write.send(tungstenite::Message::Text("{\"id\": 1, \"method\": \"eth_subscribe\", \"params\": [\"newHeads\"]}".to_string())).await).is_ok() {
                                        if let Some(Ok(_first)) = read.next().await {
                                            // TODO: what should we do with the first message?

                                            while let Some(Ok(message)) = read.next().await {
                                                if let Err(e) = handle_new_head_message(message).await {
                                                    eprintln!("error handling new head message @ {}: {}", ws_rpc, e);
                                                    break;
                                                }
                                            }
                                        }
                                        // no more messages or we got an error
                                    }
                                }
                                Err(e) => {
                                    // TODO: proper logging
                                    eprintln!("error connecting to websocket @ {}: {}", ws_rpc, e);
                                }
                            }

                            // TODO: log that we are going to reconnectto ws_rpc in 1 second
                            // TODO: how long should we wait? exponential backoff?
                            sleep(Duration::from_secs(1)).await;
                        }
                    },
                    abort_registration,
                ));

                abort_handle
            })
            .collect();

        BalancedRpcs {
            rpcs: RwLock::new(rpcs),
            connections,
            ratelimits,
            new_heads_handles,
        }
    }

    /// get the best available rpc server
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
            Err(not_until)
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

impl LoudRpcs {
    fn new(servers: Vec<(&str, u32)>, clock: &QuantaClock) -> LoudRpcs {
        let mut rpcs: Vec<String> = vec![];
        let ratelimits = RateLimiterMap::new();

        for (s, limit) in servers.into_iter() {
            rpcs.push(s.to_string());

            if limit > 0 {
                let quota = governor::Quota::per_second(NonZeroU32::new(limit).unwrap());

                let rate_limiter = governor::RateLimiter::direct_with_clock(quota, clock);

                ratelimits.insert(s.to_string(), rate_limiter);
            }
        }

        LoudRpcs { rpcs, ratelimits }
    }

    /// get all available rpc servers
    async fn get_upstream_servers(&self) -> Result<Vec<String>, NotUntil<QuantaInstant>> {
        let mut earliest_not_until = None;

        let mut selected_rpcs = vec![];

        for selected_rpc in self.rpcs.iter() {
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

            // this is rpc should work
            selected_rpcs.push(selected_rpc.clone());
        }

        if !selected_rpcs.is_empty() {
            return Ok(selected_rpcs);
        }

        // return the earliest not_until
        if let Some(not_until) = earliest_not_until {
            Err(not_until)
        } else {
            panic!("i don't think this should happen")
        }
    }

    fn as_bool(&self) -> bool {
        !self.rpcs.is_empty()
    }
}

struct Web3ProxyState {
    clock: QuantaClock,
    client: reqwest::Client,
    // TODO: LoudRpcs and BalancedRpcs should probably share a trait or something
    balanced_rpc_tiers: Vec<BalancedRpcs>,
    private_rpcs: LoudRpcs,
    /// lock this when all rate limiters are hit
    balanced_rpc_ratelimiter_lock: RwLock<()>,
    private_rpcs_ratelimiter_lock: RwLock<()>,
}

impl Web3ProxyState {
    fn new(
        balanced_rpc_tiers: Vec<Vec<(&str, u32)>>,
        private_rpcs: Vec<(&str, u32)>,
    ) -> Web3ProxyState {
        let clock = QuantaClock::default();

        let balanced_rpc_tiers = balanced_rpc_tiers
            .into_iter()
            .map(|servers| BalancedRpcs::new(servers, &clock))
            .collect();

        let private_rpcs = LoudRpcs::new(private_rpcs, &clock);

        // TODO: warn if no private relays
        Web3ProxyState {
            clock,
            client: reqwest::Client::new(),
            balanced_rpc_tiers,
            private_rpcs,
            balanced_rpc_ratelimiter_lock: Default::default(),
            private_rpcs_ratelimiter_lock: Default::default(),
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
            loop {
                let read_lock = self.private_rpcs_ratelimiter_lock.read().await;

                match self.private_rpcs.get_upstream_servers().await {
                    Ok(upstream_servers) => {
                        if let Ok(result) = self
                            .try_send_requests(upstream_servers, None, &json_body)
                            .await
                        {
                            return Ok(result);
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
            loop {
                let read_lock = self.balanced_rpc_ratelimiter_lock.read().await;

                // there are multiple tiers. save the earliest not_until (if any). if we don't return, we will sleep until then and then try again
                let mut earliest_not_until = None;

                for balanced_rpcs in self.balanced_rpc_tiers.iter() {
                    match balanced_rpcs.get_upstream_server().await {
                        Ok(upstream_server) => {
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
                        }
                        Err(not_until) => {
                            // save the smallest not_until. if nothing succeeds, return an Err with not_until in it
                            if earliest_not_until.is_none() {
                                earliest_not_until = Some(not_until);
                            } else {
                                // TODO: do we need to unwrap this far? can we just compare the not_untils
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
                let deadline = earliest_not_until.unwrap().wait_time_from(self.clock.now());

                sleep(deadline).await;

                drop(write_lock);
            }
        }
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
                // TODO: there has to be a better way to attach the url to the result
                client
                    .post(&url)
                    .json(&json_body)
                    .send()
                    .await
                    // add the url to the error so that we can reduce connection counters
                    .map_err(|e| (url.clone(), e))?
                    .text()
                    .await
                    // add the url to the result so that we can reduce connection counters
                    .map(|t| (url.clone(), t))
                    // add the url to the error so that we can reduce connection counters
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

                    // TODO: if "no block with that header" or some other jsonrpc errors, skip this response
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
        if !oks.is_empty() {
            Ok(oks.pop().unwrap())
        } else if !errs.is_empty() {
            Err(errs.pop().unwrap())
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
