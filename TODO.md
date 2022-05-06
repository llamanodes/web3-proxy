# Todo

- [ ] the ethermine rpc is usually fastest. but its in the private tier. since we only allow synced rpcs, we are going to not have an rpc a lot of the time
    - [ ] if not backends. return a 502 instead of delaying?
- [ ] tarpit ratelimiting at the start, but reject if incoming requests is super high
- [ ] thundering herd problem if we only allow a lag of 0 blocks. i don't see any solution besides allowing a one or two block lag
- [ ] add the backend server to the header?
- [ ] the web3proxyapp object gets cloned for every call. why do we need any arcs inside that? shouldn't they be able to connect to the app's?
