FROM rust:1-bullseye as builder

WORKDIR /usr/src/web3_proxy
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/web3_proxy/target \
    cargo test &&\
    cargo doc &&\
    cargo build --locked --release

FROM debian:bullseye-slim

COPY --from=builder /usr/src/web3_proxy/target/doc /usr/local/share/web3_proxy
COPY --from=builder /usr/src/web3_proxy/target/release/web3_proxy /usr/local/bin/web3_proxy
COPY --from=builder /usr/src/web3_proxy/target/release/web3_proxy_cli /usr/local/bin/web3_proxy_cli
ENTRYPOINT ["web3_proxy"]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "web3_proxy=debug"
