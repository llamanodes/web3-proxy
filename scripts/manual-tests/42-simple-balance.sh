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
curl -X GET "http://127.0.0.1:8544/user/login/0x762390ae7a3c4D987062a398C1eA8767029AB08E"

# Use this site to sign a message
# https://www.myetherwallet.com/wallet/sign (whatever is output with the above code)
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0x762390ae7a3c4d987062a398c1ea8767029ab08e",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078373632333930616537613363344439383730363261333938433165413837363730323941423038450a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a203031475a4747433532454356313031393746385a5a3350504b580a4973737565642041743a20323032332d30352d30335430393a35313a32342e3735303933345a0a45787069726174696f6e2054696d653a20323032332d30352d30335431303a31313a32342e3735303933345a",
        "sig": "57e0d720e2ad726f5c8b70a3591af3b2a4b0c3fbddb71a43a37731642f9c488a3b5e0b016eda5463707abdd3af50ddd82e682e28a150e63aad57d964ad5868191b",
        "version": "3",
        "signer": "MEW"
      }'

# bearer token is: 01GZGGDBMV0GM6MFBBHPDE78BW
# scret key is: 01GZGGDBKBVTQDFPGRJ753VSHX

###########################################
# Initially check balance, it should be 0
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01GZGGDBMV0GM6MFBBHPDE78BW" \
-X GET "127.0.0.1:8544/user/balance"


###########################################
# The user submits a transaction on the matic network
# and submits it on the endpoint
###########################################
curl \
-H "Authorization: Bearer 01GZGGDBMV0GM6MFBBHPDE78BW" \
-X GET "127.0.0.1:8544/user/balance/0xda41f748106d2d1f1bf395e65d07bd9fc507c1eb4fd50c87d8ca1f34cfd536b0"

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
for i in {1..1000}
do
  curl \
  -X POST "127.0.0.1:8544/rpc/01GZGGDBKBVTQDFPGRJ753VSHX" \
  -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'
done


# TODO: Now implement and test withdrawal

