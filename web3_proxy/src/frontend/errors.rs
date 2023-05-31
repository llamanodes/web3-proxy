use crate::errors::Web3ProxyError;
use axum::response::{IntoResponse, Response};

#[inline]
pub async fn handler_404() -> Response {
    Web3ProxyError::NotFound.into_response()
}
