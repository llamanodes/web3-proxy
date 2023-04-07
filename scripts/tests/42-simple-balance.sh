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
RUSTFLAGS="--cfg tokio_unstable" cargo run create_user --address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a

# Log in as the user so we can check the balance
# Open this website to get the nonce to log in
curl -X GET "http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a"

# Use this site to sign a message
# https://www.myetherwallet.com/wallet/sign (whatever is output with the above code)
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{"address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a", "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a2030314753585241363458333153524843564252355733575441370a4973737565642041743a20323032332d30322d32325432333a34363a30352e3539385a0a45787069726174696f6e2054696d653a20323032332d30322d32335430303a30363a30352e3539385a", "sig": "7d796804e040ce00b0150e1d15b0d86ed53c32ee2b59270cc84d2d2c7c5adf34661db734832ffeabb219d29c6e931f738b8dfd2c820518bf68648d28bf3ed8211c", "version": "3", "signer": "MEW" }'


###########################################
# Initially check balance, it should be 0
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01GSXRC05VESBJ9H24N1H040JE" \
-X GET "127.0.0.1:8544/user/balance"


###########################################
# The user submits a transaction on the matic network
# and submits it on the endpoint
###########################################
curl \
-H "Authorization: Bearer 01GSXRC05VESBJ9H24N1H040JE" \
-X GET "127.0.0.1:8544/user/balance/0x55d1e6f3ae144445ddcaa115e77772b66c890e09cbc819f410f5082ce0f3a534"

###########################################
# Check the balance again, it should have increased according to how much USDC was spent
###########################################
# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01GSXRC05VESBJ9H24N1H040JE" \
-X GET "127.0.0.1:8544/user/balance"

# TODO: Now start using the RPC, balance should decrease


## Check if calling an RPC endpoint logs the stats
## This one does already even it seems
curl -X POST -H "Content-Type: application/json" --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}' 127.0.0.1:8544


# TODO: Now implement and test withdrawal

