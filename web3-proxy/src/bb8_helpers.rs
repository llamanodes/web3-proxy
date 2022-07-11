use redis_cell_client::bb8;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct RedisErrorSink;

impl bb8::ErrorSink<redis_cell_client::RedisError> for RedisErrorSink {
    fn sink(&self, err: redis_cell_client::RedisError) {
        warn!(?err, "redis error");
    }

    fn boxed_clone(&self) -> Box<dyn bb8::ErrorSink<redis_cell_client::RedisError>> {
        Box::new(self.clone())
    }
}
