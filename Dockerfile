FROM rust:1-bullseye as builder

# our app uses rust-tls, but the sentry crate only uses openssl
RUN set -eux; \
    apt-get update; \
    apt-get install -y libssl-dev; \
    rm -rf /var/lib/apt/lists/*

ENV PATH /root/.foundry/bin:$PATH
RUN curl -L https://foundry.paradigm.xyz | bash && foundryup

WORKDIR /usr/src/web3_proxy
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/web3_proxy/target \
    cargo test &&\
    cargo install --locked --no-default-features --root /opt/bin --path ./web3_proxy

FROM debian:bullseye-slim

# our app uses rust-tls, but the sentry crate only uses openssl
RUN set -eux; \
    apt-get update; \
    apt-get install -y libssl-dev; \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /opt/bin/* /usr/local/bin/

ENTRYPOINT ["web3_proxy_cli"]
CMD [ "--config", "/web3-proxy.toml", "proxyd" ]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "warn,web3_proxy=debug,web3_proxy_cli=debug"
