use std::collections::{BTreeMap, HashMap};

use borsh::BorshDeserialize;
use near_min_api::types::{AccountId, BlockHeight, CryptoHash};

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshDeserialize)]
pub struct BlockInfo {
    pub height: BlockHeight,
    pub hash: CryptoHash,
    pub timestamp_nanosec: u64,
}

/// Storage of an account after a block, only keys under the watched prefixes
#[derive(BorshDeserialize)]
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

pub(crate) struct WatchedAccount {
    pub prefixes: Vec<Vec<u8>>,
    pub entries: BTreeMap<Vec<u8>, Vec<u8>>,
}

pub(crate) struct Change {
    pub account_id: AccountId,
    pub key: Vec<u8>,
    /// `None` if deleted
    pub value: Option<Vec<u8>>,
}

/// Storage of watched accounts after the newest applied block
pub(crate) struct Storage {
    head: BlockInfo,
    accounts: HashMap<AccountId, WatchedAccount>,
}

impl Storage {
    pub fn new(head: BlockInfo, accounts: HashMap<AccountId, WatchedAccount>) -> Self {
        Self { head, accounts }
    }

    pub fn head(&self) -> BlockInfo {
        self.head
    }

    pub fn apply(&mut self, block: BlockInfo, changes: Vec<Change>) {
        for change in changes {
            let Some(account) = self.accounts.get_mut(&change.account_id) else {
                continue;
            };
            if !account
                .prefixes
                .iter()
                .any(|prefix| change.key.starts_with(prefix))
            {
                continue;
            }
            match change.value {
                Some(value) => account.entries.insert(change.key, value),
                None => account.entries.remove(&change.key),
            };
        }
        self.head = block;
    }

    pub fn account(&self, account_id: &AccountId) -> Option<AccountState> {
        self.accounts.get(account_id).map(|account| AccountState {
            block: self.head,
            entries: account.entries.clone(),
        })
    }
}
