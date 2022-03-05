# web3-proxy
quick and dirty proxy for ethereum nodes (or similar)

```
% cargo run -- --help
    Finished dev [unoptimized + debuginfo] target(s) in 0.04s
     Running `target/debug/eth-proxy --help`
Usage: eth-proxy --eth-primary-rpc <eth-primary-rpc> --eth-private-rpc <eth-private-rpc> [--listen-port <listen-port>]

Proxy Web3 Requests

Options:
  --eth-primary-rpc the primary Ethereum RPC server
  --eth-private-rpc the private Ethereum RPC server
  --listen-port     the private Ethereum RPC
  --help            display usage information

```
