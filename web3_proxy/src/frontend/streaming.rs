use axum::{body::BoxBody, response::IntoResponse};
use bytes::Bytes;
use futures::StreamExt;
use http::Response;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::stream::Stream;

struct SizingBody<B> {
    inner: B,
    request_metadata: RequestMetadata,
}

impl<B> SizingBody<B> {
    fn new(inner: B) -> Self {
        Self { inner, size: 0 }
    }
}

impl<B> Stream for SizingBody<B>
where
    B: Stream<Item = Result<Bytes, Box<dyn std::error::Error + Send + Sync>>> + Unpin,
{
    type Item = Result<Bytes, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.size += chunk.len();
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                println!("Final response size: {}", self.size);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
