FROM rust:1-bullseye as builder

WORKDIR /usr/src/web3-proxy
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/web3-proxy/target \
    cargo install --locked --path ./web3_proxy

FROM debian:bullseye-slim

COPY --from=builder /usr/local/cargo/bin/web3_proxy /usr/local/bin/web3_proxy
COPY --from=builder /usr/local/cargo/bin/web3_proxy_cli /usr/local/bin/web3_proxy_cli
ENTRYPOINT ["web3_proxy"]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "web3_proxy=debug"
