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
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078373632333930616537613363344439383730363261333938433165413837363730323941423038450a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a20303147584e4b57573636523852433135334e38594d4b353736520a4973737565642041743a20323032332d30342d31305431323a35373a34362e3935303939335a0a45787069726174696f6e2054696d653a20323032332d30342d31305431333a31373a34362e3935303939335a",
        "sig": "38521f41f6e0b01679181267d9ff3f6682e070623c1496f1cb62923bfb8df5c01584fc3323a4cdca2d651e96103d7b665f650eca52c5bb96bdbc80d2ce99ead21c",
        "version": "3",
        "signer": "MEW"
      }'

# bearer token is: 01GXNM02TW00APX79EYFCMM2VX
# scret key is: 01GXNKWJ50ZT068ER38MRJSC20

###########################################
# Initially check balance, it should be 0
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01GXNM02TW00APX79EYFCMM2VX" \
-X GET "127.0.0.1:8544/user/balance"


###########################################
# The user submits a transaction on the matic network
# and submits it on the endpoint
###########################################
curl \
-H "Authorization: Bearer 01GXNM02TW00APX79EYFCMM2VX" \
-X GET "127.0.0.1:8544/user/balance/0xda41f748106d2d1f1bf395e65d07bd9fc507c1eb4fd50c87d8ca1f34cfd536b0"

###########################################
# Check the balance again, it should have increased according to how much USDC was spent
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01GXNM02TW00APX79EYFCMM2VX" \
-X GET "127.0.0.1:8544/user/balance"

# TODO: Now start using the RPC, balance should decrease


## Check if calling an RPC endpoint logs the stats
## This one does already even it seems
curl -X POST -H "Content-Type: application/json" --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}' 127.0.0.1:8544


# TODO: Now implement and test withdrawal

