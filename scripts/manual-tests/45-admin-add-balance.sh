
# Create / Login user1
curl -X GET "http://127.0.0.1:8544/user/login/0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a"
curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0xeb3e928a2e54be013ef8241d4c9eaf4dfae94d5a",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078654233453932384132453534424530313345463832343164344339456146344466414539344435610a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a203031483044573642334a48355a4b384a4e3947504d594e4d4b370a4973737565642041743a20323032332d30352d31345431393a33353a35352e3736323632395a0a45787069726174696f6e2054696d653a20323032332d30352d31345431393a35353a35352e3736323632395a",
        "sig": "f88b42d638246f8e51637c753052cab3a13b2a138faf3107c921ce2f0027d6506b9adcd3a7b72af830cdf50d20e6e9cb3f9f456dd1be47f6543990ea050909791c",
        "version": "3",
        "signer": "MEW"
      }'

# 01H0DW6VFCP365B9TXVQVVMHHY
# 01H0DVZNDJWQ7YG8RBHXQHJ301

# Make user1 an admin
cargo run change_admin_status 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a true

# Create/Login user2
curl -X GET "http://127.0.0.1:8544/user/login/0x762390ae7a3c4D987062a398C1eA8767029AB08E"

curl -X POST http://127.0.0.1:8544/user/login \
  -H 'Content-Type: application/json' \
  -d '{
        "address": "0x762390ae7a3c4d987062a398c1ea8767029ab08e",
        "msg": "0x6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078373632333930616537613363344439383730363261333938433165413837363730323941423038450a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a20303148304457384233304e534447594e484d33514d4a31434e530a4973737565642041743a20323032332d30352d31345431393a33373a30312e3238303338355a0a45787069726174696f6e2054696d653a20323032332d30352d31345431393a35373a30312e3238303338355a",
        "sig": "c545235557b7952a789dffa2af153af5cf663dcc05449bcc4b651b04cda57de05bcef55c0f5cbf6aa2432369582eb6a40927d14ad0a2d15f48fa45f32fbf273f1c",
        "version": "3",
        "signer": "MEW"
      }'

# 01H0DWPXRQA7XX2VFSNR02CG1N
# 01H0DWPXQQ951Y3R90QMF6MYGE

curl \
-H "Authorization: Bearer 01H0DWPXRQA7XX2VFSNR02CG1N" \
-X GET "127.0.0.1:8544/user/balance"


# Admin add balance
curl \
-H "Authorization: Bearer 01H0DW6VFCP365B9TXVQVVMHHY" \
-X GET "127.0.0.1:8544/admin/increase_balance?user_address=0x762390ae7a3c4D987062a398C1eA8767029AB08E&amount=100.0"
