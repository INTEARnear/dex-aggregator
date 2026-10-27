pub mod inteardex;

use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::{c_char, CStr, CString},
    fs::{File, OpenOptions},
    io::{ErrorKind, Write},
    net::IpAddr,
    panic::AssertUnwindSafe,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::{HeaderMap, Uri};
use bigdecimal::BigDecimal;
use futures_util::FutureExt;
use intear_dex_types::AssetId;
use libloading::Library;
use near_min_api::types::{AccountId, BlockHeight};
use pathfinder::IndexedHook;
use swap_router_types::{
    hook::{
        HookInput, HookQuote, IntearDexTrade, Prices, PricesJson, RequestDetails, HOOK_ABI_VERSION,
        HOOK_ANSWER_TIMEOUT,
    },
    indexed::{AccountState, Watch},
};
use tokio::sync::Semaphore;
use tracing::info;

use crate::{
    shared_utils::{BaseTokenId, Mainnet, NetworkView, TOKEN_INFOS_REFRESH_INTERVAL},
    stats::RouteOutcome,
    types::Quote,
    Amount, DexId, Route, SwapRequest,
};

const HOOK_DIRECTORY: &str = "hooks";
const HOOK_TIMES_FILE: &str = "hook-times.txt";
const ROUTE_TIMEOUT: Duration = Duration::from_millis(60_000);
const MAX_CALLS_RUNNING: usize = 64;

type InitFunction = unsafe extern "C" fn() -> *mut c_char;
type WatchesFunction = unsafe extern "C" fn() -> *mut c_char;
type OnIndexedStateFunction = unsafe extern "C" fn(*const u8, usize) -> *mut c_char;
type OnPricesFunction = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type RunFunction = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type FreeFunction = unsafe extern "C" fn(*mut c_char);

#[derive(Clone)]
struct HookPlugin {
    name: Arc<str>,
    on_indexed_state: OnIndexedStateFunction,
    on_prices: OnPricesFunction,
    run: RunFunction,
    free: FreeFunction,
}

pub struct Hook {
    plugin: HookPlugin,
    watches: Vec<Watch>,
    calls_running: Arc<Semaphore>,
    /// Keeps the plugin's functions loaded
    _library: Library,
}

static LOADED_HOOKS: OnceLock<Vec<Hook>> = OnceLock::new();
static HOOK_TIMES: OnceLock<Mutex<File>> = OnceLock::new();

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
        calls_running: Arc::new(Semaphore::new(MAX_CALLS_RUNNING)),
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

/// Gives the hooks the prices that changed, after every refresh of the prices
pub async fn follow_prices(network: impl NetworkView) {
    let mut sent = Prices::new();
    loop {
        if let Ok(token_infos) = network.token_infos() {
            let changed = token_infos
                .iter()
                .map(|(token_id, info)| {
                    let asset_id = match token_id {
                        BaseTokenId::Near => AssetId::Near,
                        BaseTokenId::Nep141(account_id) => AssetId::Nep141(account_id.clone()),
                    };
                    (asset_id, info.price_usd_raw.clone())
                })
                .filter(|(asset_id, price)| sent.get(asset_id) != Some(price))
                .collect::<HashMap<AssetId, BigDecimal>>();
            if !changed.is_empty() {
                let prices =
                    CString::new(serde_json::to_string(&PricesJson::from(&changed)).unwrap())
                        .expect("JSON has no NUL bytes");
                let prices = Arc::new(prices);
                for hook in loaded() {
                    let plugin = hook.plugin.clone();
                    let prices = prices.clone();
                    tokio::task::spawn_blocking(move || {
                        // SAFETY: the hook gets a NUL-terminated CString that outlives the call
                        let output = unsafe { (plugin.on_prices)(prices.as_ptr()) };
                        if let Err(err) = answer::<()>(&plugin, "on_prices", output) {
                            panic!("{} didn't take prices: {err}", plugin.name);
                        }
                    })
                    .await
                    .expect("Hooks take prices");
                }
                sent.extend(changed);
            }
        }
        tokio::time::sleep(TOKEN_INFOS_REFRESH_INTERVAL).await;
    }
}

impl Hook {
    pub fn dex_id(&self) -> DexId {
        DexId::Hook(self.plugin.name.clone())
    }

    /// The quotes the hook adds on top of `quotes` for the Intear DEX trade of the request, and
    /// how asking it went
    pub async fn quotes(
        &self,
        request: &SwapRequest,
        request_details: &RequestDetails,
        trade: &IntearDexTrade,
        block_height: BlockHeight,
        quotes: &[Quote],
    ) -> (RouteOutcome, Vec<HookQuote>) {
        let Ok(call_running) = self.calls_running.clone().try_acquire_owned() else {
            return (RouteOutcome::TimedOut, vec![]);
        };
        let input = serde_json::to_string(&HookInput {
            request: Cow::Borrowed(request_details),
            swap_request: Cow::Borrowed(request),
            trade: Cow::Borrowed(trade),
            block_height,
            quotes: Cow::Borrowed(quotes),
        })
        .unwrap();
        let plugin = self.plugin.clone();
        let timed_request = request.clone();
        let answer = tokio::task::spawn_blocking(move || {
            let _call_running = call_running;
            let started_at = Instant::now();
            let hook_quotes = run_hook(&plugin, input);
            record_hook_time(&plugin, &timed_request, started_at.elapsed(), &hook_quotes);
            hook_quotes.unwrap_or_else(|err| panic!("{} failed: {err}", plugin.name))
        });
        match tokio::time::timeout(HOOK_ANSWER_TIMEOUT, answer).await {
            Ok(Ok(hook_quotes)) if hook_quotes.is_empty() => (RouteOutcome::NotFound, vec![]),
            Ok(Ok(hook_quotes)) => (RouteOutcome::Found, hook_quotes),
            // The panic is logged where it happened
            Ok(Err(_)) => (RouteOutcome::Panicked, vec![]),
            Err(_) => (RouteOutcome::TimedOut, vec![]),
        }
    }
}

/// The routes of the swaps for the Intear DEX trade of the request, and how building them went
pub async fn routes(
    dex_id: DexId,
    request: &SwapRequest,
    trade: &IntearDexTrade,
    swaps: Vec<inteardex::IntearDexSwap>,
) -> (RouteOutcome, Vec<Route>) {
    let routes = AssertUnwindSafe(async {
        let mut routes = Vec::new();
        for swap in swaps {
            routes.extend(inteardex::route(&Mainnet, dex_id.clone(), request, trade, swap).await);
        }
        routes
    })
    .catch_unwind();
    tokio::select! {
        routes = routes => match routes {
            Ok(routes) if routes.is_empty() => (RouteOutcome::NotFound, routes),
            Ok(routes) => (RouteOutcome::Found, routes),
            Err(_) => (RouteOutcome::Panicked, vec![]),
        },
        _ = tokio::time::sleep(ROUTE_TIMEOUT) => (RouteOutcome::TimedOut, vec![]),
    }
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
