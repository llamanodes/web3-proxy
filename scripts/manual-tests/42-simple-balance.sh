##################
# Run the server
##################
# Run the proxyd instance
cargo run --release -- proxyd

# Check if the instance is running
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544

##########################
# Create a User & Log in
##########################
cargo run create_user --address 0x762390ae7a3c4D987062a398C1eA8767029AB08E
# ULID: 01GXEDC66Z9RZE6AE22JE7FRAW
# UUID: 01875cd6-18df-4e3e-e329-c2149c77e15c

# Log in as the user so we can check the balance
# Open this website to get the nonce to log in
curl -X GET "http://127.0.0.1:8544/user/login/0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a"

# Use this site to sign a message
# https://www.myetherwallet.com/wallet/sign (whatever is output with the above code)
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a203031475a4b3634574459545830364544334652463546574135580a4973737565642041743a20323032332d30352d30345431303a35303a32342e3036333133365a0a45787069726174696f6e2054696d653a20323032332d30352d30345431313a31303a32342e3036333133365a",
        "sig": "61c39f15bef1595c11d1f8a3e782ffafd3411b808310be870833b7b5f744bcab7ce1ecbd0e0b7a98b41179a81aced52ab6620f9880fa2f5154ad978bcd422fe01c",
        "version": "3",
        "signer": "MEW"
      }'

# bearer token is: 01GZK65YRW69KZECCGPSQH2XYK
# scret key is: 01GZK65YNV0P0WN2SCXYTW3R9S

# 01GZH2PS89EJJY6V8JFCVTQ4BX
# 01GZH2PS7CTHA3TAZ4HXCTX6KQ

###########################################
# Initially check balance, it should be 0
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01GZK65YRW69KZECCGPSQH2XYK" \
-X GET "127.0.0.1:8544/user/balance"


###########################################
# The user submits a transaction on the matic network
# and submits it on the endpoint
###########################################
curl \
-H "Authorization: Bearer 01GZK65YRW69KZECCGPSQH2XYK" \
-X GET "127.0.0.1:8544/user/balance/0x749788a5766577431a0a4fc8721fd7cb981f55222e073ed17976f0aba5e8818a"

###########################################
# Check the balance again, it should have increased according to how much USDC was spent
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01GZGGDBMV0GM6MFBBHPDE78BW" \
-X GET "127.0.0.1:8544/user/balance"

# TODO: Now start using the RPC, balance should decrease

# Get the RPC key
curl \
  -X GET "127.0.0.1:8544/user/keys" \
  -H "Authorization: Bearer 01GZGGDBMV0GM6MFBBHPDE78BW" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'

## Check if calling an RPC endpoint logs the stats
## This one does already even it seems
for i in {1..100}
do
  curl \
  -X POST "127.0.0.1:8544/rpc/01GZK65YNV0P0WN2SCXYTW3R9S" \
  -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'
done


# TODO: Now implement and test withdrawal

