# A redis server with the libredis_cell module installed
FROM rust:1-bullseye as builder

WORKDIR /usr/src/redis-cell
RUN \
  --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/usr/src/web3-proxy/target \
  { \
  set -eux; \
  git clone -b v0.3.0 https://github.com/brandur/redis-cell .; \
  cargo build --release; \
  }

FROM redis:bullseye

COPY --from=builder /usr/src/redis-cell/target/release/libredis_cell.so /usr/lib/redis/modules/

CMD ["redis-server", "--loadmodule", "/usr/lib/redis/modules/libredis_cell.so"]
