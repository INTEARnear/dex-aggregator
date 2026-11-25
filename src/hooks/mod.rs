pub mod inteardex;
pub mod pool_routes;
mod pools;

pub use pools::{routing_pools, RoutingPools};

use std::{
    borrow::Cow,
    ffi::{c_char, c_void, CStr, CString},
    fs::{File, OpenOptions},
    io::{ErrorKind, Write},
    mem,
    net::IpAddr,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::{Arc, LazyLock, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::{HeaderMap, Uri};
use intear_dex_common::AssetId;
use libloading::Library;
use near_min_api::types::{AccountId, BlockHeight};
use pathfinder::IndexedHook;
use swap_router_types::{
    hook::{
        HookInput, HookQuote, IntearDexRouteCosts, IntearDexTrade, PoolState, Prices, PricesJson,
        RequestDetails, HOOK_ABI_VERSION, HOOK_ANSWER_TIMEOUT,
    },
    indexed::{AccountState, Watch},
};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{
    route::RouteBlueprint,
    shared_utils::{BaseTokenId, NetworkView, TOKEN_INFOS_REFRESH_INTERVAL},
    types::Quote,
    Amount, DexId, SwapRequest,
};

const HOOK_DIRECTORY: &str = "hooks";
const HOOK_TIMES_FILE: &str = "hook-times.txt";

type InitFunction = unsafe extern "C" fn() -> *mut c_char;
type WatchesFunction = unsafe extern "C" fn() -> *mut c_char;
type OnIndexedStateFunction = unsafe extern "C" fn(*const u8, usize) -> *mut c_char;
type OnPricesFunction = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type RunFunction = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type FreeFunction = unsafe extern "C" fn(*mut c_char);
type PoolsSnapshotFunction = unsafe extern "C" fn() -> *const c_void;
type ReleasePoolsSnapshotFunction = unsafe extern "C" fn(*const c_void);
type SnapshotPoolsFunction = unsafe extern "C" fn(*const c_void) -> *mut c_char;
type SwapFunction = unsafe extern "C" fn(
    *const c_void,
    u32,
    *mut PoolState,
    u8,
    u8,
    u128,
    bool,
    u32,
    *mut u128,
) -> bool;
type BuildSwapFunction = unsafe extern "C" fn(*const c_void, *const c_char) -> *mut c_char;

#[derive(Clone)]
struct HookPlugin {
    name: Arc<str>,
    on_indexed_state: OnIndexedStateFunction,
    on_prices: OnPricesFunction,
    run: RunFunction,
    free: FreeFunction,
    pools_snapshot: PoolsSnapshotFunction,
    release_pools_snapshot: ReleasePoolsSnapshotFunction,
    snapshot_pools: SnapshotPoolsFunction,
    swap: SwapFunction,
    build_swap: BuildSwapFunction,
}

pub struct Hook {
    plugin: HookPlugin,
    watches: Vec<Watch>,
    /// Keeps the plugin's functions loaded
    _library: Library,
}

static LOADED_HOOKS: OnceLock<Vec<Hook>> = OnceLock::new();
static HOOK_TIMES: OnceLock<Mutex<File>> = OnceLock::new();
/// Whether every hook has taken prices
static PRICES_TAKEN: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::Sender::new(false));

pub fn request_details(ip: IpAddr, uri: &Uri, headers: &HeaderMap) -> RequestDetails {
    RequestDetails {
        ip,
        uri: uri.to_string(),
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
    }
}

/// Loads every `hooks/*.so`. A file that isn't a hook of this ABI stops the start.
pub fn load() {
    let mut hook_paths = match std::fs::read_dir(HOOK_DIRECTORY) {
        Ok(entries) => entries
            .map(|entry| entry.expect("Reading hook directory failed").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "so"))
            .collect::<Vec<_>>(),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            info!("No {HOOK_DIRECTORY}/ directory, no hooks to load");
            vec![]
        }
        Err(err) => panic!("Can't read {HOOK_DIRECTORY}/: {err}"),
    };
    hook_paths.sort();
    let hooks = hook_paths.iter().map(|path| load_hook(path)).collect();
    if LOADED_HOOKS.set(hooks).is_err() {
        panic!("Hooks are loaded once");
    }
    let hook_times = OpenOptions::new()
        .create(true)
        .append(true)
        .open(HOOK_TIMES_FILE)
        .unwrap_or_else(|err| panic!("Can't open {HOOK_TIMES_FILE}: {err}"));
    if HOOK_TIMES.set(Mutex::new(hook_times)).is_err() {
        panic!("Hook times are opened once");
    }
}

