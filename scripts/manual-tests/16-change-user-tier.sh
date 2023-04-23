# rm -rf data/
# docker-compose up -d
# sea-orm-cli migrate up

# Use CLI to create the admin that will call the endpoint
cargo run create_user --address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
cargo run change_admin_status 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a true

# Use CLI to create the user whose role will be changed via the endpoint
cargo run create_user --address 0x077e43dcca20da9859daa3fd78b5998b81f794f7

# Run the proxyd instance
cargo run --release -- proxyd

# Check if the instance is running
curl --verbose -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544

curl --verbose -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params": ["latest", false], "id":1}' 127.0.0.1:8544

# Open this website to get the nonce to log in
curl -X GET "http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a"

# Use this site to sign a message
# https://www.myetherwallet.com/wallet/sign (whatever is output with the above code)
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{"address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a", "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a2030314753414e37464d47574335314e50544737343338384a44350a4973737565642041743a20323032332d30322d31355431333a34363a33372e3037323739335a0a45787069726174696f6e2054696d653a20323032332d30322d31355431343a30363a33372e3037323739335a", "sig": "2d2eb576b2e6d05845710b7229f2a1ff9707e928fdcf571d1ce0ae094577e4310873fa1376c69440b60d6a1c76c62a4586b9d6426fb6559dee371e490d708f3e1b", "version": "3", "signer": "MEW"}'

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
-H "Authorization: Bearer 01GSANKVBB22D5P2351P4Y42NV" \
-X GET "127.0.0.1:8544/admin/modify_role?user_address=0x077e43dcca20da9859daa3fd78b5998b81f794f7&user_tier_title=Unlimited&user_id=1"

