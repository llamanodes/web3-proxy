FROM rust:1-buster as builder

WORKDIR /usr/src/web3-proxy
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/web3-proxy/target \
    cargo install --path ./web3-proxy

FROM debian:buster-slim

COPY --from=builder /usr/local/cargo/bin/web3-proxy /usr/local/bin/web3-proxy
ENTRYPOINT ["web3-proxy"]
