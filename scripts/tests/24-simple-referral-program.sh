

# Use CLI to create the admin that will call the endpoint
cargo run create_user --address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
cargo run change_admin_status 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a true


# Run the proxyd instance
RUSTFLAGS="--cfg tokio_unstable" cargo run --release -- proxyd

# Check if the instance is running
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544

# Open this website to get the nonce to log in
curl -X GET "http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a"

# Use this site to sign a message
# https://www.myetherwallet.com/wallet/sign (whatever is output with the above code)
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{"address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a", "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a2030314753585241363458333153524843564252355733575441370a4973737565642041743a20323032332d30322d32325432333a34363a30352e3539385a0a45787069726174696f6e2054696d653a20323032332d30322d32335430303a30363a30352e3539385a", "sig": "7d796804e040ce00b0150e1d15b0d86ed53c32ee2b59270cc84d2d2c7c5adf34661db734832ffeabb219d29c6e931f738b8dfd2c820518bf68648d28bf3ed8211c", "version": "3", "signer": "MEW" }'



## Login in the user first (add a random bearer token into the database)
## (This segment was not yet tested, but should next time you run the query)
#INSERT INTO login (bearer_token, user_id, expires_at, read_only) VALUES (
#  "01GSAMZ6QY7KH9AQ",
#  1,
#  "2024-01-01",
#  FALSE
#);

#curl -X POST -H "Content-Type: application/json" --data '{}' 127.0.0.1:8544/user/login
#curl -X GET "127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a/"
#curl -X GET "127.0.0.1:8544/admin/modify_role?user_address=0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a&user_tier_title=Unlimited"

# Now modify the user role and check this in the database
curl \
-H "Authorization: Bearer 01GSXRC05VESBJ9H24N1H040JE" \
-X GET "127.0.0.1:8544/admin/modify_role?user_address=0x077e43dcca20da9859daa3fd78b5998b81f794f7&user_tier_title=Unlimited&user_id=1"




##  Create Docker
rm -rf data/
docker-compose up -d
RUSTFLAGS="--cfg tokio_unstable" cargo run --release -- proxyd
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544
##
##  Create User A and B. User A refers user B
RUSTFLAGS="--cfg tokio_unstable" cargo run create_user --address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
RUSTFLAGS="--cfg tokio_unstable" cargo run create_user --address 0x077e43dcca20da9859daa3fd78b5998b81f794f7

##
##  Top up balance of User A
curl -X GET "http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a"
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{"address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a", "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a20303147535846474d3235334254445944524e4e364a38474b434e0a4973737565642041743a20323032332d30322d32325432313a31323a31392e3236393435355a0a45787069726174696f6e2054696d653a20323032332d30322d32325432313a33323a31392e3236393435355a", "sig": "b3d1f62e0a10aa58e05ecdf7741a5d9f98fe5cac676eb95c64b8dafce015ebac02bf23db7a66e1305589dd18c0a7122b39d9dc55b0f3fc2d839115a6b9f671701c", "version": "3", "signer": "MEW"}'

# Bearer token is: 01GSXFKDV21Z33XCE6ZMAEYE56
# In the database, it is these bytes: 01 86 7A F9 B7 62 0F C6 B1 C6 FD 14 EF 38 A6

# Balance seems to be returning properly ...
curl \
-H "Authorization: Bearer 01GSXRC05VESBJ9H24N1H040JE" \
-X GET "127.0.0.1:8544/user/balance"

# Now let's test topping up balance
# This is apparently one that went through the approve message
curl \
-H "Authorization: Bearer 01GSXRC05VESBJ9H24N1H040JE" \
-X GET "127.0.0.1:8544/user/balance/0x55d1e6f3ae144445ddcaa115e77772b66c890e09cbc819f410f5082ce0f3a534"

curl \
-H "Authorization: Bearer 01GSXRC05VESBJ9H24N1H040JE" \
-X GET "127.0.0.1:8544/user/balance/0xcda6202f6d12e1a8d954d4aeda9e0bd622980151808b5c27d516ddce8aeff980"

