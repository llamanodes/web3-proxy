FROM debian:bullseye-slim as rust

WORKDIR /app
ENV CARGO_UNSTABLE_SPARSE_REGISTRY true
ENV CARGO_TERM_COLOR always
ENV PATH "/root/.foundry/bin:/root/.cargo/bin:${PATH}"

# install rustup dependencies
# also install web3-proxy system dependencies. most things are rust-only, but not everything
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    set -eux; \
    \
    apt-get update; \
    apt-get install --no-install-recommends --yes \
    build-essential \
    ca-certificates \
    cmake \
    curl \
    git \
    liblz4-dev \
    libpthread-stubs0-dev \
    libsasl2-dev \
    libzstd-dev \
    make \
    pkg-config \
    ;

# install rustup
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux; \
    \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile=minimal

# run a cargo command to install our desired version of rust
# it is expected to exit code 101 since no Cargo.toml exists
COPY rust-toolchain.toml ./
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux; \
    \
    cargo check || [ "$?" -eq 101 ]

# nextest runs tests in parallel (done its in own FROM so that it can run in parallel)
FROM rust as rust_nextest
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux; \
    \
    cargo install --locked cargo-nextest

# foundry/anvil are needed to run tests (done its in own FROM so that it can run in parallel)
FROM rust as rust_foundry
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux; \
    \
    curl -L https://foundry.paradigm.xyz | bash && foundryup

FROM rust as rust_with_env

# changing our features doesn't change any of the steps above
ENV WEB3_PROXY_FEATURES "deadlock_detection,rdkafka-src"

# copy the app
COPY . .

# fetch deps
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/app/target \
    set -eux; \
    \
    [ -e "$(pwd)/payment-contracts/src/contracts/mod.rs" ] || touch "$(pwd)/payment-contracts/build.rs"; \
    cargo --locked --verbose fetch

# build tests (done its in own FROM so that it can run in parallel)
FROM rust_with_env as build_tests

COPY --from=rust_foundry /root/.foundry/bin/anvil /root/.foundry/bin/
COPY --from=rust_nextest /root/.cargo/bin/cargo-nextest* /root/.cargo/bin/

# test the application with cargo-nextest
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/app/target \
    set -eux; \
    \
    [ -e "$(pwd)/payment-contracts/src/contracts/mod.rs" ] || touch "$(pwd)/payment-contracts/build.rs"; \
    RUST_LOG=web3_proxy=trace,info \
    cargo \
    --frozen \
    --offline \
    --verbose \
    nextest run \
    --features "$WEB3_PROXY_FEATURES" --no-default-features \
    ; \
    touch /test_success

FROM rust_with_env as build_app

# # build the release application
# # using a "release" profile (which install does by default) is **very** important
# # TODO: use the "faster_release" profile which builds with `codegen-units = 1` (but compile is SLOW)
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/app/target \
    set -eux; \
    \
    [ -e "$(pwd)/payment-contracts/src/contracts/mod.rs" ] || touch "$(pwd)/payment-contracts/build.rs"; \
    cargo install \
    --features "$WEB3_PROXY_FEATURES" \
    --frozen \
    --no-default-features \
    --offline \
    --path ./web3_proxy \
    --root /usr/local \
    --verbose \
    ; \
    /usr/local/bin/web3_proxy_cli --help | grep 'Usage: web3_proxy_cli'

# copy this file so that docker actually creates the build_tests container
# without this, the runtime container doesn't need build_tests and so docker build skips it
COPY --from=build_tests /test_success /

#
# We do not need the Rust toolchain or any deps to run the binary!
#
FROM debian:bullseye-slim AS runtime

# Create llama user to avoid running container with root
RUN set -eux; \
    \
    mkdir /llama; \
    adduser --home /llama --shell /sbin/nologin --gecos '' --no-create-home --disabled-password --uid 1001 llama; \
    chown -R llama /llama

USER llama

ENTRYPOINT ["web3_proxy_cli"]
CMD [ "--config", "/web3-proxy.toml", "proxyd" ]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "warn,ethers_providers::rpc=off,web3_proxy=debug,web3_proxy::rpcs::consensus=info,web3_proxy_cli=debug"

# we copy something from build_tests just so that docker actually builds it
COPY --from=build_app /usr/local/bin/* /usr/local/bin/

# make sure the app works
RUN web3_proxy_cli --help
