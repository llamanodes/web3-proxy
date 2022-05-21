use crate::config::Web3ConnectionConfig;
use crate::connections::Web3Connections;
use crate::jsonrpc::JsonRpcErrorData;
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcForwardedResponseEnum;
use crate::jsonrpc::JsonRpcRequest;
use crate::jsonrpc::JsonRpcRequestEnum;
use dashmap::DashMap;
use ethers::prelude::{HttpClientError, ProviderError, WsClientError, H256};
use futures::future::join_all;
use linkedhashmap::LinkedHashMap;
use parking_lot::RwLock;
use redis_cell_client::RedisCellClient;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task;
use tokio::time::sleep;
use tracing::{debug, info, instrument, trace, warn};

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

// TODO: put this in config? what size should we do?
const RESPONSE_CACHE_CAP: usize = 1024;

/// TODO: these types are probably very bad keys and values. i couldn't get caching of warp::reply::Json to work
type CacheKey = (H256, String, Option<String>);

type ResponseLruCache = RwLock<LinkedHashMap<CacheKey, JsonRpcForwardedResponse>>;

type ActiveRequestsMap = DashMap<CacheKey, watch::Receiver<bool>>;

/// The application
// TODO: this debug impl is way too verbose. make something smaller
// TODO: if Web3ProxyApp is always in an Arc, i think we can avoid having at least some of these internal things in arcs
pub struct Web3ProxyApp {
    /// Send requests to the best server available
    balanced_rpcs: Arc<Web3Connections>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    private_rpcs: Arc<Web3Connections>,
    active_requests: ActiveRequestsMap,
    response_cache: ResponseLruCache,
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}

