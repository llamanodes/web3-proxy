# docker-compose up -d
# rm -rf data/
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

# Login in the user first (add a random bearer token into the database)
# (This segment was not yet tested, but should next time you run the query)
INSERT INTO login (bearer_token, user_id, expires_at, read_only) VALUES (
  "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
  1,
  "2222-01-01",
  FALSE
);

#curl -X POST -H "Content-Type: application/json" --data '{}' 127.0.0.1:8544/user/login
#curl -X GET "127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a/"
#curl -X GET "127.0.0.1:8544/admin/modify_role?user_address=0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a&user_tier_title=Unlimited"

# Now modify the user role and check this in the database
curl \
-H "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9" \
-X GET "127.0.0.1:8544/admin/modify_role?user_address=0x077e43dcca20da9859daa3fd78b5998b81f794f7&user_tier_title=1&user_id=1"

curl \
-H "Authorization: Bearer QWxhZGRpbjpvcGVuIHNlc2FtZQ==" \
-X GET "127.0.0.1:8544/admin/modify_role?user_address=0x077e43dcca20da9859daa3fd78b5998b81f794f7&user_tier_title=Unlimited&user_id=1"
