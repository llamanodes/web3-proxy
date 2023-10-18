# web3_proxy

Web3_proxy is a fast caching and load-balancing proxy designed for web3 (Ethereum or similar) JsonRPC servers.

**Under construction!** Please note that the code is currently under active development. If you wish to run the proxy yourself, please send us a message on Discord, and we can explain things that aren't documented yet. Most RPC methods are currently supported, though filters will be added soon. Additionally, more tests are always needed.

Signed transactions `(eth_sendRawTransaction)` are sent in parallel to the configured private RPCs (NeoC, Eden, BloxRoute, Flashbots, etc.).

All other requests are sent to an RPC server that is currently on the latest block (LlamaNodes, Alchemy, Moralis, Rivet, your node, or one of many other providers). If multiple servers are in sync, we prioritize servers based on their `active_requests` and request latency. Please keep in mind that this means that the fastest server is most likely to serve requests, while slower servers are unlikely to ever receive any requests.

Each server has different limits that can be configured. The `soft_limit` is the number of parallel active requests where a server starts to slow down, while the `hard_limit` is where a server starts giving rate limits or other errors.

## Quick development

1. `brew install librdkafka` or `sudo apt-get install librdkafka-dev`
2. Run `docker-compose up -d` to start the database and caches. See `docker-compose.yml` for details.
3. Copy `./config/example.toml` to `./config/development.toml` and change settings to match your setup.
4. Run `cargo` commands:

```
$ cargo run --release -- --help
```
```
   Compiling web3_proxy v0.1.0 (/home/bryan/src/web3_proxy/web3_proxy)
    Finished release [optimized + debuginfo] target(s) in 17.69s
     Running `target/release/web3_proxy --help`
Usage: web3_proxy [--port <port>] [--workers <workers>] [--config <config>]

web3_proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.

Options:
  --port            what port the proxy should listen on
  --workers         number of worker threads
  --config          path to a toml of rpc servers
  --help            display usage information
```

Start the server with the defaults (listen on `http://localhost:8544` and use `./config/development.toml` which uses the database and cache running under docker and proxies to a bunch of public nodes:

```
cargo run --release -- proxyd
```

Quickly run tests:

```
RUST_BACKTRACE=1 RUST_LOG=web3_proxy=trace,info cargo nextest run
```

Run more tests:

```
RUST_BACKTRACE=1 RUST_LOG=web3_proxy=trace,info cargo nextest run --features tests-needing-docker
```

## Mysql

Be sure to set `innodb_rollback_on_timeout=1`

## Influx

If running multiple web3-proxies connected to the same influxdb bucket, you **must** set `app.unique_id` to a globally unique value for each server!

`app.unique_id` defaults to 0 which will only work if you only have one server!

## Common commands

Create a user:

```
cargo run -- --db-url "$YOUR_DB_URL" create_user --address "$USER_ADDRESS_0x"
```

Check that the proxy is working:

```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544
```
```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_blockNumber","id":1}' 127.0.0.1:8544
```
```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_getBlockByNumber", "params": ["latest", false],"id":1}' 127.0.0.1:8544
```
```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_getBalance", "params": ["0x0000000000000000000000000000000000000000", "latest"],"id":1}' 127.0.0.1:8544
```

Check that the websocket is working:

```
$ websocat ws://127.0.0.1:8544

{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}

{"jsonrpc": "2.0", "id": 1, "method": "eth_subscribe", "params": ["newHeads"]}

{"jsonrpc": "2.0", "id": 1, "method": "eth_subscribe", "params": ["newPendingTransactions"]}
```

You can copy `config/example.toml` to `config/production-$CHAINNAME.toml` and then run `docker-compose up --build -d` start proxies for many chains.

Compare 3 RPCs:

```
web3_proxy_cli health_compass https://eth.llamarpc.com https://eth-ski.llamarpc.com https://rpc.ankr.com/eth
```

Manually process a deposit:

```
curl -X POST http://127.0.0.1:8544/user/balance/0xYOURTXID
```

### Run migrations

Generally it is simplest to just run the app to run migrations. It runs migrations on start.

But if you want to run them manually (generally only useful in development):

```
cd migration
cargo run up
```

### Create a user:

```
web3_proxy_cli --config ... create_user --address 0x0000000000000000000000000000000000000000 --email infra@llamanodes.com --description "..."
```

### Give a user unlimited requests per second:

Copy the ULID key (or UUID key) out of the above command, and put it into the following command.

```
web3_proxy_cli --config ... change_user_tier_by_key "$RPC_ULID_KEY_FROM_PREV_COMMAND" "Unlimited"
```

### Health compass

Health check 3 servers and error if the first one doesn't match the others.

```
web3_proxy_cli health_compass https://eth.llamarpc.com/ https://rpc.ankr.com/eth https://cloudflare-eth.com
```

## Adding new database tables

```
cargo install sea-orm-cli
```

1. (optional) drop the current dev db
2. `sea-orm-cli migrate`
3. `sea-orm-cli generate entity -u mysql://root:dev_web3_proxy@127.0.0.1:13306/dev_web3_proxy -o entities/src --with-serde both`
    - Be careful when adding the `--tables THE,MODIFIED,TABLES` flag. It will delete relationships if they aren't listed
4. After running the above, you will need to manually fix some things
    - Add any derives that got removed (like `Default`)
    - `Vec<u8>` -> `sea_orm::prelude::Uuid` (Related: <https://github.com/SeaQL/sea-query/issues/375>)
    - `i8` -> `bool` (Related: <https://github.com/SeaQL/sea-orm/issues/924>)
    - add all the tables back into `mod.rs`

## Flame Graphs

Flame graphs make a developer's join of finding slow code painless:

    $ cat /proc/sys/kernel/kptr_restrict
    1
    $ echo 0 | sudo tee /proc/sys/kernel/kptr_restrict
    0
    $ cat /proc/sys/kernel/perf_event_paranoid
    4
    $ echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid
    -1
    $ CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph --bin web3_proxy_cli --no-inline -- proxyd

Be sure to use `--no-inline` or perf will be VERY slow

## GDB

Developers can run the proxy under gdb for advanced debugging:

    cargo build --release && RUST_LOG=info,web3_proxy=debug,ethers_providers::rpc=off rust-gdb --args target/debug/web3_proxy --listen-port 7503 --rpc-config-path ./config/production-eth.toml

TODO: also enable debug symbols in the release build by modifying the root Cargo.toml

## Load Testing

Test the proxy:

    wrk -t12 -c400 -d30s --latency http://127.0.0.1:8544/health
    wrk -t12 -c400 -d30s --latency http://127.0.0.1:8544/status
    wrk -s ./wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8544/u/$API_KEY
    wrk -s ./wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8544/u/$API_KEY

Test geth (assuming it is on 8545):

    wrk -s ./wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8545
    wrk -s ./wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8545

Test erigon (assuming it is on 8945):

    wrk -s ./wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8945
    wrk -s ./wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8945

Note: Testing with `getLatestBlockByNumber.lua` is not great because the latest block changes and so one run is likely to be very different than another.

Run [ethspam](https://github.com/INFURA/versus) and [versus](https://github.com/shazow/ethspam) for a more realistic load test:

    ethspam --rpc http://127.0.0.1:8544 | versus --concurrency=10 --stop-after=1000 http://127.0.0.1:8544

    ethspam --rpc http://127.0.0.1:8544/u/$API_KEY | versus --concurrency=100 --stop-after=10000 http://127.0.0.1:8544/u/$API_KEY
