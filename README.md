# web3-proxy
quick and dirty proxy for ethereum rpcs (or similar)

Signed transactions are sent to the configured private RPC (eden, flashbots, etc.). All other requests are sent to the configured primary RPC (alchemy, moralis, rivet, your own node, or one of many other providers).

```
cargo run -- --help
```
```
    Finished dev [unoptimized + debuginfo] target(s) in 0.04s
     Running `target/debug/eth-proxy --help`
Usage: eth-proxy --eth-primary-rpc <eth-primary-rpc> --eth-private-rpc <eth-private-rpc> [--listen-port <listen-port>]

Proxy Web3 Requests

Options:
  --eth-primary-rpc the primary Ethereum RPC server
  --eth-private-rpc the private Ethereum RPC server
  --listen-port     the port to listen on
  --help            display usage information
```

```
cargo run -r -- --eth-primary-rpc "https://your.favorite.provider"
```

```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","params":[],"id":67}' 127.0.0.1:8845/eth
```

## Flame Graphs

    $ cat /proc/sys/kernel/kptr_restrict
    1
    $ echo 0 |sudo tee /proc/sys/kernel/kptr_restrict
    0
    $ CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph


## Load Testing

Test the proxy:

    wrk -s ./data/wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8445
    wrk -s ./data/wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8445

Test geth:

    wrk -s ./data/wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8545
    wrk -s ./data/wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8545

Test erigon:

    wrk -s ./data/wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8945
    wrk -s ./data/wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8945


## Todo

- [x] simple proxy
- [ ] better locking. when lots of requests come in, we seem to be in the way of block updates
- [ ] proper logging
- [ ] load balance between multiple RPC servers
- [ ] support more than just ETH
- [ ] option to disable private rpc and send everything to primary
- [ ] health check nodes by block height
- [ ] measure latency to nodes
- [ ] Dockerfile
- [ ] testing getLatestBlockByNumber is not great because the latest block changes and so one run is likely to be different than another
- [ ] if a request gets a socket timeout, try on another server
  - maybe always try at least two servers in parallel? and then return the first? or only if the first one doesn't respond very quickly?
