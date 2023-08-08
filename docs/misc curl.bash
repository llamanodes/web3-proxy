#!/bin/bash
set -eux -o pipefail

curl --verbose -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' http://127.0.0.1:8544/debug/$dev_rpc_key
curl --verbose -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_blockNumber","id":1}' http://127.0.0.1:8544/debug/$dev_rpc_key
curl --verbose -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_getBalance", "params": ["0x0000000000000000000000000000000000000000", "latest"],"id":1}' http://127.0.0.1:8544/debug/$dev_rpc_key

curl --verbose -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' https://eth.llamarpc.com/debug/$prod_rpc_key
curl --verbose -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_blockNumber","id":1}' https://eth.llamarpc.com/debug/$prod_rpc_key
curl --verbose -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_getBalance", "params": ["0x0000000000000000000000000000000000000000", "latest"],"id":1}' https://eth.llamarpc.com/debug/$prod_rpc_key

# TODO: what chain?
curl http://127.0.0.1:8544  -X POST \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_getLogs","params":[{"blockHash": "0x7c5a35e9cb3e8ae0e221ab470abae9d446c3a5626ce6689fc777dcffcab52c70", "topics":["0x241ea03ca20251805084d27d4440371c34a0b85ff108f6bb5611248f73818b80"]}],"id":1}'

curl http://127.0.0.1:8544  -X POST \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","id":1}'

# polygon
# {"jsonrpc":"2.0","id":2,"method":"eth_getLogs","params":[{"address":"0xbB35ef85FEF432bd918276A41dd6e04d1dBA5d42","fromBlock":"0x263e5b2","toBlock":"0x263e5bc","topics":["0x3de57033544efee8507d277ad65c808de56c31849180038a1e673db86d1c362d"]}

# eth
curl http://127.0.0.1:8544 \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_getLogs","params":[{"address": "0xdAC17F958D2ee523a2206206994597C13D831ec7"}],"id":1,"jsonrpc":"2.0"}'

curl https://eth.llamarpc.com/ \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_getLogs","params":[{"address": "0xdAC17F958D2ee523a2206206994597C13D831ec7"}],"id":1,"jsonrpc":"2.0"}'

curl http://127.0.0.1:8545/ \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_getLogs","params":[{"address": "0xdAC17F958D2ee523a2206206994597C13D831ec7"}],"id":1,"jsonrpc":"2.0"}'

curl http://127.0.0.1:8545 \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_getTransactionReceipt","params":["0x85d995eba9763907fdf35cd2034144dd9d53ce32cbec21349d4b12823c6860c5"],"id":1,"jsonrpc":"2.0"}'

curl https://eth.llamarpc.com \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_getTransactionReceipt","params":["0x85d995eba9763907fdf35cd2034144dd9d53ce32cbec21349d4b12823c6860c5"],"id":1,"jsonrpc":"2.0"}'


curl http://localhost:8544 \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{
  "id": 1,
  "jsonrpc": "2.0",
  "method": "eth_feeHistory",
  "params": [
    4,
    4,
    4
  ]
}'

curl https://docs-demo.quiknode.pro/ \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_feeHistory","params":[4, "latest", [25, 75]],"id":1,"jsonrpc":"2.0"}'

curl https://ethereum.llamarpc.com/ \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_feeHistory","params":[4, "latest", [25, 75]],"id":1,"jsonrpc":"2.0"}'

curl http://127.0.0.1:8544/ \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_feeHistory","params":[4, "latest", [25, 75]],"id":1,"jsonrpc":"2.0"}'

curl http://10.11.12.16:8548/ \
    -X POST \
    -H "Content-Type: application/json" \
    --data '{"method":"eth_feeHistory","params":[4, "latest", [25, 75]],"id":1,"jsonrpc":"2.0"}'


# --> [
#     {"jsonrpc": "2.0", "method": "sum", "params": [1,2,4], "id": "1"},
#     {"jsonrpc": "2.0", "method": "notify_hello", "params": [7]},
#     {"jsonrpc": "2.0", "method": "subtract", "params": [42,23], "id": "2"},
#     {"foo": "boo"},
#     {"jsonrpc": "2.0", "method": "foo.get", "params": {"name": "myself"}, "id": "5"},
#     {"jsonrpc": "2.0", "method": "get_data", "id": "9"} 
# ]
# <-- [
#     {"jsonrpc": "2.0", "result": 7, "id": "1"},
#     {"jsonrpc": "2.0", "result": 19, "id": "2"},
#     {"jsonrpc": "2.0", "error": {"code": -32600, "message": "Invalid Request"}, "id": null},
#     {"jsonrpc": "2.0", "error": {"code": -32601, "message": "Method not found"}, "id": "5"},
#     {"jsonrpc": "2.0", "result": ["hello", 5], "id": "9"}
# ]

# --> [1,2,3]
# <-- [
#     {"jsonrpc": "2.0", "error": {"code": -32600, "message": "Invalid Request"}, "id": null},
#     {"jsonrpc": "2.0", "error": {"code": -32600, "message": "Invalid Request"}, "id": null},
#     {"jsonrpc": "2.0", "error": {"code": -32600, "message": "Invalid Request"}, "id": null}
# ]
