//! sea-orm types don't always serialize how we want. this helps that, though it won't help every case.
use ethers::prelude::Address;
use sea_orm::prelude::Uuid;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::convert::TryInto;
use ulid::Ulid;

pub fn to_fixed_length<T, const N: usize>(v: Vec<T>) -> [T; N] {
    v.try_into()
        .unwrap_or_else(|v: Vec<T>| panic!("Expected a Vec of length {} but it was {}", N, v.len()))
}

pub fn vec_as_address<S>(x: &[u8], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let x = Address::from_slice(x);

    x.serialize(s)
}

pub fn address_to_vec<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let address = Address::deserialize(deserializer)?;

    Ok(address.to_fixed_bytes().into())
}

pub fn uuid_as_ulid<S>(x: &Uuid, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let x = Ulid::from(x.as_u128());

    // TODO: to_string shouldn't be needed, but i'm still seeing Uuid length
    x.to_string().serialize(s)
}

pub fn ulid_to_uuid<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
    let ulid = Ulid::deserialize(deserializer)?;

    Ok(ulid.into())
}
