FROM rust:1-buster as builder

WORKDIR /usr/src/web3-proxy
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/web3-proxy/target \
    cargo install --path ./web3-proxy

FROM debian:buster-slim
# RUN apt-get update && apt-get install -y extra-runtime-dependencies && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/cargo/bin/web3-proxy /usr/local/bin/web3-proxy
CMD ["web3-proxy"]
