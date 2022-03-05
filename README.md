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
cargo run -- --eth-primary-rpc "https://your.favorite.provider"
```

```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","params":[],"id":67}' 127.0.0.1:8845/eth
```


## Todo

- [x] simple proxy
- [ ] proper logging
- [ ] load balance between multiple RPC servers
- [ ] support more than just ETH
- [ ] option to disable private rpc and send everything to primary
- [ ] health check nodes by block height
- [ ] measure latency to nodes
- [ ] Dockerfile
