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
- [x] automatically route to archive server when necessary
  - originally, no processing was done to params; they were just serde_json::RawValue. this is probably fastest, but we need to look for "latest" and count elements, so we have to use serde_json::Value
  - when getting the next server, filtering on "archive" isn't going to work well. need to check inner instead
- [x] if the requested block is ahead of the best block, return without querying any backend servers
- [x] http servers should check block at the very start
- [x] subscription id should be per connection, not global
- [x] when under load, i'm seeing "http interval lagging!". sometimes it happens when not loaded.
  - we were skipping our delay interval when block hash wasn't changed. so if a block was ever slow, the http provider would get the same hash twice and then would try eth_getBlockByNumber a ton of times
- [x] inspect any jsonrpc errors. if its something like "header not found" or "block with id $x not found" retry on another node (and add a negative score to that server)
  - this error seems to happen when we use load balanced backend rpcs like pokt and ankr
- [x] RESPONSE_CACHE_CAP in bytes instead of number of entries
- [x] if we don't cache errors, then in-flight request caching is going to bottleneck 
  - i think now that we retry header not found and similar, caching errors should be fine
- [x] RESPONSE_CACHE_CAP from config
- [x] web3_sha3 rpc command
- [x] test that launches anvil and connects the proxy to it and does some basic queries
  - [x] need to have some sort of shutdown signaling. doesn't need to be graceful at this point, but should be eventually
- [x] if the fastest server has hit rate limits, we won't be able to serve any traffic until another server is synced.
  - thundering herd problem if we only allow a lag of 0 blocks
  - we can improve this by only publishing the synced connections once a threshold of total available soft and hard limits is passed. how can we do this without hammering redis? at least its only once per block per server
  - [x] instead of tracking `pending_synced_connections`, have a mapping of where all connections are individually. then each change, re-check for consensus.
- [x] synced connections swap threshold set to 1 so that it always serves something
- [ ] basic request method stats

## V1

- [x] if the eth_call (or similar) params include a block, we can cache for that
- [x] when block subscribers receive blocks, store them in a block_map
- [x] eth_blockNumber without a backend request
- [x] if we send a transaction to private rpcs and then people query it on public rpcs things, some interfaces might think the transaction is dropped (i saw this happen in a brownie script of mine). how should we handle this?
  - [x] send getTransaction rpc requests to the private rpc tier
- [x] I'm hitting infura rate limits very quickly. I feel like that means something is very inefficient
  - whenever blocks were slow, we started checking as fast as possible
- [cancelled] eth_getBlockByNumber and similar calls served from the block map
  - will need all Block<TxHash> **and** Block<TransactionReceipt> in caches or fetched efficiently
  - so maybe we don't want this. we can just use the general request cache for these. they will only require 1 request and it means requests won't get in the way as much on writes as new blocks arrive.
- [ ] cli tool for managing users and resetting api keys
- [ ] incoming rate limiting by api key
- [ ] nice output when cargo doc is run
- [ ] if we request an old block, more servers can handle it than we currently use.
    - [ ] instead of the one list of just heads, store our intermediate mappings (rpcs_by_hash, rpcs_by_num, blocks_by_hash) in SyncedConnections. this shouldn't be too much slower than what we have now
    - [ ] remove the if/else where we optionally route to archive and refactor to require a BlockNumber enum
    - [ ] then check syncedconnections for the blockNum. if num given, use the cannonical chain to figure out the winning hash
    - [ ] this means if someone requests a recent but not ancient block, they can use all our servers, even the slower ones
- [ ] refactor so configs can change while running
  - create the app without applying any config to it
  - have a blocking future watching the config file and calling app.apply_config() on first load and on change
  - work started on this in the "config_reloads" branch. because of how we pass channels around during spawn, this requires a larger refactor.
- [ ] if a rpc fails to connect at start, retry later instead of skipping it forever
- [ ] synced connections swap threshold should come from config
  - if there are bad forks, we need to think about this more. keep backfilling until there is a common block, or just error? if the common block is old, i think we should error rather than serve data. that's kind of "downtime" but really its on the chain and not us. think about this more
- [ ] have a "backup" tier that is only used when the primary tier has no servers or is many blocks behind
  - we don't want the backup tier taking over with the head block if they happen to be fast at that (but overall low/expensive rps). only if the primary tier has fallen behind or gone entirely offline should we go to third parties
  - [ ] until this is done, an alternative is for infra to have a "failover" script that changes the configs to include a bunch of third party servers manually.