## Check if calling an RPC endpoint logs the stats
## This one does already even it seems
curl -X POST -H "Content-Type: application/json" --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}' 127.0.0.1:8544

## Make the user into a premium user manually inside the database
## unlimited, tier 2 or above I assume
##
##  Make premium call "get referral code"
curl \
-H "Authorization: Bearer 01GSXRC05VESBJ9H24N1H040JE" \
-X GET "127.0.0.1:8544/user/referral"
# {"referral_code":"llamanodes-hE7T9HvAzvLXfPYrPD4iwbwCUlQoEgki","user":{"address":"0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a","description":null,"email":null,"id":1,"user_tier_id":4}}

## Now create, and login the second user
# Use CLI to create the user whose role will be changed via the endpoint
RUSTFLAGS="--cfg tokio_unstable" cargo run create_user --address 0x73fe2d0610FdD3a764C95904d6d257FA0d908f77

# Login on https://www.myetherwallet.com/wallet/sign
# Login with a referral code right away ...
curl -X GET "http://127.0.0.1:8544/user/login/0x73fe2d0610FdD3a764C95904d6d257FA0d908f77"
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{"address": "0x73fe2d0610fdd3a764c95904d6d257fa0d908f77", "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078373366653264303631304664443361373634433935393034643664323537464130643930386637370a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a2030314754365346483543474159325041564b345a3843343051450a4973737565642041743a20323032332d30322d32365431313a35393a33392e3138303936335a0a45787069726174696f6e2054696d653a20323032332d30322d32365431323a31393a33392e3138303936335a", "sig": "44032f2dc72b5f77380861ff471253c83ccd15a3d5cea7b5188a82cbbf5f38a2022001b4a17e0a988f09e39e1adb453cb93f5558adf1f87e9fd14d92ddc7ee701b", "version": "3", "signer": "MEW"}'

# UUID is here: 01GT6SSBJES845M3WXJQB3J4MR
# Now this guy can login with a referral code (again ...)
# I suppose he will get another
curl -X GET "http://127.0.0.1:8544/user/login/0x73fe2d0610FdD3a764C95904d6d257FA0d908f77"
curl -X POST "http://127.0.0.1:8544/user/login?referral_code=llamanodes-hE7T9HvAzvLXfPYrPD4iwbwCUlQoEgki" \
  -H 'Content-Type: application/json' \
  -d '{"address": "0x73fe2d0610fdd3a764c95904d6d257fa0d908f77", "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078373366653264303631304664443361373634433935393034643664323537464130643930386637370a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a203031475436575237414333544732435a373336413737524241320a4973737565642041743a20323032332d30322d32365431323a35363a34392e3734303939345a0a45787069726174696f6e2054696d653a20323032332d30322d32365431333a31363a34392e3734303939345a", "sig": "c37fc2bc2a155c582b6a4d1f9324d251e583fa2190489fdd1900d30b5583448021d72cd33614e7dbeee7cf56915dcefbd7155c8b1e3d08131106326bb7b610991c", "version": "3", "signer": "MEW"}'

#{"bearer_token":"01GT6XTS5F1YFJRHHB8070053A","rpc_keys":{"3":{"active":true,"allowed_ips":null,"allowed_origins":null,"allowed_referers":null,"allowed_user_agents":null,"description":null,"id":3,"log_level":"None","log_revert_chance":0.0,"private_txs":true,"secret_key":"01GT6SEWCV6G5EW0PV18Z37YC5","user_id":4}},"user":{"address":"0x73fe2d0610fdd3a764c95904d6d257fa0d908f77","description":null,"email":null,"id":4,"user_tier_id":1}}%                                                                                                                                                                                              davidal@student-net-nw-0959 web3-proxy %

# Getting referral code, now a third party needs to log in basically
# Ok, the referrer and referrals were registered, now we can spend items, and eventually credits will be applied to the referrer


