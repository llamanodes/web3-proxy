#
# cargo-nextest
# We only pay the installation cost once, 
# it will be cached from the second build onwards
#
FROM rust:1.67.1-bullseye AS builder

WORKDIR /app
ENV CARGO_TERM_COLOR always

# a next-generation test runner for Rust projects.
# TODO: more mount type cache?
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-nextest

# foundry is needed to run tests
ENV PATH /root/.foundry/bin:$PATH
RUN curl -L https://foundry.paradigm.xyz | bash && foundryup

# copy the application
COPY . .

# test the application with cargo-nextest
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

# Create llama user to avoid running container with root
RUN mkdir /llama \
    && adduser --home /llama --shell /sbin/nologin --gecos '' --no-create-home --disabled-password --uid 1001 llama \
    && chown -R llama /llama

USER llama

ENTRYPOINT ["web3_proxy_cli"]
CMD [ "--config", "/web3-proxy.toml", "proxyd" ]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "warn,web3_proxy=debug,web3_proxy_cli=debug"

COPY --from=builder /opt/bin/* /usr/local/bin/
