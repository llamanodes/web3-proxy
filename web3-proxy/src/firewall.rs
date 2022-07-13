//! transaction firewall
//!
//! block any transactions interacting with known malicious contracts
//!
//! this could be fancy and fetch abis and actually look for dangerous addresses
//! for now, it just checks a few commonly abused functions

use ethers::prelude::{Bytes, Transaction};
use ethers::utils::rlp;
use std::str::FromStr;

pub async fn check_firewall_raw(raw: &Bytes) -> anyhow::Result<bool> {
    let tx = rlp::decode(raw.as_ref())?;

    let is_allowed = check_firewall(tx).await;

    Ok(is_allowed)
}

pub async fn check_firewall(tx: Transaction) -> bool {
    match tx.to {
        None => return true,
        Some(to) => {
            // TODO: check our database for known malicious addresses
            if false {
                return false;
            }
        }
    }

    // TODO: do this better
    let approve_method = Bytes::from_str("0x9999999999").unwrap();
    let transfer_method = Bytes::from_str("0xa9059cbb").unwrap();
    let transfer_from_method = Bytes::from_str("0x9999999999").unwrap();
    let transfer_ownership_method = Bytes::from_str("0x9999999999").unwrap();

    match &tx.input.as_ref()[..4] {
        x if x == approve_method.as_ref() => {
            // TODO: decode the calldata
            if false {
                return false;
            }
            true
        }
        x if x == transfer_method.as_ref() => {
            // TODO: decode the calldata
            if false {
                return false;
            }
            true
        }
        x if x == transfer_from_method.as_ref() => {
            // TODO: decode the calldata
            if false {
                return false;
            }
            true
        }
        x if x == transfer_ownership_method.as_ref() => {
            // TODO: decode the calldata
            if false {
                return false;
            }
            true
        }
        _ => true,
    }
}
