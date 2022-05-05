# Todo

- [ ] tarpit ratelimiting at the start, but reject if incoming requests is super high
- [ ] thundering herd problem if we only allow a lag of 1 block. soft rate limits should help

# notes
its almost working. when i curl it, it doesn't work exactly right though

## first time:

    ```
    thread 'tokio-runtime-worker' panicked at 'not implemented', src/provider_tiers.rs:142:13
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
    ```

I think this is not seeing any as in sync. not sure why else it would not have any not_until set.
I believe this is because we don't know the first block. we should force an update or something at the start

## second time:
"false"

it loses all the "jsonrpc" parts and just has the simple result. need to return a proper jsonrpc response

# TODO: add the backend server to the header

# random thoughts:

the web3proxyapp object gets cloned for every call. why do we need any arcs inside that? shouldn't they be able to connect to the app's?

on friday i had it over 100k rps. but now, even when i roll back to that commit, i can't get it that high. what changed?

i think we need a top level head block. otherwise if tier0 stalls, we will keep using it