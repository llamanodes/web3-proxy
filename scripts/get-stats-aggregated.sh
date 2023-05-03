# Make a get request to get the stats in an aggregated fashion

# I dont think we need a user id ...


curl -X GET \
"http://localhost:8544/user/stats/aggregate?query_start=1678780033&query_window_seconds=1000"

curl -X GET \
-H "Authorization: Bearer 01GZHND8E5BYRVPXXMKPQ75RJ1" \
"http://localhost:8544/user/stats/detailed?query_start=1678780033&query_window_seconds=1000"
