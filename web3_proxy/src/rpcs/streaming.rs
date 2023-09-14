use bytes::{Bytes, BytesMut};
use ethers::providers::ProviderError;
use reqwest::Response;
use serde::de::DeserializeOwned;

// TODO wrap response payload
/// Response that will be buffered if short enough, otherwise streamed
#[derive(Debug)]
pub enum StreamingResponse<T> {
    Buffer(T),
    Stream(Bytes, Response),
}

impl<T> StreamingResponse<T>
where
    T: DeserializeOwned,
{
    pub async fn read_if_short(mut response: Response, nbytes: u64) -> Result<Self, ProviderError> {
        match response.content_length() {
            Some(len) if len <= nbytes => Ok(Self::from_bytes(response.bytes().await?)?),
            Some(_) => Ok(StreamingResponse::Stream(Bytes::new(), response)),
            None => {
                let mut buf = BytesMut::new();
                while (buf.len() as u64) < nbytes {
                    match response.chunk().await? {
                        Some(chunk) => {
                            buf.extend_from_slice(&chunk);
                        }
                        None => return Ok(Self::from_bytes(buf.freeze())?),
                    }
                }
                Ok(StreamingResponse::Stream(buf.freeze(), response))
            }
        }
    }

    fn from_bytes(buf: Bytes) -> Result<Self, serde_json::Error> {
        let val = serde_json::from_slice(&buf)?;
        Ok(Self::Buffer(val))
    }

    pub fn into_inner(self) -> Option<T> {
        match self {
            Self::Buffer(val) => Some(val),
            Self::Stream(..) => None,
        }
    }

    /*
    pub async fn chunk(&mut self) -> reqwest::Result<Option<Bytes>> {
        match self {
            StreamingResponse::Buffer(buf) if buf.len() > 0 => {
                let mut rv = Bytes::new();
                std::mem::swap(buf, &mut rv);
                Ok(Some(rv))
            }
            StreamingResponse::Buffer(_buf) => Ok(None),
            StreamingResponse::Stream(_buf, response) => response.chunk().await,
        }
    }

    pub fn parse<T>(&self) -> serde_json::Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        match self {
            StreamingResponse::Buffer(buf) => serde_json::from_slice(buf),
            StreamingResponse::Stream(..) => Ok(None),
        }
    }
    */
}
