mod storage;

pub use storage::{AccountState, BlockInfo};

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use inindexer::MessageStreamer;
use inindexer::near_indexer_primitives::StreamerMessage;
use inindexer::near_indexer_primitives::views::StateChangeValueView;
use inindexer::teardata::TeardataProvider;
use near_min_api::types::{AccountId, BlockHeight, BlockId, BlockReference, CryptoHash, Finality};
use near_min_api::{QueryFinality, RpcClient};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::storage::{Change, Storage, WatchedAccount};

const LOAD_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Storage of an account to follow, only keys that start with one of the prefixes
#[derive(Clone)]
pub struct Watch {
    pub account_id: AccountId,
    pub prefixes: Vec<Vec<u8>>,
}

#[derive(Clone)]
pub struct PoolIndexer {
    storage: Arc<Mutex<Option<Storage>>>,
    blocks: watch::Receiver<Option<BlockInfo>>,
}

impl PoolIndexer {
    pub fn start(watches: Vec<Watch>, rpc: Arc<RpcClient>) -> (Self, JoinHandle<anyhow::Error>) {
        let storage = Arc::new(Mutex::new(None));
        let (blocks_sender, blocks) = watch::channel(None);
        let task = tokio::spawn(run(watches, rpc, Arc::clone(&storage), blocks_sender));
        (Self { storage, blocks }, task)
    }

    /// The newest indexed block, `None` until storage is loaded
    pub fn blocks(&self) -> watch::Receiver<Option<BlockInfo>> {
        self.blocks.clone()
    }

    /// Storage of the account after the newest indexed block, `None` until storage is loaded
    pub fn account_state(&self, account_id: &AccountId) -> Option<AccountState> {
        self.storage
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|storage| storage.account(account_id))
    }
}

async fn run(
    watches: Vec<Watch>,
    rpc: Arc<RpcClient>,
    storage: Arc<Mutex<Option<Storage>>>,
    blocks: watch::Sender<Option<BlockInfo>>,
) -> anyhow::Error {
    let loaded = loop {
        match load(&watches, &rpc).await {
            Ok(loaded) => break loaded,
            Err(e) => {
                warn!("Failed to load storage: {e:#}");
                tokio::time::sleep(LOAD_RETRY_INTERVAL).await;
            }
        }
    };
    let head = loaded.head();
    *storage.lock().unwrap() = Some(loaded);
    blocks.send_replace(Some(head));
    follow(&watches, &storage, &blocks, head.height + 1).await
}

async fn load(watches: &[Watch], rpc: &RpcClient) -> Result<Storage, anyhow::Error> {
    let header = rpc
        .block(BlockReference::Finality(Finality::Final))
        .await?
        .header;
    let head = BlockInfo {
        height: header.height,
        hash: header.hash,
        timestamp_nanosec: header.timestamp_nanosec,
    };
    let mut accounts = HashMap::new();
    for watch in watches {
        let entries = view_watched_state(rpc, watch, head.hash).await?;
        info!(
            "Loaded {} keys of {} at block {}",
            entries.len(),
            watch.account_id,
            head.height
        );
        accounts.insert(
            watch.account_id.clone(),
            WatchedAccount {
                prefixes: watch.prefixes.clone(),
                entries,
            },
        );
    }
    Ok(Storage::new(head, accounts))
}

/// Applies final blocks starting at `first_block`, returns why it stopped
async fn follow(
    watches: &[Watch],
    storage: &Mutex<Option<Storage>>,
    blocks: &watch::Sender<Option<BlockInfo>>,
    first_block: BlockHeight,
) -> anyhow::Error {
    let (stream, mut messages) = match TeardataProvider::mainnet().stream(first_block, None).await {
        Ok(stream) => stream,
        Err(e) => return anyhow::anyhow!("Failed to stream blocks: {e:?}"),
    };
    while let Some(message) = messages.recv().await {
        match apply(watches, storage, &message) {
            Ok(block) => {
                blocks.send_replace(Some(block));
            }
            Err(e) => return e,
        }
    }
    match stream.await {
        Ok(Err(e)) => anyhow::anyhow!("Block stream failed: {e:?}"),
        Ok(Ok(())) => anyhow::anyhow!("Block stream ended"),
        Err(e) => anyhow::anyhow!("Block stream panicked: {e}"),
    }
}

fn apply(
    watches: &[Watch],
    storage: &Mutex<Option<Storage>>,
    message: &StreamerMessage,
) -> Result<BlockInfo, anyhow::Error> {
    let header = &message.block.header;
    let mut storage = storage.lock().unwrap();
    let storage = storage.as_mut().unwrap();
    let head = storage.head();
    if header.prev_hash.0 != head.hash.0 {
        anyhow::bail!(
            "Block {} doesn't follow the indexed block {} ({})",
            header.height,
            head.height,
            head.hash
        );
    }
    let mut changes = Vec::new();
    for state_change in message.shards.iter().flat_map(|shard| &shard.state_changes) {
        let (account_id, key, value) = match &state_change.value {
            StateChangeValueView::DataUpdate {
                account_id,
                key,
                value,
            } => (account_id, key, Some(value.to_vec())),
            StateChangeValueView::DataDeletion { account_id, key } => (account_id, key, None),
            _ => continue,
        };
        let Some(watch) = watches
            .iter()
            .find(|watch| watch.account_id.as_str() == account_id.as_str())
        else {
            continue;
        };
        changes.push(Change {
            account_id: watch.account_id.clone(),
            key: key.to_vec(),
            value,
        });
    }
    let block = BlockInfo {
        height: header.height,
        hash: CryptoHash(header.hash.0),
        timestamp_nanosec: header.timestamp_nanosec,
    };
    storage.apply(block, changes);
    Ok(block)
}

/// Storage of the account under the watched prefixes
async fn view_watched_state(
    rpc: &RpcClient,
    watch: &Watch,
    block_hash: CryptoHash,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, anyhow::Error> {
    let mut entries = BTreeMap::new();
    for prefix in &watch.prefixes {
        let result = rpc
            .view_state(
                watch.account_id.clone(),
                prefix,
                QueryFinality::BlockId(BlockId::Hash(block_hash)),
            )
            .await?;
        if result.last_key.is_some() {
            anyhow::bail!(
                "view_state of {} at block {block_hash} returned a partial result",
                watch.account_id
            );
        }
        entries.extend(
            result
                .values
                .into_iter()
                .map(|item| (item.key.into(), item.value.into())),
        );
    }
    Ok(entries)
}