- [ ] stats when forks are resolved (and what chain they were on?)
- [ ] failsafe. if no blocks or transactions in some time, warn and reset the connection
- [ ] right now the block_map is unbounded. move this to redis and do some calculations to be sure about RAM usage
- [ ] emit stats for successes, retries, failures, with the types of requests, account, chain, rpc
- [ ] right now we send too many getTransaction queries to the private rpc tier and i are being rate limited by some of them. change to be serial and weight by hard/soft limit.  
- [ ] improved logging with useful instrumentation
- [ ] Got warning: "WARN subscribe_new_heads:send_block: web3_proxy::connection: unable to get block from https://rpc.ethermine.org: Deserialization Error: expected value at line 1 column 1. Response: error code: 1015". this is cloudflare rate limiting on fetching a block, but this is a private rpc. why is there a block subscription?
- [ ] basic transaction firewall
  - this is part done, but we need to pick a database schema to check
- [ ] cli for creating and editing api keys
- [ ] cli for blocking malicious contracts with the firewall
- [ ] Api keys need option to lock to IP, cors header, etc
- [ ] Only subscribe to transactions when someone is listening and if the server has opted in to it
- [ ] When sending eth_sendRawTransaction, retry errors
- [ ] I think block limit should default to 1 or error if its still at 0 after archive checks
  - [ ] Public bsc server got “0” for block data limit (ninicoin)
- [ ] If we need an archive server and no servers in sync, exit immediately with an error instead of waiting 60 seconds
- [ ] 60 second timeout is too short. Maybe do that for free tier and larger timeout for paid. Problem is that some queries can take over 1000 seconds

new endpoints for users:
- think about where to put this. a separate app might be better
- [ ] GET /user/login/$address
  - returns a JSON string for the user to sign
- [ ] POST /user/login/$address
  - returns a JSON string including the api key
  - sets session cookie