impl Web3ProxyApp {
    // #[instrument(name = "try_new_Web3ProxyApp", skip_all)]
    pub async fn try_new(
        chain_id: usize,
        redis_address: Option<String>,
        balanced_rpcs: Vec<Web3ConnectionConfig>,
        private_rpcs: Vec<Web3ConnectionConfig>,
    ) -> anyhow::Result<Web3ProxyApp> {
        // make a http shared client
        // TODO: how should we configure the connection pool?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = Some(
            reqwest::ClientBuilder::new()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(60))
                .user_agent(APP_USER_AGENT)
                .build()?,
        );

        let rate_limiter = match redis_address {
            Some(redis_address) => {
                info!("Conneting to redis on {}", redis_address);
                Some(RedisCellClient::open(&redis_address).await.unwrap())
            }
            None => None,
        };

        // TODO: attach context to this error
        let balanced_rpcs = Web3Connections::try_new(
            chain_id,
            balanced_rpcs,
            http_client.as_ref(),
            rate_limiter.as_ref(),
        )
        .await?;

        // TODO: do this separately instead of during try_new
        {
            let balanced_rpcs = balanced_rpcs.clone();
            task::spawn(async move {
                balanced_rpcs.subscribe_heads().await;
            });
        }

        // TODO: attach context to this error
        let private_rpcs = if private_rpcs.is_empty() {
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            balanced_rpcs.clone()
        } else {
            Web3Connections::try_new(
                chain_id,
                private_rpcs,
                http_client.as_ref(),
                rate_limiter.as_ref(),
            )
            .await?
        };

        Ok(Web3ProxyApp {
            balanced_rpcs,
            private_rpcs,
            active_requests: Default::default(),
            response_cache: Default::default(),
        })
    }

    pub fn get_balanced_rpcs(&self) -> &Web3Connections {
        &self.balanced_rpcs
    }

    pub fn get_private_rpcs(&self) -> &Web3Connections {
        &self.private_rpcs
    }

    pub fn get_active_requests(&self) -> &ActiveRequestsMap {
        &self.active_requests
    }

    /// send the request to the approriate RPCs
    /// TODO: dry this up
    #[instrument(skip_all)]
    pub async fn proxy_web3_rpc(
        self: Arc<Web3ProxyApp>,
        request: JsonRpcRequestEnum,
    ) -> anyhow::Result<JsonRpcForwardedResponseEnum> {
        // TODO: i don't always see this in the logs. why?
        debug!("Received request: {:?}", request);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => {
                JsonRpcForwardedResponseEnum::Single(self.proxy_web3_rpc_request(request).await?)
            }
            JsonRpcRequestEnum::Batch(requests) => {
                JsonRpcForwardedResponseEnum::Batch(self.proxy_web3_rpc_requests(requests).await?)
            }
        };

        // TODO: i don't always see this in the logs. why?
        debug!("Forwarding response: {:?}", response);

        Ok(response)
    }

    // #[instrument(skip_all)]
    async fn proxy_web3_rpc_requests(
        self: Arc<Web3ProxyApp>,
        requests: Vec<JsonRpcRequest>,
    ) -> anyhow::Result<Vec<JsonRpcForwardedResponse>> {
        // TODO: we should probably change ethers-rs to support this directly
        // we cut up the request and send to potentually different servers. this could be a problem.
        // if the client needs consistent blocks, they should specify instead of assume batches work on the same
        // TODO: is spawning here actually slower?
        let num_requests = requests.len();
        let responses = join_all(
            requests
                .into_iter()
                .map(|request| self.clone().proxy_web3_rpc_request(request))
                .collect::<Vec<_>>(),
        )
        .await;

        // TODO: i'm sure this could be done better with iterators
        let mut collected: Vec<JsonRpcForwardedResponse> = Vec::with_capacity(num_requests);
        for response in responses {
            collected.push(response?);
        }

        Ok(collected)
    }

    // #[instrument(skip_all)]
    async fn proxy_web3_rpc_request(
        self: Arc<Web3ProxyApp>,
        request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        trace!("Received request: {:?}", request);

        if request.method == "eth_sendRawTransaction" {
            // there are private rpcs configured and the request is eth_sendSignedTransaction. send to all private rpcs
            // TODO: think more about this lock. i think it won't actually help the herd. it probably makes it worse if we have a tight lag_limit
            match self.private_rpcs.get_upstream_servers().await {
                Ok(active_request_handles) => {
                    let (tx, rx) = flume::unbounded();

                    let connections = self.private_rpcs.clone();
                    let method = request.method.clone();
                    let params = request.params.clone();

                    // TODO: benchmark this compared to waiting on unbounded futures
                    // TODO: do something with this handle?
                    task::Builder::default()
                        .name("try_send_parallel_requests")
                        .spawn(async move {
                            connections
                                .try_send_parallel_requests(
                                    active_request_handles,
                                    method,
                                    params,
                                    tx,
                                )
                                .await
                        });

                    // wait for the first response
                    // TODO: we don't want the first response. we want the quorum response
                    let backend_response = rx.recv_async().await?;

                    if let Ok(backend_response) = backend_response {
                        // TODO: i think we
                        let response = JsonRpcForwardedResponse {
                            jsonrpc: "2.0".to_string(),
                            id: request.id,
                            result: Some(backend_response),
                            error: None,
                        };
                        return Ok(response);
                    }
                }
                Err(None) => {
                    // TODO: return a 502?
                    return Err(anyhow::anyhow!("no private rpcs!"));
                }
                Err(Some(retry_after)) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    sleep(retry_after).await;

                    warn!("All rate limits exceeded. Sleeping");
                }
            };
        } else {
            // this is not a private transaction (or no private relays are configured)
            // TODO: how much should we retry?
            for _i in 0..10usize {
                // TODO: think more about this loop.
                // // TODO: add more to this span. and do it properly
                // let span = info_span!("i", ?i);
                // let _enter = span.enter();

                // todo: move getting a cache_key or the result into a helper function. then we could have multiple caches
                // TODO: i think we are maybe getting stuck on this lock. maybe a new block arrives, it tries to write and gets hung up on something. then this can't proceed
                trace!("{:?} waiting for head_block_hash", request);

                let head_block_hash = self.balanced_rpcs.get_head_block_hash();

                trace!("{:?} head_block_hash {}", request, head_block_hash);

                // TODO: building this cache key is slow and its large, but i don't see a better way right now
                // TODO: inspect the params and see if a block is specified. if so, use that block number instead of current_block
                let cache_key = (
                    head_block_hash,
                    request.method.clone(),
                    request.params.clone().map(|x| x.to_string()),
                );

                // first check to see if this is cached
                if let Some(cached) = self.response_cache.read().get(&cache_key) {
                    let _ = self.active_requests.remove(&cache_key);

                    // TODO: emit a stat
                    trace!("{:?} cache hit!", request);

                    return Ok(cached.to_owned());
                } else {
                    trace!("{:?} cache miss!", request);
                }

                // check if this request is already in flight
                let (in_flight_tx, in_flight_rx) = watch::channel(true);
                let mut other_in_flight_rx = None;
                match self.active_requests.entry(cache_key.clone()) {
                    dashmap::mapref::entry::Entry::Occupied(entry) => {
                        other_in_flight_rx = Some(entry.get().clone());
                    }
                    dashmap::mapref::entry::Entry::Vacant(entry) => {
                        entry.insert(in_flight_rx);
                    }
                }

                if let Some(mut other_in_flight_rx) = other_in_flight_rx {
                    // wait for the other request to finish. it can finish successfully or with an error
                    trace!("{:?} waiting on in-flight request", request);

                    let _ = other_in_flight_rx.changed().await;

                    // now that we've waited, lets check the cache again
                    if let Some(cached) = self.response_cache.read().get(&cache_key) {
                        let _ = self.active_requests.remove(&cache_key);
                        let _ = in_flight_tx.send(false);

                        // TODO: emit a stat
                        trace!(
                            "{:?} cache hit after waiting for in-flight request!",
                            request
                        );

                        return Ok(cached.to_owned());
                    } else {
                        // TODO: emit a stat
                        trace!(
                            "{:?} cache miss after waiting for in-flight request!",
                            request
                        );
                    }
                }

                match self.balanced_rpcs.next_upstream_server().await {
                    Ok(active_request_handle) => {
                        let response = active_request_handle
                            .request(&request.method, &request.params)
                            .await;

                        let response = match response {
                            Ok(partial_response) => {
                                // TODO: trace here was really slow with millions of requests.
                                // trace!("forwarding request from {}", upstream_server);

                                let response = JsonRpcForwardedResponse {
                                    jsonrpc: "2.0".to_string(),
                                    id: request.id,
                                    // TODO: since we only use the result here, should that be all we return from try_send_request?
                                    result: Some(partial_response),
                                    error: None,
                                };

                                // TODO: small race condidition here. parallel requests with the same query will both be saved to the cache
                                let mut response_cache = self.response_cache.write();

                                // TODO: cache the warp::reply to save us serializing every time
                                response_cache.insert(cache_key.clone(), response.clone());
                                if response_cache.len() >= RESPONSE_CACHE_CAP {
                                    // TODO: this isn't an LRU. it's a "least recently created". does that have a fancy name? should we make it an lru? these caches only live for one block
                                    response_cache.pop_front();
                                }

                                drop(response_cache);

                                // TODO: needing to remove manually here makes me think we should do this differently
                                let _ = self.active_requests.remove(&cache_key);
                                let _ = in_flight_tx.send(false);

                                response
                            }
                            Err(e) => {
                                // send now since we aren't going to cache an error response
                                let _ = in_flight_tx.send(false);

                                // TODO: move this to a helper function?
                                let code;
                                let message: String;
                                let data;

                                match e {
                                    ProviderError::JsonRpcClientError(e) => {
                                        // TODO: we should check what type the provider is rather than trying to downcast both types of errors
                                        if let Some(e) = e.downcast_ref::<HttpClientError>() {
                                            match &*e {
                                                HttpClientError::JsonRpcError(e) => {
                                                    code = e.code;
                                                    message = e.message.clone();
                                                    data = e.data.clone();
                                                }
                                                e => {
                                                    // TODO: improve this
                                                    code = -32603;
                                                    message = format!("{}", e);
                                                    data = None;
                                                }
                                            }
                                        } else if let Some(e) = e.downcast_ref::<WsClientError>() {
                                            match &*e {
                                                WsClientError::JsonRpcError(e) => {
                                                    code = e.code;
                                                    message = e.message.clone();
                                                    data = e.data.clone();
                                                }
                                                e => {
                                                    // TODO: improve this
                                                    code = -32603;
                                                    message = format!("{}", e);
                                                    data = None;
                                                }
                                            }
                                        } else {
                                            unimplemented!();
                                        }
                                    }
                                    _ => {
                                        code = -32603;
                                        message = format!("{}", e);
                                        data = None;
                                    }
                                }

                                JsonRpcForwardedResponse {
                                    jsonrpc: "2.0".to_string(),
                                    id: request.id,
                                    result: None,
                                    error: Some(JsonRpcErrorData {
                                        code,
                                        message,
                                        data,
                                    }),
                                }
                            }
                        };

                        // TODO: needing to remove manually here makes me think we should do this differently
                        let _ = self.active_requests.remove(&cache_key);

                        if response.error.is_some() {
                            trace!("Sending error reply: {:?}", response);

                            // errors already sent false to the in_flight_tx
                        } else {
                            trace!("Sending reply: {:?}", response);

                            let _ = in_flight_tx.send(false);
                        }

                        return Ok(response);
                    }
                    Err(None) => {
                        // TODO: this is too verbose. if there are other servers in other tiers, we use those!
                        warn!("No servers in sync!");

                        // TODO: needing to remove manually here makes me think we should do this differently
                        let _ = self.active_requests.remove(&cache_key);
                        let _ = in_flight_tx.send(false);

                        return Err(anyhow::anyhow!("no servers in sync"));
                    }
                    Err(Some(retry_after)) => {
                        // TODO: move this to a helper function
                        // sleep (TODO: with a lock?) until our rate limits should be available
                        // TODO: if a server catches up sync while we are waiting, we could stop waiting
                        warn!("All rate limits exceeded. Sleeping");

                        sleep(retry_after).await;

                        // TODO: needing to remove manually here makes me think we should do this differently
                        let _ = self.active_requests.remove(&cache_key);
                        let _ = in_flight_tx.send(false);

                        continue;
                    }
                }
            }
        }

        Err(anyhow::anyhow!("internal error"))
    }
}
