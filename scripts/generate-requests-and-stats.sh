# Got eth spam from here
# https://github.com/shazow/ethspam

# Got versus from here
# https://github.com/INFURA/versus
# ./ethspam | ./versus --stop-after 100 "http://localhost:8544/" # Pipe into the endpoint ..., add a bearer token and all that

./ethspam http://127.0.0.1:8544 | ./versus --stop-after 100 http://localhost:8544
