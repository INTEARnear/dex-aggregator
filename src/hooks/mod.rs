mod inteardex_otc;

use std::{
    borrow::Cow,
    ffi::{c_char, CStr, CString},
    io::ErrorKind,
    net::IpAddr,
    panic::AssertUnwindSafe,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use axum::http::{HeaderMap, Uri};
use futures_util::FutureExt;
use libloading::Library;
use swap_router_types::hook::{
    HookInput, HookQuote, RequestDetails, HOOK_ABI_VERSION, HOOK_ANSWER_TIMEOUT,
};
use tokio::sync::Semaphore;
use tracing::info;

use crate::{shared_utils::Mainnet, stats::RouteOutcome, types::Quote, DexId, Route, SwapRequest};

const HOOK_DIRECTORY: &str = "hooks";
const ROUTE_TIMEOUT: Duration = Duration::from_millis(60_000);
const MAX_CALLS_RUNNING: usize = 64;

type InitFunction = unsafe extern "C" fn() -> *mut c_char;
type HookFunction = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type FreeFunction = unsafe extern "C" fn(*mut c_char);

#[derive(Clone)]
struct HookPlugin {
    name: Arc<str>,
    hook: HookFunction,
    free: FreeFunction,
}

pub struct Hook {
    plugin: HookPlugin,
    calls_running: Arc<Semaphore>,
    /// Keeps the plugin's functions loaded
    _library: Library,
}

static LOADED_HOOKS: OnceLock<Vec<Hook>> = OnceLock::new();

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
}

pub fn loaded() -> &'static [Hook] {
    LOADED_HOOKS.get().expect("Hooks are loaded at startup")
}

fn load_hook(path: &Path) -> Hook {
    let name = path
        .file_stem()
        .and_then(|file_stem| file_stem.to_str())
        .unwrap_or_else(|| panic!("{} doesn't have a UTF-8 name", path.display()));
    // `dexes` in requests is comma-separated
    assert!(
        !name.is_empty() && !name.contains(','),
        "{} can't be a hook name",
        path.display()
    );
    // SAFETY: a hook is trusted code that runs with the aggregator's permissions, and only exports
    // the functions of the ABI in `swap_router_types::hook`
    let (library, plugin) = unsafe {
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
        let init = *library
            .get::<InitFunction>(b"hook_init")
            .unwrap_or_else(|err| panic!("{} has no hook_init: {err}", path.display()));
        let hook = *library
            .get::<HookFunction>(b"hook_run")
            .unwrap_or_else(|err| panic!("{} has no hook_run: {err}", path.display()));
        let free = *library
            .get::<FreeFunction>(b"hook_free")
            .unwrap_or_else(|err| panic!("{} has no hook_free: {err}", path.display()));
        let plugin = HookPlugin {
            name: name.into(),
            hook,
            free,
        };
        let init_output = take_output(&plugin, init());
        if let Err(err) =
            serde_json::from_str::<Result<(), String>>(&init_output).unwrap_or_else(|err| {
                panic!(
                    "{} returned what isn't an init result: {err}: {init_output}",
                    path.display()
                )
            })
        {
            panic!("{} failed to start: {err}", path.display());
        }
        (library, plugin)
    };
    let hook = Hook {
        plugin,
        calls_running: Arc::new(Semaphore::new(MAX_CALLS_RUNNING)),
        _library: library,
    };
    info!("Loaded hook {} from {}", hook.dex_id(), path.display());
    hook
}

impl Hook {
    pub fn dex_id(&self) -> DexId {
        DexId::Hook(self.plugin.name.clone())
    }

    /// The routes of the quotes the hook adds on top of `quotes`, and how asking it went
    pub async fn routes(
        &self,
        request: &SwapRequest,
        request_details: &RequestDetails,
        quotes: &[Quote],
    ) -> (RouteOutcome, Vec<Route>) {
        let Ok(call_running) = self.calls_running.clone().try_acquire_owned() else {
            return (RouteOutcome::TimedOut, vec![]);
        };
        let input = serde_json::to_string(&HookInput {
            request: Cow::Borrowed(request_details),
            swap_request: Cow::Borrowed(request),
            quotes: Cow::Borrowed(quotes),
        })
        .unwrap();
        let plugin = self.plugin.clone();
        let answer = tokio::task::spawn_blocking(move || {
            let _call_running = call_running;
            call_hook(&plugin, input)
        });
        let hook_quotes = match tokio::time::timeout(HOOK_ANSWER_TIMEOUT, answer).await {
            Ok(Ok(hook_quotes)) => hook_quotes,
            // The panic is logged where it happened
            Ok(Err(_)) => return (RouteOutcome::Panicked, vec![]),
            Err(_) => return (RouteOutcome::TimedOut, vec![]),
        };
        if hook_quotes.is_empty() {
            return (RouteOutcome::NotFound, vec![]);
        }
        let routes = AssertUnwindSafe(self.quote_routes(request, hook_quotes)).catch_unwind();
        tokio::select! {
            routes = routes => match routes {
                Ok(routes) if routes.is_empty() => (RouteOutcome::NotFound, routes),
                Ok(routes) => (RouteOutcome::Found, routes),
                Err(_) => (RouteOutcome::Panicked, vec![]),
            },
            _ = tokio::time::sleep(ROUTE_TIMEOUT) => (RouteOutcome::TimedOut, vec![]),
        }
    }

    /// The route of each quote that has one. A quote that isn't for the request panics.
    async fn quote_routes(&self, request: &SwapRequest, hook_quotes: Vec<HookQuote>) -> Vec<Route> {
        let mut routes = Vec::new();
        for hook_quote in hook_quotes {
            let route = match hook_quote {
                HookQuote::IntearOtcIntent { maker_intent } => {
                    inteardex_otc::route(&Mainnet, self.dex_id(), request, maker_intent).await
                }
            };
            routes.extend(route);
        }
        routes
    }
}

/// Blocks on the hook. A hook that fails or answers with incorrect ABI panics, which is recorded
/// as the hook panicking.
fn call_hook(plugin: &HookPlugin, input: String) -> Vec<HookQuote> {
    let dex_id = DexId::Hook(plugin.name.clone());
    let input = CString::new(input).expect("JSON has no NUL bytes");
    // SAFETY: the hook gets a NUL-terminated CString that outlives the call
    let output = take_output(plugin, unsafe { (plugin.hook)(input.as_ptr()) });
    serde_json::from_str::<Result<Vec<HookQuote>, String>>(&output)
        .unwrap_or_else(|err| panic!("{dex_id} returned what isn't quotes: {err}: {output}"))
        .unwrap_or_else(|err| panic!("{dex_id} failed: {err}"))
}

/// Copies an output of the plugin and frees it with `hook_free`
fn take_output(plugin: &HookPlugin, output: *mut c_char) -> String {
    let dex_id = DexId::Hook(plugin.name.clone());
    assert!(!output.is_null(), "{dex_id} returned no output");
    // SAFETY: the plugin returns a NUL-terminated CStr, which `hook_free` frees after it's copied
    unsafe {
        let output_json = CStr::from_ptr(output).to_str().map(str::to_owned);
        (plugin.free)(output);
        output_json
    }
    .unwrap_or_else(|err| panic!("{dex_id} returned invalid UTF-8: {err}"))
}
