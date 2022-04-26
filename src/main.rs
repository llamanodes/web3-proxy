// TODO: don't use RwLock<HashMap>. i think we need a concurrent hashmap or we will hit all sorts of deadlocks

use derive_more::From;
use ethers::prelude::{Block, TxHash};
use ethers::providers::Middleware;
use futures::future;
use futures::StreamExt;
use governor::clock::{Clock, QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::{NotUntil, RateLimiter};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio::time::sleep;
use tracing::info;
use warp::Filter;

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

// TODO: i tried to use Box<dyn Provider>, but hit https://github.com/gakonst/ethers-rs/issues/592
#[derive(From)]
enum EthersProvider {
    Http(ethers::providers::Provider<ethers::providers::Http>),
    Ws(ethers::providers::Provider<ethers::providers::Ws>),
}

impl EthersProvider {
    /// Send a web3 request
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ethers::prelude::ProviderError> {
        match self {
            Self::Http(provider) => provider.request(method, params).await,
            Self::Ws(provider) => provider.request(method, params).await,
        }
    }

    /// Subscribe to new block heads
    pub async fn new_heads(&self, url: String, blocks: Arc<BlockMap>) -> anyhow::Result<()> {
        // TODO: automatically reconnect
        match &self {
            EthersProvider::Http(provider) => {
                let mut stream = provider.watch_blocks().await?;
                while let Some(block_number) = stream.next().await {
                    let block = provider.get_block(block_number).await?.unwrap();

                    println!(
                        "{:?} = {} Ts: {:?}, block number: {}",
                        block.hash.unwrap(),
                        url,
                        block.timestamp,
                        block.number.unwrap(),
                    );

                    let mut blocks = blocks.write().await;

                    blocks.insert(url.clone(), block);
                }
            }
            EthersProvider::Ws(provider) => {
                let mut stream = provider.subscribe_blocks().await?;
                while let Some(block) = stream.next().await {
                    // TODO: save the block into a dashmap on
                    println!(
                        "{:?} = {} Ts: {:?}, block number: {}",
                        block.hash.unwrap(),
                        url,
                        block.timestamp,
                        block.number.unwrap(),
                    );

                    let mut blocks = blocks.write().await;

                    blocks.insert(url.clone(), block);
                }
            }
        }

        Ok(())
    }
}

struct EthersConnection {
    /// keep track of currently open requests. We sort on this
    active_requests: u32,
    provider: Arc<EthersProvider>,
}

impl EthersConnection {
    async fn try_new(
        url_str: String,
        http_client: Option<reqwest::Client>,
        blocks: Arc<BlockMap>,
    ) -> anyhow::Result<EthersConnection> {
        // TODO: create an ethers-rs rpc client and subscribe/watch new heads in a spawned task
        let provider = if url_str.starts_with("http") {
            let url: url::Url = url_str.parse()?;

            let http_client = http_client.ok_or_else(|| anyhow::anyhow!("no http_client"))?;

            let provider = ethers::providers::Http::new_with_client(url, http_client);

            // TODO: dry this up
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(1))
                .into()
        } else if url_str.starts_with("ws") {
            let provider = ethers::providers::Ws::connect(url_str.clone()).await?;

            // TODO: make sure this survives disconnects

            // TODO: dry this up
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(1))
                .into()
        } else {
            return Err(anyhow::anyhow!("only http and ws servers are supported"));
        };

        let provider = Arc::new(provider);

        // subscribe to new heads in a spawned future
        // TODO: if http, maybe we should check them all on the same interval. and if there is at least one websocket, use that message to start check?
        let provider_clone: Arc<EthersProvider> = Arc::clone(&provider);
        tokio::spawn(async move { provider_clone.new_heads(url_str, blocks).await });

        Ok(EthersConnection {
            active_requests: 0,
            provider,
        })
    }

    fn inc(&mut self) {
        self.active_requests += 1;
    }

    fn dec(&mut self) {
        self.active_requests -= 1;
    }
}

impl Eq for EthersConnection {}

impl Ord for EthersConnection {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.active_requests.cmp(&other.active_requests)
    }
}

