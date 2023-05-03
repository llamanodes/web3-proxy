### Tests subuser premium account endpoints
##################
# Run the server
##################
# Run the proxyd instance
cargo run --release -- proxyd

# Check if the instance is running
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544


##################
# Create the premium / primary user & log in (Wallet 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a)
##################
cargo run create_user --address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a

# Make user premium, so he can create subusers
cargo run change_user_tier_by_address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a "Unlimited"
# could also use CLI to change user role
# ULID 01GXRAGS5F9VJFQRVMZGE1Q85T
# UUID 018770a8-64af-4ee4-fbe3-74fc1c1ba0ba

# Open this website to get the nonce to log in, sign the message, and paste the payload in the endpoint that follows it
http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
https://www.myetherwallet.com/wallet/sign

http://127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
https://www.myetherwallet.com/wallet/sign

# Use this site to sign a message
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0x762390ae7a3c4d987062a398c1ea8767029ab08e",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078373632333930616537613363344439383730363261333938433165413837363730323941423038450a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a203031475a484e4350315a57345134305a384b4e4e304454564a320a4973737565642041743a20323032332d30352d30335432303a33383a31392e3435363231345a0a45787069726174696f6e2054696d653a20323032332d30352d30335432303a35383a31392e3435363231345a",
        "sig": "82d2ee89fb6075bdc57fa66db4e0b2b84ad0b6515e1b3d71bb1dd4e6f1711b2f0f6b5f5e40116fd51e609bc8b4c0642f4cdaaf96a6c48e66093fe153d4e2873f1c",
        "version": "3",
        "signer": "MEW"
      }'

# Bearer token is: 01GZHMCXHXHPGAABAQQTXKMSM3
# RPC secret key is: 01GZHMCXGXT5Z4M8SCKCMKDAZ6

# 01GZHND8E5BYRVPXXMKPQ75RJ1
# 01GZHND83W8VAHCZWEPP1AA24M

# Top up the balance of the account
curl \
-H "Authorization: Bearer 01GZHMCXHXHPGAABAQQTXKMSM3" \
-X GET "127.0.0.1:8544/user/balance/0x749788a5766577431a0a4fc8721fd7cb981f55222e073ed17976f0aba5e8818a"


# Make an example RPC request to check if the tokens work
curl \
  -X POST "127.0.0.1:8544/rpc/01GZHMCXGXT5Z4M8SCKCMKDAZ6" \
  -H "Content-Type: application/json" \
  --data '{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}'

##################
# Now act as the subuser (Wallet 0x762390ae7a3c4D987062a398C1eA8767029AB08E)
# We first login the subuser
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

##################
# Now the primary user adds the secondary user as a subuser
##################
# Get first users RPC keys
curl \
-H "Authorization: Bearer 01GXRB6AHZSXFDX2S1QJPJ8X51" \
-X GET "127.0.0.1:8544/user/keys"

# Secret key
curl \
  -X GET "127.0.0.1:8544/user/subuser?subuser_address=0x762390ae7a3c4D987062a398C1eA8767029AB08E&rpc_key=01GZHMCXGXT5Z4M8SCKCMKDAZ6&new_status=upsert&new_role=admin" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer 01GZHMCXHXHPGAABAQQTXKMSM3"

# The primary user can check what subusers he gave access to
curl \
  -X GET "127.0.0.1:8544/user/subusers?rpc_key=01GZHMCXGXT5Z4M8SCKCMKDAZ6" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer 01GZHMCXHXHPGAABAQQTXKMSM3"

# The secondary user can see all the projects that he is associated with
curl \
  -X GET "127.0.0.1:8544/subuser/rpc_keys" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer 01GXRFKFQXDV0MQ2RT52BCPZ23"

# Secret key
curl \
  -X GET "127.0.0.1:8544/user/subuser?subuser_address=0x762390ae7a3c4D987062a398C1eA8767029AB08E&rpc_key=01GXRFKFPY5DDRCRVB3B3HVDYK&new_status=remove&new_role=collaborator" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer 01GXRFKFQXDV0MQ2RT52BCPZ23"