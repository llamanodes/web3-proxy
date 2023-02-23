

# Use CLI to create the admin that will call the endpoint
RUSTFLAGS="--cfg tokio_unstable" cargo run create_user --address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
RUSTFLAGS="--cfg tokio_unstable" cargo run change_admin_status 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a true

# Use CLI to create the user whose role will be changed via the endpoint
RUSTFLAGS="--cfg tokio_unstable" cargo run create_user --address 0x077e43dcca20da9859daa3fd78b5998b81f794f7

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


##  Make user premium tier (unlimited, tier 2 or above (?))
##
##  Login admin
#
##  Make admin call "get referral code"