pub fn loaded() -> &'static [Hook] {
    LOADED_HOOKS.get().expect("Hooks are loaded at startup")
}

fn load_hook(path: &Path) -> Hook {
    let name = path
        .file_stem()
        .and_then(|file_stem| file_stem.to_str())
        .unwrap_or_else(|| panic!("{} doesn't have a UTF-8 name", path.display()));
    // `dexes` in requests is comma-separated, and a hook can't take the name of another DEX
    assert!(
        !name.contains(',') && name.parse::<DexId>() == Ok(DexId::Hook(name.into())),
        "{} can't be a hook name",
        path.display()
    );
    // SAFETY: a hook is trusted code that runs with the aggregator's permissions, and only exports
    // the functions of the ABI in `swap_router_types::hook`
    let (library, plugin, watches) = unsafe {
        let library =
            Library::new(path).unwrap_or_else(|err| panic!("Can't load {}: {err}", path.display()));
        let abi_version = library
            .get::<unsafe extern "C" fn() -> u32>(b"hook_abi_version")
            .unwrap_or_else(|err| panic!("{} has no hook_abi_version: {err}", path.display()))(
        );
        assert_eq!(
            abi_version,
            HOOK_ABI_VERSION,
            "{} is built for hook ABI version {abi_version}, the aggregator loads {HOOK_ABI_VERSION}",
            path.display()
        );
        macro_rules! function {
            ($name:literal, $type:ty) => {
                *library
                    .get::<$type>($name.as_bytes())
                    .unwrap_or_else(|err| panic!("{} has no {}: {err}", path.display(), $name))
            };
        }
        let init = function!("hook_init", InitFunction);
        let hook_watches = function!("hook_watches", WatchesFunction);
        let plugin = HookPlugin {
            name: name.into(),
            on_indexed_state: function!("hook_on_indexed_state", OnIndexedStateFunction),
            on_prices: function!("hook_on_prices", OnPricesFunction),
            run: function!("hook_run", RunFunction),
            free: function!("hook_free", FreeFunction),
            pools_snapshot: function!("hook_pools_snapshot", PoolsSnapshotFunction),
            release_pools_snapshot: function!(
                "hook_release_pools_snapshot",
                ReleasePoolsSnapshotFunction
            ),
            snapshot_pools: function!("hook_snapshot_pools", SnapshotPoolsFunction),
            swap: function!("hook_swap", SwapFunction),
            build_swap: function!("hook_build_swap", BuildSwapFunction),
        };
        if let Err(err) = answer::<()>(&plugin, "init", init()) {
            panic!("{} failed to start: {err}", path.display());
        }
        let watches = answer::<Vec<Watch>>(&plugin, "watches", hook_watches())
            .unwrap_or_else(|err| panic!("{} has no watches: {err}", path.display()));
        (library, plugin, watches)
    };
    let hook = Hook {
        plugin,
        watches,
        _library: library,
    };
    info!("Loaded hook {} from {}", hook.dex_id(), path.display());
    hook
}

