use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use near_min_api::types::{AccountId, BlockHeight, CryptoHash};
use serde::{Deserialize, Serialize};

/// Storage of an account to follow, only keys that start with one of the prefixes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub account_id: AccountId,
    pub prefixes: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlockInfo {
    pub height: BlockHeight,
    pub hash: CryptoHash,
    pub timestamp_nanosec: u64,
}

/// Storage of an account after a block, only keys under the watched prefixes
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct AccountState {
    pub block: BlockInfo,
    pub entries: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl AccountState {
    pub fn with_prefix<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> + 'a {
        self.entries
            .range(prefix.to_vec()..)
            .take_while(move |(key, _)| key.starts_with(prefix))
    }
}
