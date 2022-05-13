# web3-proxy

Web3-proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.

Signed transactions (eth_sendRawTransaction) are sent in parallel to the configured private RPCs (eden, ethermine, flashbots, etc.).

All other requests are sent to an RPC server on the latest block (alchemy, moralis, rivet, your own node, or one of many other providers). If multiple servers are in sync, they are prioritized by `active_requests/soft_limit`. Note that this means that the fastest server is most likely to serve requests and slow servers are unlikely to ever get any requests.

Each servers has different limits to configure. The `soft_limit` is the number of parallel active requests where a server starts to slow down. The `hard_limit` is where a server starts giving rate limits or other errors.

```
cargo run --release -- --help
```
```
   Compiling web3-proxy v0.1.0 (/home/bryan/src/web3-proxy/web3-proxy)
    Finished release [optimized] target(s) in 9.45s
     Running `target/release/web3-proxy --help`
Usage: web3-proxy [--listen-port <listen-port>] [--rpc-config-path <rpc-config-path>]

Web3-proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.

Options:
  --listen-port     what port the proxy should listen on
  --rpc-config-path path to a toml of rpc servers
  --help            display usage information
```

Start the server with the defaults (listen on `http://localhost:8544` and use `./config/example.toml` which proxies to a local websocket on 8546 and ankr's public ETH node):

```
cargo run --release
```

Check that the proxy is working:

```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544/eth
```

You can copy `config/example.toml` to `config/production-$CHAINNAME.toml` and then run `docker-compose up --build -d` start a proxies for many chains.

## Flame Graphs

Flame graphs make finding slow code painless:

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


Note: Testing with `getLatestBlockByNumber.lua` is not great because the latest block changes and so one run is likely to be very different than another.
