# rm -rf data/
# docker-compose up -d
# sea-orm-cli migrate up

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
curl \
-H "Authorization: Bearer 01GSANKVBB22D5P2351P4Y42NV" \
-X GET "http://127.0.0.1:8544/admin/imitate-login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a/0x077e43dcca20da9859daa3fd78b5998b81f794f7"

# Use this site to sign a message
# https://www.myetherwallet.com/wallet/sign (whatever is output with the above code)
curl -X POST http://127.0.0.1:8544/admin/imitate-login \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer 01GSANKVBB22D5P2351P4Y42NV" \
  -d '{"address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a", "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a20303147534150545132413932415332435752563158504d4347470a4973737565642041743a20323032332d30322d31355431343a31343a33352e3835303636385a0a45787069726174696f6e2054696d653a20323032332d30322d31355431343a33343a33352e3835303636385a", "sig": "d5fed789e98769b8b726a79f222f2e06476de15948d35c167c4f294bb98edf42244edc703b6d729e5d08bd73c318fc9729b985022229c7669a945d64da47ab641c", "version": "3", "signer": "MEW"}'

# Now modify the user role and check this in the database
# 01GSAMMWQ41TVVH3DH8MSEP8X6
# Now we can get a bearer-token to imitate the user
curl \
-H "Authorization: Bearer 01GSAPZNVZ96ADJAEZ1VTRSA5T" \
-X GET "127.0.0.1:8544/user/keys"


# docker-compose down