- [ ] GET /user/$address
  - checks for api key in session cookie or header
  - returns a JSON string including user stats
    - balance in USD 
    - deposits history (currency, amounts, transaction id)
    - number of requests used (so we can calculate average spending over a month, burn rate for a user etc, something like "Your balance will be depleted in xx days)
    - the email address of a user if he opted in to get contacted via email
    - all the success/retry/fail counts and latencies (but that may better come from somewhere else)
- [ ] POST /user/$address
  - opt-in link email address
  - checks for api key in session cookie or header
  - allows modifying user settings
- [ ] GET /$api_key
  - proxies to web3 websocket
- [ ] POST /$api_key
  - proxies to web3
- [ ] POST /users/process_transaction
  - checks a transaction to see if it modifies a user's balance. records results in a sql database
  - we will have our own event subscriber watching for "deposit" events, but sometimes events get missed and users might incorrectly "transfer" the tokens directly to an address instead of using the dapp

## V2

- [ ] sea-orm brings in async-std, but we are using tokio. benchmark switching 
- [ ] jwt auth so people can easily switch from infura
- [ ] handle log subscriptions
- [ ] most things that are cached locally should probably be in shared redis caches
- [ ] automated soft limit
  - look at average request time for getBlock? i'm not sure how good a proxy that will be for serving eth_call, but its a start
  - https://crates.io/crates/histogram-sampler
- [ ] interval for http subscriptions should be based on block time. load from config is easy, but better to query. currently hard coded to 13 seconds
- [ ] improve transaction firewall
- [ ] handle user payments
- [ ] Load testing script so we can find optimal cost servers 

in another repo: event subscriber
  - [ ] watch for transfer events to our contract and submit them to /payment/$tx_hash
  - [ ] cli tool that support can run to manually check and submit a transaction

## "Maybe some day" and other Miscellaneous Things

- [ ] search for all the "TODO" items in the code and move them here
- [ ] don't "unwrap" anywhere. give proper errors
- [ ] instead of giving a rate limit error code, delay the connection's response at the start. reject if incoming requests is super high?
- [ ] add the backend server to the header?
- [ ] have a low-latency option that always tries at least two servers in parallel and then returns the first success?
  - this doubles our request load though. maybe only if the first one doesn't respond very quickly? 
- [ ] zero downtime deploys
- [ ] graceful shutdown. stop taking new requests and don't stop until all outstanding queries are handled
  - https://github.com/tokio-rs/mini-redis/blob/master/src/shutdown.rs
- [ ] are we using Acquire/Release/AcqRel properly? or do we need other modes?
- [ ] use https://github.com/ledgerwatch/interfaces to talk to erigon directly instead of through erigon's rpcdaemon (possible example code which uses ledgerwatch/interfaces: https://github.com/akula-bft/akula/tree/master)
- [ ] subscribe to pending transactions and build an intelligent gas estimator
- [ ] flashbots specific methods
  - [ ] flashbots protect fast mode or not? probably fast matches most user's needs, but no reverts is nice.
  - [ ] https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#authentication maybe have per-user keys. or pass their header on if its set
- [ ] if no redis set, but public rate limits are set, exit with an error
- [ ] i saw "WebSocket connection closed unexpectedly" but no log about reconnecting
  - need better logs on this because afaict it did reconnect
- [ ] if archive servers are added to the rotation while they are still syncing, they might get requests too soon. keep archive servers out of the configs until they are done syncing. full nodes should be fine to add to the configs even while syncing, though its a wasted connection
- [ ] better document load tests: docker run --rm --name spam shazow/ethspam --rpc http://$LOCAL_IP:8544 | versus --concurrency=100 --stop-after=10000 http://$LOCAL_IP:8544; docker stop spam
- [ ] if the call is something simple like "symbol" or "decimals", cache that too. though i think this could bite us.
- [ ] add a subscription that returns the head block number and hash but nothing else
- [ ] if chain split detected, what should we do? don't send transactions?
- [ ] archive check works well for local servers, but public nodes (especially on other chains) seem to give unreliable results. likely because of load balancers. maybe have a "max block data limit"
- [ ] https://docs.rs/derive_builder/latest/derive_builder/
- [ ] Detect orphaned transactions
- [ ] https://crates.io/crates/reqwest-middleware easy retry with exponential back off
  - Though I think we want retries that go to other backends instead
- [ ] Some of the pub things should probably be "pub(crate)"
- [ ] Maybe storing pending txs on receipt in a dashmap is wrong. We want to store in a timer_heap (or similar) when we actually send. This way there's no lock contention until the race is over.
- [ ] Support "safe" block height. It's planned for eth2 but we can kind of do it now but just doing head block num-3
- [ ] Archive check on BSC gave “archive” when it isn’t. and FTM gave 90k for all servers even though they should be archive
- [ ] cache eth_getLogs in a database?
- [ ] stats for "read amplification". how many backend requests do we send compared to frontend requests we received?
- [ ] fully test retrying when "header not found"
  - i saw "header not found" on a simple eth_getCode query to a public load balanced bsc archive node on block 1
- [ ] weird flapping fork could have more useful logs. like, howd we get to 1/1/4 and fork. geth changed its mind 3 times?
  2022-07-22T23:52:18.593956Z  WARN block_receiver: web3_proxy::connections: chain is forked! 1 possible heads. 1/1/4 rpcs have 0xa906…5bc1 rpc=Web3Connection { url: "ws://127.0.0.1:8546", data: 64, .. } new_block_num=15195517
  2022-07-22T23:52:18.983441Z  WARN block_receiver: web3_proxy::connections: chain is forked! 1 possible heads. 1/1/4 rpcs have 0x70e8…48e0 rpc=Web3Connection { url: "ws://127.0.0.1:8546", data: 64, .. } new_block_num=15195517
  2022-07-22T23:52:19.350720Z  WARN block_receiver: web3_proxy::connections: chain is forked! 2 possible heads. 1/2/4 rpcs have 0x70e8…48e0 rpc=Web3Connection { url: "ws://127.0.0.1:8549", data: "archive", .. } new_block_num=15195517
  2022-07-22T23:52:26.041140Z  WARN block_receiver: web3_proxy::connections: chain is forked! 2 possible heads. 2/4/4 rpcs have 0x70e8…48e0 rpc=Web3Connection { url: "http://127.0.0.1:8549", data: "archive", .. } new_block_num=15195517
  - [ ] threshold should check actual available request limits (if any) instead of just the soft limit
- [ ] foreign key on_update and on_delete
- [ ] database creation timestamps
- [ ] better error handling. we warn too often for validation errors and use the same error code for most every request