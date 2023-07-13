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
#cargo run create_user --address 0x762390ae7a3c4D987062a398C1eA8767029AB08E
# ULID: 01GXEDC66Z9RZE6AE22JE7FRAW
# UUID: 01875cd6-18df-4e3e-e329-c2149c77e15c

# Log in as the user so we can check the balance
# Open this website to get the nonce to log in
curl -X GET "http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a"

# Use this site to sign a message
# https://www.myetherwallet.com/wallet/sign (whatever is output with the above code)
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a203133370a4e6f6e63653a203031483538414a5148354a5931594437535252534d5354424d4d0a4973737565642041743a20323032332d30372d31335431393a31303a32342e3239333534325a0a45787069726174696f6e2054696d653a20323032332d30372d31335431393a33303a32342e3239333534325a",
        "sig": "9e5816b152b5f5527cd7e9f8f8aa7203cbe0aec6a8cc5e534b1e6e728d79ba1617cb8db1053a28cace8feb87304a00568292b8ee2a887f1956577186ae0988a31b",
        "version": "3",
        "signer": "MEW"
      }'

# bearer token is: 01H4YP4AW35DBMW9CXJXTE7MBA
# scret key is: 01H4YP4AVSZGZT0WXCSMMZ1MEH

###########################################
# Initially check balance, it should be 0
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01H4YP4AW35DBMW9CXJXTE7MBA" \
-X GET "127.0.0.1:8544/user/balance"


###########################################
# The user submits a transaction on the matic network
# and submits it on the endpoint
###########################################
curl \
-H "Authorization: Bearer 01H4YP4AW35DBMW9CXJXTE7MBA" \
-X POST "127.0.0.1:8544/user/balance/0x749788a5766577431a0a4fc8721fd7cb981f55222e073ed17976f0aba5e8818a"

###########################################
# Check the balance again, it should have increased according to how much USDC was spent
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01H4YP4AW35DBMW9CXJXTE7MBA" \
-X GET "127.0.0.1:8544/user/balance"

# Get the RPC key
curl \
  -X GET "127.0.0.1:8544/user/keys" \
  -H "Authorization: Bearer 01H4YP4AW35DBMW9CXJXTE7MBA"

## Check if calling an RPC endpoint logs the stats
## This one does already even it seems
for i in {1..100000}
do
  curl \
  -X POST "127.0.0.1:8544/rpc/01H4YP4AVSZGZT0WXCSMMZ1MEH" \
  -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'
done
