##################
# Run the server
##################

# Keep the proxyd instance running the background (and test that it works)
cargo run --release -- proxyd

# Check if the instance is running
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544

##################
# Login 2 users
##################
http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
https://www.myetherwallet.com/wallet/sign

# Use this site to sign a message
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a20303148324435424e47584d423539394e3952314a3148575a444e0a4973737565642041743a20323032332d30362d30385430393a32383a31362e36363939335a0a45787069726174696f6e2054696d653a20323032332d30362d30385430393a34383a31362e36363939335a",
        "sig": "cb85bccbc12619a5b492817ae26f1daea1aff921c5a404c190c5cd371afda9cc299f2685e97c7ca996ab8c2cbb74b8e115a24251bd09a393b628490df65793161b",
        "version": "3",
        "signer": "MEW"
      }'

# Keys are:
# Bearer: 01H2D5CAQJF7P80222P4ZAFQ26
# RPC: 01H2D5CAP1KF2NKRS30SGATDSD

# Now log in the other user using a referral code
# Retrieve the referral link
curl \
-H "Authorization: Bearer 01H2D5CAQJF7P80222P4ZAFQ26" \
-X GET "127.0.0.1:8544/user/referral"

# Referral Code: 01H2D5CMPQKG36WTW1TM21SG28


http://127.0.0.1:8544/user/login/0x762390ae7a3c4D987062a398C1eA8767029AB08E
https://www.myetherwallet.com/wallet/sign
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0x762390ae7a3c4d987062a398c1ea8767029ab08e",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078373632333930616537613363344439383730363261333938433165413837363730323941423038450a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a20303148324435435a314150414e5a505950594d5735415945474d0a4973737565642041743a20323032332d30362d30385430393a32383a35392e3137383533385a0a45787069726174696f6e2054696d653a20323032332d30362d30385430393a34383a35392e3137383533385a",
        "sig": "af69a53b860bd327c6bb1299439aec70c330aaf9e26cd92c02f4bc2b48b4ebe74fd61b4388ba6ac6a6b83a5e313560d8bbd7877d9eeb416504a3dadca50113621c",
        "version": "3",
        "signer": "MEW",
        "referral_code": "01H2D5CMPQKG36WTW1TM21SG28"
      }'

# Keys are:
# Bearer: 01H2D5DN564M4Q2T6PETEZY83Q
# RPC: 01H2D5DN4D423VR2KFWBZE46TR

# Check if the referral entry has been made in the database: checked

# Make a deposit transaction for both parties, and mark them (it does not matter who calls the endpoints)
curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/balance/0x749788a5766577431a0a4fc8721fd7cb981f55222e073ed17976f0aba5e8818a"

curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/balance/0xd56dee328dfa3bea26c3762834081881e5eff62e77a2b45e72d98016daaeffba"

curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/balance/0xda41f748106d2d1f1bf395e65d07bd9fc507c1eb4fd50c87d8ca1f34cfd536b0"

curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/balance/0x12b38f3456ccb687ead8386c33071bedc23360931b9be672bb444b7ee1927bbe"
# This throws an error, because the amount that's provided is 10^(18+12). With USDC (18 decimal points), this would be above 1BN dollars.
# I suppose we won't accept shitcoins anytime soone.

curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/balance/0x81022efe36564d737af223e06b9a6c62f29ad7ce2f85dd99f1ea2f2e9a73306e"


# Check the balance for both users now
curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/balance"

curl \
-H "Authorization: Bearer 01H2D5CAQJF7P80222P4ZAFQ26" \
-X GET "127.0.0.1:8544/user/balance"

# Now we check if the referrals are triggered ...

# Referred user makes some requests
for i in {1..10000}
do
  curl \
    -X POST "127.0.0.1:8544/rpc/01H2D5CAP1KF2NKRS30SGATDSD" \
    -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'
done

# Let's also make simultaneous requests
for i in {1..10000}
do
  curl \
    -X POST "127.0.0.1:8544/rpc/01H2D5DN4D423VR2KFWBZE46TR" \
    -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'
done


# Get some data on the referral items
curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/referral/stats/used-codes"

curl \
-H "Authorization: Bearer 01H2D5CAQJF7P80222P4ZAFQ26" \
-X GET "127.0.0.1:8544/user/referral/stats/used-codes"


curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/referral/stats/shared-codes"

curl \
-H "Authorization: Bearer 01H2D5CAQJF7P80222P4ZAFQ26" \
-X GET "127.0.0.1:8544/user/referral/stats/shared-codes"


# Finally also get some stats
curl -X GET \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
"http://localhost:8544/user/stats/detailed?query_start=1686236378&query_window_seconds=3600"

curl -X GET \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
"http://localhost:8544/user/stats/aggregate?query_start=1686236378&query_window_seconds=3600"

curl -X GET \
"http://localhost:8544/user/stats/aggregate?query_start=1686772800&query_window_seconds=3600"

curl \
-H "Authorization: Bearer 01H2D5DN564M4Q2T6PETEZY83Q" \
-X GET "127.0.0.1:8544/user/referral/stats/used-codes"