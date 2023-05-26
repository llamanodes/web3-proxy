FROM rust:1.69.0-bullseye AS builder

WORKDIR /app
ENV CARGO_TERM_COLOR always

# a next-generation test runner for Rust projects.
# We only pay the installation cost once, 
# it will be cached from the second build onwards
# TODO: more mount type cache?
# TODO: do this in a seperate FROM and COPY it in
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-nextest

# foundry is needed to run tests
# TODO: do this in a seperate FROM and COPY it in
ENV PATH /root/.foundry/bin:$PATH
RUN curl -L https://foundry.paradigm.xyz | bash && foundryup

# install web3-proxy system dependencies. most things are rust-only, but not everything
RUN apt-get update && \
    apt-get install --yes \
    cmake \
    liblz4-dev \
    libpthread-stubs0-dev \
    libsasl2-dev \
    libssl-dev \
    libzstd-dev \
    make \
    && \
    rm -rf /var/lib/apt/lists/*

# copy the application
COPY . .

ENV WEB3_PROXY_FEATURES "rdkafka-src"

# test the application with cargo-nextest
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo nextest run --features "$WEB3_PROXY_FEATURES" --no-default-features

# build the application
# using a "release" profile (which install does) is **very** important
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo install \
    --features "$WEB3_PROXY_FEATURES" \
    --locked \
    --no-default-features \
    --path ./web3_proxy \
    --profile faster_release \
    --root /usr/local/bin \
    ;

#
# We do not need the Rust toolchain to run the binary!
#
FROM debian:bullseye-slim AS runtime

# Create llama user to avoid running container with root
RUN mkdir /llama \
    && adduser --home /llama --shell /sbin/nologin --gecos '' --no-create-home --disabled-password --uid 1001 llama \
    && chown -R llama /llama

USER llama

ENTRYPOINT ["web3_proxy_cli"]
CMD [ "--config", "/web3-proxy.toml", "proxyd" ]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "warn,ethers_providers::rpc=off,web3_proxy=debug,web3_proxy_cli=debug"

COPY --from=builder /usr/local/bin/* /usr/local/bin/
