use std::collections::{BTreeMap, HashMap};

use near_min_api::types::AccountId;
use swap_router_types::indexed::{AccountState, BlockInfo};

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
