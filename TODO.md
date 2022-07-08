# Todo

## MVP

- [x] simple proxy
- [x] better locking. when lots of requests come in, we seem to be in the way of block updates
- [x] load balance between multiple RPC servers
- [x] support more than just ETH
- [x] option to disable private rpc and send everything to primary
- [x] support websocket clients
  - we support websockets for the backends already, but we need them for the frontend too
- [x] health check nodes by block height
- [x] Dockerfile
- [x] docker-compose.yml
- [x] after connecting to a server, check that it gives the expected chainId
- [x] the ethermine rpc is usually fastest. but its in the private tier. since we only allow synced rpcs, we are going to not have an rpc a lot of the time
- [x] if not backends. return a 502 instead of delaying?
- [x] move from warp to axum
- [x] handle websocket disconnect and reconnect
- [x] eth_sendRawTransaction should return the most common result, not the first
- [x] use redis and redis-cell for rate limits
- [x] it works for a few seconds and then gets stuck on something.
  - [x] its working with one backend node, but multiple breaks. something to do with pending transactions
  - [x] dashmap entry api is easy to deadlock! be careful with it!
- [x] the web3proxyapp object gets cloned for every call. why do we need any arcs inside that? shouldn't they be able to connect to the app's? can we just use static lifetimes
- [x] refactor Connection::spawn. have it return a handle to the spawned future of it running with block and transaction subscriptions
- [x] refactor Connections::spawn. have it return a handle that is selecting on those handles?
- [x] some production configs are occassionally stuck waiting at 100% cpu
  - they stop processing new blocks. i'm guessing 2 blocks arrive at the same time, but i thought our locks would handle that
  - even after removing a bunch of the locks, the deadlock still happens. i can't reliably reproduce. i just let it run for awhile and it happens.
  - running gdb shows the thread at tokio tungstenite thread is spinning near 100% cpu and none of the rest of the program is proceeding
  - fixed by https://github.com/gakonst/ethers-rs/pull/1287
- [x] when sending with private relays, brownie's tx.wait can think the transaction was dropped. smarter retry on eth_getTransactionByHash and eth_getTransactionReceipt (maybe only if we sent the transaction ourselves)
- [x] if web3 proxy gets an http error back, retry another node
- [x] endpoint for health checks. if no synced servers, give a 502 error
- [x] rpc errors propagate too far. one subscription failing ends the app. isolate the providers more (might already be fixed)
- [x] incoming rate limiting (by ip)
- [x] connection pool for redis
- [ ] automatically route to archive server when necessary
- [ ] handle log subscriptions
- [ ] basic request method stats
- [x] http servers should check block at the very start
- [ ] Got warning: "WARN subscribe_new_heads:send_block: web3_proxy::connection: unable to get block from https://rpc.ethermine.org: Deserialization Error: expected value at line 1 column 1. Response: error code: 1015". this is cloudflare rate limiting on fetching a block, but this is a private rpc. why is there a block subscription?

## V1

- [ ] refactor so configs can change while running
  - create the app without applying any config to it
  - have a blocking future watching the config file and calling app.apply_config() on first load and on change
  - work started on this in the "config_reloads" branch. because of how we pass channels around during spawn, this requires a larger refactor.
- [ ] interval for http subscriptions should be based on block time. load from config is easy, but better to query
- [ ] most things that are cached locally should probably be in shared redis caches
- [ ] stats when forks are resolved (and what chain they were on?)
- [ ] incoming rate limiting (by api key)
- [ ] failsafe. if no blocks or transactions in the last second, warn and reset the connection
- [ ] improved logging with useful instrumentation
- [ ] add a subscription that returns the head block number and hash but nothing else
- [ ] if we don't cache errors, then in-flight request caching is going to bottleneck 
- [ ] improve caching
  - [ ] if the eth_call (or similar) params include a block, we can cache for longer
  - [ ] if the call is something simple like "symbol" or "decimals", cache that too
  - [ ] when we receive a block, we should store it for later eth_getBlockByNumber, eth_blockNumber, and similar calls
- [ ] if a rpc fails to connect at start, retry later instead of skipping it forever
- [ ] inspect any jsonrpc errors. if its something like "header not found" or "block with id $x not found" retry on another node (and add a negative score to that server)
  - this error seems to happen when we use load balanced backend rpcs like pokt and ankr
- [ ] when block subscribers receive blocks, store them in a cache. use this cache instead of querying eth_getBlock
- [ ] if the fastest server has hit rate limits, we won't be able to serve any traffic until another server is synced.
  - thundering herd problem if we only allow a lag of 0 blocks
  - we can fix this by only `publish`ing the sorted list once a threshold of total soft limits is passed 
- [ ] emit stats for successes, retries, failures, with the types of requests, account, chain, rpc
- [ ] automated soft limit
  - look at average request time for getBlock? i'm not sure how good a proxy that will be for serving eth_call, but its a start
- [x] if we send a transaction to private rpcs and then people query it on public rpcs things, some interfaces might think the transaction is dropped (i saw this happen in a brownie script of mine). how should we handle this?
  - send getTransaction rpc requests to the private rpc tier
- [ ] don't "unwrap" anywhere. give proper errors

## V2

- [ ] make it easy to add or remove providers while the server is running
- [ ] ethers has a transactions_unsorted httprpc method that we should probably use. all rpcs probably don't support it, so make it okay for that to fail
- [ ] if chain split detected, don't send transactions?
- [ ] have a "backup" tier that is only used when the primary tier has no servers or is multiple blocks behind. we don't want the backup tier taking over with the head block if they happen to be fast at that (but overall low/expensive rps). only if the primary tier has fallen behind or gone entirely offline should we go to third parties
- [ ] more advanced automated soft limit
  - measure average latency of a node's responses and load balance on that

## "Maybe some day" and other Miscellaneous Things

- [ ] instead of giving a rate limit error code, delay the connection's response at the start. reject if incoming requests is super high?
- [ ] add the backend server to the header?
- [ ] think more about how multiple rpc tiers should work
- maybe always try at least two servers in parallel? and then return the first? or only if the first one doesn't respond very quickly? this doubles our request load though.
- [ ] one proxy for multiple chains?
- [ ] zero downtime deploys
- [ ] are we using Acquire/Release/AcqRel properly? or do we need other modes?
- [x] subscription id should be per connection, not global
- [ ] use https://github.com/ledgerwatch/interfaces to talk to erigon directly instead of through erigon's rpcdaemon (possible example code which uses ledgerwatch/interfaces: https://github.com/akula-bft/akula/tree/master)
- [ ] subscribe to pending transactions and build an intelligent gas estimator
- [ ] include private rpcs with regular queries? i don't want to overwhelm them, but they could be good for excess load