/// The hooks that watch storage, for the pool indexer to give it to
pub fn indexed() -> Vec<IndexedHook> {
    loaded()
        .iter()
        .filter(|hook| !hook.watches.is_empty())
        .map(|hook| {
            let plugin = hook.plugin.clone();
            IndexedHook {
                name: plugin.name.to_string(),
                watches: hook.watches.clone(),
                on_state: Arc::new(move |states: Vec<(AccountId, AccountState)>| {
                    let states = borsh::to_vec(&states).unwrap();
                    // SAFETY: the hook gets the bytes of `states`, which outlive the call
                    let output =
                        unsafe { (plugin.on_indexed_state)(states.as_ptr(), states.len()) };
                    answer::<()>(&plugin, "on_indexed_state", output)
                }),
            }
        })
        .collect()
}

/// Once every hook has taken prices, which a hook may need to quote its pools
pub async fn prices_taken() {
    PRICES_TAKEN
        .subscribe()
        .wait_for(|taken| *taken)
        .await
        .expect("The sender is static");
}

/// Gives each hook the prices that changed since it last took them, after every refresh of the
/// prices. A hook that doesn't take them gets them again after the next refresh.
pub async fn follow_prices(network: impl NetworkView) {
    let mut taken_by_hook = vec![Prices::new(); loaded().len()];
    loop {
        if let Ok(token_infos) = network.token_infos() {
            let prices = token_infos
                .iter()
                .map(|(token_id, info)| {
                    let asset_id = match token_id {
                        BaseTokenId::Near => AssetId::Near,
                        BaseTokenId::Nep141(account_id) => AssetId::Nep141(account_id.clone()),
                    };
                    (asset_id, info.price_usd_raw.clone())
                })
                .collect::<Prices>();
            for (hook, taken) in loaded().iter().zip(&mut taken_by_hook) {
                let changed = prices
                    .iter()
                    .filter(|(asset_id, price)| taken.get(asset_id) != Some(price))
                    .map(|(asset_id, price)| (asset_id.clone(), price.clone()))
                    .collect::<Prices>();
                if changed.is_empty() {
                    continue;
                }
                let changed_json =
                    CString::new(serde_json::to_string(&PricesJson::from(&changed)).unwrap())
                        .expect("JSON has no NUL bytes");
                let plugin = hook.plugin.clone();
                let answer = tokio::task::spawn_blocking(move || {
                    // SAFETY: the hook gets a NUL-terminated CString that outlives the call
                    let output = unsafe { (plugin.on_prices)(changed_json.as_ptr()) };
                    answer::<()>(&plugin, "on_prices", output)
                })
                .await
                .expect("Hooks take prices");
                match answer {
                    Ok(()) => taken.extend(changed),
                    Err(err) => warn!(
                        "{} didn't take prices, retrying after the next refresh: {err}",
                        hook.plugin.name
                    ),
                }
            }
            if taken_by_hook.iter().all(|taken| !taken.is_empty()) {
                PRICES_TAKEN.send_if_modified(|all_taken| !mem::replace(all_taken, true));
            }
        }
        tokio::time::sleep(TOKEN_INFOS_REFRESH_INTERVAL).await;
    }
}

impl Hook {
    pub fn dex_id(&self) -> DexId {
        DexId::Hook(self.plugin.name.clone())
    }

    /// The quotes the hook adds on top of `quotes` for the Intear DEX trade of the request. None
    /// when it takes too long or panics.
    pub async fn quotes(
        &self,
        request: &SwapRequest,
        request_details: &RequestDetails,
        trade: &IntearDexTrade,
        block_height: BlockHeight,
        quotes: &[Quote],
        route_costs: &IntearDexRouteCosts,
    ) -> Vec<HookQuote> {
        let input = serde_json::to_string(&HookInput {
            request: Cow::Borrowed(request_details),
            swap_request: Cow::Borrowed(request),
            trade: Cow::Borrowed(trade),
            block_height,
            quotes: Cow::Borrowed(quotes),
            route_costs: Cow::Borrowed(route_costs),
        })
        .unwrap();
        let plugin = self.plugin.clone();
        let timed_request = request.clone();
        let answer = tokio::task::spawn_blocking(move || {
            let started_at = Instant::now();
            let hook_quotes = run_hook(&plugin, input);
            record_hook_time(&plugin, &timed_request, started_at.elapsed(), &hook_quotes);
            hook_quotes.unwrap_or_else(|err| panic!("{} failed: {err}", plugin.name))
        });
        match tokio::time::timeout(HOOK_ANSWER_TIMEOUT, answer).await {
            Ok(Ok(hook_quotes)) => hook_quotes,
            // The panic is logged where it happened
            Ok(Err(_)) => vec![],
            Err(_) => {
                warn!(
                    "{} took over {HOOK_ANSWER_TIMEOUT:?} to quote, leaving its quotes out of \
                     {trade:?}",
                    self.plugin.name
                );
                vec![]
            }
        }
    }
}

