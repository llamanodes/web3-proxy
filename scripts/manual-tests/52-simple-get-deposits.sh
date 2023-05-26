# Check the balance of the user
# Balance seems to be returning properly (0, in this test case)
curl \
-H "Authorization: Bearer 01GZHMCXHXHPGAABAQQTXKMSM3" \
-X GET "127.0.0.1:8544/user/deposits"
