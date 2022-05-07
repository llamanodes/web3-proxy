# web3-proxy
quick and dirty proxy for ethereum rpcs (or similar)

Signed transactions are sent to the configured private RPC (eden, flashbots, etc.). All other requests are sent to an RPC server on the latest block (alchemy, moralis, rivet, your own node, or one of many other providers).

```
cargo run --release -- --help
```
```
    Finished release [optimized] target(s) in 0.13s
     Running `target/release/web3-proxy --help`
Usage: web3-proxy [--listen-port <listen-port>] [--rpc-config-path <rpc-config-path>]

Reach new heights.

Options:
  --listen-port     what port the proxy should listen on
  --rpc-config-path what port the proxy should listen on
  --help            display usage information
```

```
cargo run --release
```

```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","params":[],"id":67}' 127.0.0.1:8544/eth
```

## Flame Graphs

    $ cat /proc/sys/kernel/kptr_restrict
    1
    $ echo 0 |sudo tee /proc/sys/kernel/kptr_restrict
    0
    $ CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph


## Load Testing

Test the proxy:

    wrk -s ./data/wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8544
    wrk -s ./data/wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8544

Test geth:

    wrk -s ./data/wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8545
    wrk -s ./data/wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8545

Test erigon:

    wrk -s ./data/wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8945
    wrk -s ./data/wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8945


## Todo

- [x] simple proxy
- [x] better locking. when lots of requests come in, we seem to be in the way of block updates
- [ ] proper logging
- [x] load balance between multiple RPC servers
- [x] support more than just ETH
- [x] option to disable private rpc and send everything to primary
- [x] health check nodes by block height
- [ ] measure latency to nodes
- [x] Dockerfile
- [ ] testing getLatestBlockByNumber is not great because the latest block changes and so one run is likely to be different than another
- [ ] if a request gets a socket timeout, try on another server
  - maybe always try at least two servers in parallel? and then return the first? or only if the first one doesn't respond very quickly?