impl PartialOrd for EthersConnection {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EthersConnection {
    fn eq(&self, other: &Self) -> bool {
        self.active_requests == other.active_requests
    }
}

type BlockMap = RwLock<HashMap<String, Block<TxHash>>>;
type RateLimiterMap = RwLock<HashMap<String, RpcRateLimiter>>;
// TODO: include the ethers client on this map
type ConnectionsMap = RwLock<HashMap<String, EthersConnection>>;

type RpcRateLimiter =
    RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>;

/// Load balance to the least-connection rpc
struct RpcTier {
    rpcs: RwLock<Vec<String>>,
    connections: Arc<ConnectionsMap>,
    ratelimits: RateLimiterMap,
}

impl RpcTier {
    async fn try_new(
        servers: Vec<(&str, u32)>,
        http_client: Option<reqwest::Client>,
        blocks: Arc<BlockMap>,
        clock: &QuantaClock,
    ) -> anyhow::Result<RpcTier> {
        let mut rpcs: Vec<String> = vec![];
        let mut connections = HashMap::new();
        let mut ratelimits = HashMap::new();

        for (s, limit) in servers.into_iter() {
            rpcs.push(s.to_string());

            let connection =
                EthersConnection::try_new(s.to_string(), http_client.clone(), blocks.clone())
                    .await?;

            connections.insert(s.to_string(), connection);

            if limit > 0 {
                let quota = governor::Quota::per_second(NonZeroU32::new(limit).unwrap());

                let rate_limiter = governor::RateLimiter::direct_with_clock(quota, clock);

                ratelimits.insert(s.to_string(), rate_limiter);
            }
        }

        /*
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
        */

        Ok(RpcTier {
            rpcs: RwLock::new(rpcs),
            connections: Arc::new(RwLock::new(connections)),
            ratelimits: RwLock::new(ratelimits),
        })
    }

