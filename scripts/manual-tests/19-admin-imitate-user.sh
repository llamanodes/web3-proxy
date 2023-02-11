# Admin can login as a user ... (but again, we must first have logged in
# docker-compose up -d
# rm -rf data/
# sea-orm-cli migrate up

RUSTFLAGS="--cfg tokio_unstable" cargo run create_user --address 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a
RUSTFLAGS="--cfg tokio_unstable" cargo run change_admin_status 0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a true

# Run the proxyd instance
# cargo run --release -- proxyd

# Check if the instance is running
# curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544

# Login as user first
curl -X GET "127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a"
#curl -X POST -H "Content-Type: application/json" --data '{}' 127.0.0.1:8544/user/login
curl -X GET "127.0.0.1:8544/user/login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a/"

# Now modify the user role and check this in the database
# Now we can get a bearer-token to imitate the user
curl -X GET "127.0.0.1:8544/admin/imitate-login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a"
#curl -X POST -H "Content-Type: application/json" --data '{}' 127.0.0.1:8544/user/login
curl -X GET "127.0.0.1:8544/admin/imitate-login/0xeB3E928A2E54BE013EF8241d4C9EaF4DfAE94D5a/"


# docker-compose down
