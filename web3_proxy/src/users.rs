use rand::prelude::*;
use uuid::{Builder, Uuid};

pub fn new_api_key() -> Uuid {
    // TODO: chacha20?
    let mut rng = thread_rng();

    let random_bytes = rng.gen();

    Builder::from_random_bytes(random_bytes).into_uuid()
}