    /// get the best available rpc server
    async fn next_upstream_server(&self) -> Result<String, NotUntil<QuantaInstant>> {
        let mut balanced_rpcs = self.rpcs.write().await;

        // sort rpcs by their active connections
        let connections = self.connections.read().await;

        balanced_rpcs
            .sort_unstable_by(|a, b| connections.get(a).unwrap().cmp(connections.get(b).unwrap()));

        let mut earliest_not_until = None;

        for selected_rpc in balanced_rpcs.iter() {
            // TODO: check current block number. if behind, make our own NotUntil here
            let ratelimits = self.ratelimits.write().await;

            // check rate limits
            match ratelimits.get(selected_rpc).unwrap().check() {
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
            self.connections
                .write()
                .await
                .get_mut(selected_rpc)
                .unwrap()
                .inc();

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

    /// get all available rpc servers
    async fn get_upstream_servers(&self) -> Result<Vec<String>, NotUntil<QuantaInstant>> {
        let mut earliest_not_until = None;

        let mut selected_rpcs = vec![];

        for selected_rpc in self.rpcs.read().await.iter() {
            // check rate limits
            match self
                .ratelimits
                .write()
                .await
                .get(selected_rpc)
                .unwrap()
                .check()
            {
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
            self.connections
                .write()
                .await
                .get_mut(selected_rpc)
                .unwrap()
                .inc();

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
            // TODO: is this right?
            Ok(vec![])
        }
    }
}

struct Web3ProxyState {
    clock: QuantaClock,
    balanced_rpc_tiers: Arc<Vec<RpcTier>>,
    private_rpcs: Option<Arc<RpcTier>>,
    /// write lock on these when all rate limits are hit
    balanced_rpc_ratelimiter_lock: RwLock<()>,
    private_rpcs_ratelimiter_lock: RwLock<()>,
}

impl Web3ProxyState {
    async fn try_new(
        balanced_rpc_tiers: Vec<Vec<(&str, u32)>>,
        private_rpcs: Vec<(&str, u32)>,
    ) -> anyhow::Result<Web3ProxyState> {
        let clock = QuantaClock::default();

        let blocks = Arc::new(BlockMap::default());

        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something
        let http_client = reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(300))
            .user_agent(APP_USER_AGENT)
            .build()?;

        // TODO: i'm sure we s
        let balanced_rpc_tiers = Arc::new(
            future::join_all(balanced_rpc_tiers.into_iter().map(|balanced_rpc_tier| {
                RpcTier::try_new(
                    balanced_rpc_tier,
                    Some(http_client.clone()),
                    blocks.clone(),
                    &clock,
                )
            }))
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<RpcTier>>>()?,
        );

        let private_rpcs = if private_rpcs.is_empty() {
            None
        } else {
            Some(Arc::new(
                RpcTier::try_new(private_rpcs, Some(http_client), blocks.clone(), &clock).await?,
            ))
        };

        // TODO: warn if no private relays
        Ok(Web3ProxyState {
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
        self: Arc<Web3ProxyState>,
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

                match private_rpcs.get_upstream_servers().await {
                    Ok(upstream_servers) => {
                        let (tx, mut rx) =
                            mpsc::unbounded_channel::<anyhow::Result<serde_json::Value>>();

                        let clone = self.clone();
                        let connections = private_rpcs.connections.clone();
                        let json_body = json_body.clone();

                        tokio::spawn(async move {
                            clone
                                .try_send_requests(upstream_servers, connections, json_body, tx)
                                .await
                        });

                        let response = rx
                            .recv()
                            .await
                            .ok_or_else(|| anyhow::anyhow!("no successful response"))?;

                        if let Ok(response) = response {
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

                for balanced_rpcs in self.balanced_rpc_tiers.iter() {
                    match balanced_rpcs.next_upstream_server().await {
                        Ok(upstream_server) => {
                            let (tx, mut rx) =
                                mpsc::unbounded_channel::<anyhow::Result<serde_json::Value>>();

                            let clone = self.clone();
                            let connections = balanced_rpcs.connections.clone();
                            let json_body = json_body.clone();

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

                            let response = rx
                                .recv()
                                .await
                                .ok_or_else(|| anyhow::anyhow!("no successful response"))?;

                            if let Ok(response) = response {
                                return Ok(warp::reply::json(&response));
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
        connections: Arc<ConnectionsMap>,
        json_request_body: serde_json::Value,
        tx: mpsc::UnboundedSender<anyhow::Result<serde_json::Value>>,
    ) -> anyhow::Result<()> {
        // {"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}
        let incoming_id = json_request_body
            .get("id")
            .ok_or_else(|| anyhow::anyhow!("bad id"))?
            .to_owned();
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
            let incoming_id = incoming_id.clone();
            let connections = connections.clone();
            let method = method.clone();
            let params = params.clone();
            let tx = tx.clone();

            async move {
                // get the client for this rpc server
                let provider = connections.read().await.get(&rpc).unwrap().provider.clone();

                // TODO: there has to be a better way to attach the url to the result
                let mut response = provider.request(&method, params).await?;

                connections.write().await.get_mut(&rpc).unwrap().dec();

                if let Some(response_id) = response.get_mut("id") {
                    *response_id = incoming_id;
                }

                // send the first good response to a one shot channel. that way we respond quickly
                // drop the result because errors are expected after the first send
                // TODO: if "no block with that header" or some other jsonrpc errors, skip this response
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
                    eprintln!("Got a tokio::JoinError: {}", e);
                    errs.push(anyhow::anyhow!("Got a tokio::JoinError"));
                }
            }
        }

        let e: anyhow::Result<serde_json::Value> = if !errs.is_empty() {
            Err(errs.pop().unwrap())
        } else {
            Err(anyhow::anyhow!("no successful responses"))
        };

        // TODO: think about this more. we want to send it
        if tx.send(e).is_ok() {
            // if we were able to send an error, then we never sent a success
            return Err(anyhow::anyhow!("no successful responses"));
        } else {
            // sending the error failed. the other side must be closed (which means we sent a success)
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() {
    // install global collector configured based on RUST_LOG env var.
    tracing_subscriber::fmt::init();

    info!("starting");

    // TODO: load the config from yaml instead of hard coding
    // TODO: support multiple chains in one process. then we could just point "chain.stytt.com" at this and caddy wouldn't need anything else
    // TODO: i kind of want to make use of caddy's load balancing and health checking and such though
    let listen_port = 8445;

    // TODO: be smart about about using archive nodes?
    let state = Web3ProxyState::try_new(
        vec![
            // local nodes
            vec![("ws://10.11.12.16:8545", 0), ("ws://10.11.12.16:8946", 0)],
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
    )
    .await
    .unwrap();

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