/// The routes of the swaps for the Intear DEX trade of the request. None if building them panics.
pub fn routes(
    dex_id: DexId,
    request: &SwapRequest,
    trade: &IntearDexTrade,
    swaps: Vec<inteardex::IntearDexSwap>,
) -> Vec<RouteBlueprint> {
    catch_unwind(AssertUnwindSafe(|| {
        swaps
            .into_iter()
            .filter_map(|swap| inteardex::route(dex_id.clone(), request, trade, swap))
            .collect::<Vec<_>>()
    }))
    .unwrap_or_default()
}

/// Blocks on the hook. An answer that isn't of the ABI panics, which is recorded as the hook
/// panicking.
fn run_hook(plugin: &HookPlugin, input: String) -> Result<Vec<HookQuote>, String> {
    let input = CString::new(input).expect("JSON has no NUL bytes");
    // SAFETY: the hook gets a NUL-terminated CString that outlives the call
    let output = unsafe { (plugin.run)(input.as_ptr()) };
    answer(plugin, "run", output)
}

/// A line of [`HOOK_TIMES_FILE`]: when the call ended in Unix milliseconds, the hook, how long the
/// call took in microseconds, the number of quotes or `error`, the tokens, and the amount
fn record_hook_time(
    plugin: &HookPlugin,
    request: &SwapRequest,
    took: Duration,
    hook_quotes: &Result<Vec<HookQuote>, String>,
) {
    let ended_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let quotes = match hook_quotes {
        Ok(hook_quotes) => hook_quotes.len().to_string(),
        Err(_) => "error".to_string(),
    };
    let (amount_kind, amount) = match request.amount {
        Amount::AmountIn(amount) => ("amount_in", amount),
        Amount::AmountOut(amount) => ("amount_out", amount),
    };
    let line = format!(
        "{}\t{}\t{}\t{quotes}\t{}\t{}\t{amount_kind}\t{amount}\n",
        ended_at.as_millis(),
        plugin.name,
        took.as_micros(),
        request.token_in,
        request.token_out,
    );
    HOOK_TIMES
        .get()
        .expect("Hook times are opened with the hooks")
        .lock()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap_or_else(|err| panic!("Can't write to {HOOK_TIMES_FILE}: {err}"));
}

/// Copies the hook's answer, frees it with `hook_free`, and reads its result. An answer that isn't
/// of the ABI panics.
fn answer<T: serde::de::DeserializeOwned>(
    plugin: &HookPlugin,
    function: &str,
    output: *mut c_char,
) -> Result<T, String> {
    let name = &plugin.name;
    assert!(
        !output.is_null(),
        "{name} returned no output from {function}"
    );
    // SAFETY: the plugin returns a NUL-terminated CStr, which `hook_free` frees after it's copied
    let output = unsafe {
        let output_json = CStr::from_ptr(output).to_str().map(str::to_owned);
        (plugin.free)(output);
        output_json
    }
    .unwrap_or_else(|err| panic!("{name} returned invalid UTF-8 from {function}: {err}"));
    serde_json::from_str::<Result<T, String>>(&output).unwrap_or_else(|err| {
        panic!("{name} returned what isn't an answer of {function}: {err}: {output}")
    })
}
