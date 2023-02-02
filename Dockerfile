#
# cargo-chef
# We only pay the installation cost once, 
# it will be cached from the second build onwards
#
FROM rust:1-bullseye AS chef 

WORKDIR /app

# # cargo binstall provides a low-complexity mechanism for installing rust binaries as an alternative to building from source (via cargo install) or manually downloading packages.
# # This is intended to work with existing CI artifacts and infrastructure, and with minimal overhead for package maintainers.
# # TODO: more mount type cache?
# # TODO: this works on some architectures, but is failing on others.
# RUN --mount=type=cache,target=/usr/local/cargo/registry \
#     cargo install cargo-binstall

# cache the dependencies of your Rust project and speed up your Docker builds. 
# TODO: more mount type cache?
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-chef

# a next-generation test runner for Rust projects.
# TODO: more mount type cache?
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-nextest

#
# prepare examines your project and builds a recipe that captures the set of information required to build your dependencies.
# The recipe.json is the equivalent of the Python requirements.txt file - it is the only input required for cargo chef cook, the command that will build out our dependencies:
#
FROM chef AS prepare
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo chef prepare --recipe-path recipe.json

#
# test and build web3-proxy
#
FROM chef AS builder

# foundry is needed to run tests
ENV PATH /root/.foundry/bin:$PATH
RUN curl -L https://foundry.paradigm.xyz | bash && foundryup

# copy dependency config from the previous step
COPY --from=prepare /app/recipe.json recipe.json
# Build dependencies - this is the caching Docker layer!
# TODO: if the main app does `codegen-units = 1`, how much time is this actually saving us?
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo chef cook --release --recipe-path recipe.json

# copy the application
COPY . .

# test the application with cargo-nextest
ENV CARGO_TERM_COLOR always
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo nextest run

# build the application
# using a "release" profile (which install does) is **very** important
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo install --locked --no-default-features --profile faster_release --root /opt/bin --path ./web3_proxy

#
# We do not need the Rust toolchain to run the binary!
#
FROM debian:bullseye-slim AS runtime

ENTRYPOINT ["web3_proxy_cli"]
CMD [ "--config", "/web3-proxy.toml", "proxyd" ]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "warn,web3_proxy=debug,web3_proxy_cli=debug"

COPY --from=builder /opt/bin/* /usr/local/bin/
