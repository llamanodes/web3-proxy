use axum::{extract::Query, routing::get, Router};
use serde::{de, Deserialize, Deserializer};
use std::{fmt, str::FromStr};

/// Serde deserialization decorator to map empty Strings to None,
/// https://github.com/tokio-rs/axum/blob/1fe45583626a4c9c890cc01131d38c57f8728686/examples/query-params-with-empty-strings/src/main.rs
pub fn empty_string_as_none<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    let opt = Option::<String>::deserialize(de)?;
    match opt.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => FromStr::from_str(s).map_err(de::Error::custom).map(Some),
    }
}
