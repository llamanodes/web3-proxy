FROM rust:1-bullseye as builder

ENV PATH /root/.foundry/bin:$PATH
RUN curl -L https://foundry.paradigm.xyz | bash && foundryup

WORKDIR /usr/src/web3_proxy
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/web3_proxy/target \
    cargo test &&\
    cargo install --locked --root /opt/bin --path ./web3_proxy

FROM debian:bullseye-slim

COPY --from=builder /opt/bin/* /usr/local/bin/
ENTRYPOINT ["web3_proxy"]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "web3_proxy=debug"
