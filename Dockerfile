FROM debian:bullseye-slim as builder

WORKDIR /app
ENV CARGO_TERM_COLOR always
ENV PATH "/root/.foundry/bin:/root/.cargo/bin:${PATH}"

# install rustup dependencies
# also install web3-proxy system dependencies. most things are rust-only, but not everything
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    \
    apt-get update && \
    apt-get install --no-install-recommends --yes \
    build-essential \
    ca-certificates \
    cmake \
    curl \
    git \
    liblz4-dev \
    libpthread-stubs0-dev \
    libsasl2-dev \
    libssl-dev \
    libzstd-dev \
    make \
    pkg-config


# install rustup
RUN --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile=minimal

# run a cargo command which install our desired version of rust
COPY rust-toolchain.toml ./
RUN --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    \
    cargo check || [ "$?" -eq 101 ]

# a next-generation test runner for Rust projects.
# We only pay the installation cost once, 
# it will be cached from the second build onwards
# TODO: more mount type cache?
RUN --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    \
    cargo install --locked cargo-nextest 

# foundry is needed to run tests
# TODO: do this in a seperate FROM and COPY it in
RUN --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    \
    curl -L https://foundry.paradigm.xyz | bash && foundryup

ENV WEB3_PROXY_FEATURES "rdkafka-src,connectinfo"

FROM builder as build_tests

# test the application with cargo-nextest
RUN --mount=type=bind,target=.,rw \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target,sharing=private \
    \
    RUST_LOG=web3_proxy=trace,info cargo --locked nextest run --features "$WEB3_PROXY_FEATURES" --no-default-features && \
    touch /test_success

FROM builder as build_app

# build the application
# using a "release" profile (which install does by default) is **very** important
# TODO: use the "faster_release" profile which builds with `codegen-units = 1`
RUN --mount=type=bind,target=.,rw \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target,sharing=private \
    \
    cargo install \
    --features "$WEB3_PROXY_FEATURES" \
    --locked \
    --no-default-features \
    --path ./web3_proxy \
    --root /usr/local/bin \
    ;

# copy this file so that docker actually creates the build_tests container
# without this, the runtime container doesn't need build_tests and so docker build skips it
COPY --from=build_tests /test_success /

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
ENV RUST_LOG "warn,ethers_providers::rpc=off,web3_proxy=debug,web3_proxy::rpcs::consensus=info,web3_proxy_cli=debug"

# we copy something from build_tests just so that docker actually builds it
COPY --from=build_app /usr/local/bin/* /usr/local/bin/

# make sure the app works
RUN web3_proxy_cli --help
