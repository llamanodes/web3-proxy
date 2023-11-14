FROM debian:bullseye-slim as mold

ENV SHELL /bin/bash
SHELL [ "/bin/bash", "-c" ]

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    set -eux -o pipefail; \
    \
    apt-get update; \
    apt-get install --no-install-recommends --yes \
    ca-certificates \
    git \
    ;

RUN { set -eux; \
    \
    git clone https://github.com/rui314/mold.git; \
    mkdir mold/build; \
    cd mold/build; \
    git checkout v2.3.3; \
    ../install-build-deps.sh; \
    cmake -DCMAKE_BUILD_TYPE=Release -DCMAKE_CXX_COMPILER=c++ ..; \
    cmake --build . -j $(nproc); \
    cmake --build . --target install; \
    }

COPY ./docker/cargo-config.toml /root/.cargo/config.toml

FROM debian:bullseye-slim as rust

WORKDIR /app
# sccache cannot cache incrementals, but we use --mount=type=cache and import caches so it should be helpful
ENV CARGO_INCREMENTAL true
ENV CARGO_TERM_COLOR always
ENV CARGO_UNSTABLE_SPARSE_REGISTRY true
ENV PATH "/root/.foundry/bin:/root/.cargo/bin:${PATH}"
ENV SHELL /bin/bash
SHELL [ "/bin/bash", "-c" ]

# install rustup dependencies
# install clang for mold
# also install web3-proxy system dependencies. most things are rust-only, but not everything
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    set -eux -o pipefail; \
    \
    apt-get update; \
    apt-get install --no-install-recommends --yes \
    build-essential \
    ca-certificates \
    clang \
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
    set -eux -o pipefail; \
    \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile=minimal

# run a cargo command to install our desired version of rust
# it is expected to exit code 101 since no Cargo.toml exists
# the rm is there because `cargo clean` can't run without a Cargo.toml, but a new version of rust likely needs a clean target dir
COPY rust-toolchain.toml ./
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/app/target \
    --mount=type=cache,target=/app/target_test \
    set -eux -o pipefail; \
    \
    cargo check || [ "$?" -eq 101 ]; \
    [ -e /app/target/rust-toolchain.toml ] && [ "$(cat /app/target/rust-toolchain.toml)" != "$(cat ./rust-toolchain.toml)" ] && rm -rf /app/target/*; \
    [ -e /app/target_test/rust-toolchain.toml ] && [ "$(cat /app/target_test/rust-toolchain.toml)" != "$(cat ./rust-toolchain.toml)" ] && rm -rf /app/target_test/*; \
    cp ./rust-toolchain.toml /app/target/rust-toolchain.toml; \
    cp ./rust-toolchain.toml /app/target_test/rust-toolchain.toml

# install mold (a faster linker)
COPY --link --from=mold /usr/local/bin/mold /usr/local/bin/mold
COPY --link --from=mold /root/.cargo/config.toml /root/.cargo/config.toml

# cargo binstall makes it fast to install binaries
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux -o pipefail; \
    \
    curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh >/tmp/install-binstall.sh; \
    bash /tmp/install-binstall.sh; \
    rm -rf /tmp/*

# flamegraph/tokio-console are used for debugging
FROM rust as rust_flamegraph

RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux -o pipefail; \
    \
    cargo binstall -y flamegraph

# FROM rust as rust_tokio_console
# RUN --mount=type=cache,target=/root/.cargo/git \
#     --mount=type=cache,target=/root/.cargo/registry \
#     set -eux -o pipefail; \
#     \
#     cargo binstall -y tokio-console

# nextest runs tests in parallel (done its in own FROM so that it can run in parallel)
# TODO: i'd like to use binaries for these, but i had trouble with arm and binstall
FROM rust as rust_nextest

RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux -o pipefail; \
    \
    cargo binstall -y cargo-nextest

# foundry/anvil are needed to run tests (done its in own FROM so that it can run in parallel)
FROM rust as rust_foundry

RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux -o pipefail; \
    \
    curl -L https://foundry.paradigm.xyz | bash && foundryup

FROM rust as rust_with_env

# changing our features doesn't change any of the steps above
# TODO: i think this should be an ARG
ENV WEB3_PROXY_FEATURES "stripe"

# copy the app
COPY . .

# fill the package caches
# TODO: clean needed because of rust upgrade and jenkins caches :'(
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    set -eux -o pipefail; \
    \
    [ -e "$(pwd)/payment-contracts/src/contracts/mod.rs" ] || touch "$(pwd)/payment-contracts/build.rs"; \
    cargo --locked fetch

# build tests (done its in own FROM so that it can run in parallel)
FROM rust_with_env as build_tests

COPY --link --from=rust_foundry /root/.foundry/bin/anvil /root/.foundry/bin/
COPY --link --from=rust_nextest /root/.cargo/bin/cargo-nextest* /root/.cargo/bin/

# test the application with cargo-nextest
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/app/target_test \
    set -eux -o pipefail; \
    \
    export CARGO_TARGET_DIR=target_test; \
    [ -e "$(pwd)/payment-contracts/src/contracts/mod.rs" ] || touch "$(pwd)/payment-contracts/build.rs"; \
    RUST_LOG=web3_proxy=trace,info \
    cargo \
    --frozen \
    --offline \
    nextest run \
    --features "$WEB3_PROXY_FEATURES" --no-default-features \
    ; \
    touch /test_success

FROM rust_with_env as build_app

# build the release application
# using a "release" profile (which install does by default) is **very** important
# TODO: use the "faster_release" profile which builds with `codegen-units = 1` (but compile is SLOW)
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/app/target \
    set -eux -o pipefail; \
    \
    [ -e "$(pwd)/payment-contracts/src/contracts/mod.rs" ] || touch "$(pwd)/payment-contracts/build.rs"; \
    cargo install \
    --features "$WEB3_PROXY_FEATURES" \
    --frozen \
    --offline \
    --no-default-features \
    --path ./web3_proxy_cli \
    --root /usr/local \
    ; \
    /usr/local/bin/web3_proxy_cli --help | grep 'Usage: web3_proxy_cli'

# copy this file so that docker actually creates the build_tests container
# without this, the runtime container doesn't need build_tests and so docker build skips it
COPY --link --from=build_tests /test_success /

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

ENV PATH "/root/.cargo/bin:${PATH}"

# TODO: lower log level when done with prototyping
ENV RUST_LOG "warn,ethers_providers::rpc=off,web3_proxy=debug,web3_proxy::rpcs::consensus=info,web3_proxy_cli=debug"

# we copy something from build_tests just so that docker actually builds it
COPY --link --from=rust_flamegraph /root/.cargo/bin/* /root/.cargo/bin/
COPY --link --from=build_app /usr/local/bin/* /usr/local/bin/

# make sure the app works
RUN web3_proxy_cli --help
