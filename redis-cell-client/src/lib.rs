mod client;

pub use client::RedisCellClient;
pub use redis;
// TODO: don't hard code MultiplexedConnection
pub use redis::aio::MultiplexedConnection;
pub use redis::Client;
