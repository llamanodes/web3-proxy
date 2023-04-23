##################
# Run the server
##################

# Keep the proxyd instance running the background (and test that it works)
cargo run --release -- proxyd

# Check if the instance is running
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544

##################
# Create the referring user & log in (Wallet 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a)
##################
cargo run create_user --address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a

# Make user premium, so he can create referral keys
cargo run change_user_tier_by_address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a "Unlimited"
# could also use CLI to change user role
# ULID 01GXRAGS5F9VJFQRVMZGE1Q85T
# UUID 018770a8-64af-4ee4-fbe3-74fc1c1ba0ba

# Open this website to get the nonce to log in, sign the message, and paste the payload in the endpoint that follows it
http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
https://www.myetherwallet.com/wallet/sign

# Use this site to sign a message
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a2030314758524235424a584b47535845454b5a314438424857565a0a4973737565642041743a20323032332d30342d31315431343a32323a35302e3937333930365a0a45787069726174696f6e2054696d653a20323032332d30342d31315431343a34323a35302e3937333930365a",
        "sig": "be1f9fed3f6f206c15677b7da488071b936b68daf560715b75cf9232afe4b9923c2c5d00a558847131f0f04200b4b123011f62521b7b97bab2c8b794c82b29621b",
        "version": "3",
        "signer": "MEW"
      }'

# Bearer token is: 01GXRB6AHZSXFDX2S1QJPJ8X51
# RPC secret key is: 01GXRAGS5F9VJFQRVMZGE1Q85T

# Make an example RPC request to check if the tokens work
curl \
  -X POST "127.0.0.1:8544/rpc/01GXRAGS5F9VJFQRVMZGE1Q85T" \
  -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'

# Now retrieve the referral link
curl \
-H "Authorization: Bearer 01GXRB6AHZSXFDX2S1QJPJ8X51" \
-X GET "127.0.0.1:8544/user/referral"

# This is the referral code which will be used by the redeemer
# "llamanodes-01GXRB6RVM00MACTKABYVF8MJR"

##################
# Now act as the referrer (Wallet 0x762390ae7a3c4D987062a398C1eA8767029AB08E)
# We first login the referrer
# Using the referrer code creates an entry in the table
##################
# Login using the referral link. This should create the user, and also mark him as being referred
# http://127.0.0.1:8544/user/login/0x762390ae7a3c4D987062a398C1eA8767029AB08E
# https://www.myetherwallet.com/wallet/sign
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0x762390ae7a3c4d987062a398c1ea8767029ab08e",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078373632333930616537613363344439383730363261333938433165413837363730323941423038450a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a20303147585246454b5654334d584531334b5956443159323853460a4973737565642041743a20323032332d30342d31315431353a33373a34382e3636373438315a0a45787069726174696f6e2054696d653a20323032332d30342d31315431353a35373a34382e3636373438315a",
        "sig": "1784c968fdc244248a4c0b8d52158ff773e044646d6e5ce61d457679d740566b66fd16ad24777f09c971e2c3dfa74966ffb8c083a9bef2a527e49bc3770713431c",
        "version": "3",
        "signer": "MEW",
        "referral_code": "llamanodes-01GXRB6RVM00MACTKABYVF8MJR"
      }'

# Bearer token 01GXRFKFQXDV0MQ2RT52BCPZ23
# RPC key 01GXRFKFPY5DDRCRVB3B3HVDYK

# Make some requests, the referrer should not receive any credits for this (balance table is not created for free-tier users ...) This works fine
for i in {1..1000}
do
  curl \
    -X POST "127.0.0.1:8544/rpc/01GXRFKFPY5DDRCRVB3B3HVDYK" \
    -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'
done

###########################################
# Now the referred user deposits some tokens
# They then send it to the endpoint
###########################################
curl \
-H "Authorization: Bearer 01GXRFKFQXDV0MQ2RT52BCPZ23" \
-X GET "127.0.0.1:8544/user/balance/0xda41f748106d2d1f1bf395e65d07bd9fc507c1eb4fd50c87d8ca1f34cfd536b0"

curl \
-H "Authorization: Bearer 01GXRFKFQXDV0MQ2RT52BCPZ23" \
-X GET "127.0.0.1:8544/user/balance/0xd56dee328dfa3bea26c3762834081881e5eff62e77a2b45e72d98016daaeffba"


###########################################
# Now the referred user starts spending the money. Let's make requests worth $100 and see what happens ...
# At all times, the referrer should receive 10% of the spent tokens
###########################################
for i in {1..10000000}
do
  curl \
    -X POST "127.0.0.1:8544/rpc/01GXRFKFPY5DDRCRVB3B3HVDYK" \
    -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'
done

# Check that the new user was indeed logged in, and that a referral table entry was created (in the database)
# Check that the 10% referral rate works
