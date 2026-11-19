use std::{borrow::Cow, collections::HashMap, net::IpAddr, time::Duration};

use bigdecimal::BigDecimal;
#[cfg(feature = "hook")]
use intear_dex_common::FeeDiscountFraction;
use intear_dex_common::{AssetId, DexId};
use near_min_api::types::{AccountId, BlockHeight, NearGas, NearToken};
use near_sdk::{
    json_types::{Base64VecU8, U128},
    PublicKeyHandle,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[cfg(feature = "hook")]
use crate::indexed::{AccountState, Watch};
use crate::types::{Amount, CostPrices, Gas, Quote, SwapRequest};

pub const HOOK_ABI_VERSION: u32 = 9;
pub const HOOK_ANSWER_TIMEOUT: Duration = Duration::from_secs(1);

/// The HTTP request the swap request came in, the websocket upgrade for subscriptions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestDetails {
    pub ip: IpAddr,
    pub uri: String,
    pub headers: Vec<(String, String)>,
}

/// The swap on Intear DEX that the aggregator routes the request through: the assets after
/// converting the request's tokens, and the amount after the app fees
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntearDexTrade {
    pub asset_in: AssetId,
    pub asset_out: AssetId,
    /// `AmountIn` is what the DEX gets, which the engine takes the app fees out of the input for.
    /// `AmountOut` is what the DEX gives, and the app fees will go on top of the input.
    pub amount: Amount,
    /// Of the request, resolved for these assets. 0.005 is 0.5%.
    #[serde(
        deserialize_with = "crate::types::from_str",
        serialize_with = "crate::types::to_str"
    )]
    pub slippage: BigDecimal,
    pub trader_account_id: Option<AccountId>,
}

/// What a hook's [`Hook::run`] is called with. The aggregator lends its data, and a hook gets it
/// owned.
#[derive(Serialize, Deserialize)]
pub struct HookInput<'a> {
    pub request: Cow<'a, RequestDetails>,
    pub swap_request: Cow<'a, SwapRequest>,
    pub trade: Cow<'a, IntearDexTrade>,
    pub block_height: BlockHeight,
    /// The quotes of the other DEXes, best first
    pub quotes: Cow<'a, [Quote]>,
    pub route_costs: Cow<'a, IntearDexRouteCosts>,
}

/// What the route of a swap on Intear DEX costs besides the gas its swap burns, which the
/// aggregator ranks it with against the other quotes, as [`crate::types::net_amount`] does
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntearDexRouteCosts {
    /// The gas fee and storage deposits of the route
    pub without_oracle_request: NearToken,
    /// The same, when the swap's operations start with an oracle request
    pub with_oracle_request: NearToken,
    /// Of a unit of gas that the swap burns
    pub gas_price: NearToken,
    /// None when quotes are ranked by their amounts alone
    pub prices: Option<CostPrices>,
}

/// A swap on Intear DEX that a hook quotes for the whole [`IntearDexTrade`], like a market maker
/// does. The aggregator builds its route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookQuote {
    pub dex_id: DexId,
    /// The swap's message to the DEX
    pub message: Base64VecU8,
    /// What the DEX gives for an exact-in trade, or takes for an exact-out one, without the app
    /// fees and the fee of the oracle
    pub estimated_amount: U128,
    /// The least the DEX gives, or the most it takes, which the swap is constrained to
    pub worst_case_amount: U128,
    /// Of the DEX's swap on Intear DEX
    pub gas: Gas,
    /// What a discount that the message carries saves, already in the estimated amount
    pub discount: Option<U128>,
    /// What the DEX's swap reads the response of its oracle to, which the operations start with
    /// an `OracleRequest` of. Only one starts them, so quotes that ask for another one can't be
    /// fallbacks of this one.
    pub oracle_request: Option<OracleRequest>,
}

/// A request to the oracle that has `oracle_key`, on Intear DEX
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OracleRequest {
    pub oracle_key: PublicKeyHandle,
    pub request: Base64VecU8,
    /// What the oracle takes for the request in the trade's asset in, which the engine takes from
    /// the input before the swap. The quote is of a trade after it.
    pub fee: U128,
}

/// Bytes of the state of a pool that its swaps change. The aggregator keeps a copy of it for each
/// simulation of a swap, so it's small and copied without allocating.
pub const POOL_STATE_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct PoolState(pub [u8; POOL_STATE_BYTES]);

impl PoolState {
    /// Of a pool whose swaps only change its balances
    pub fn of_balances<const N: usize>(balances: [u128; N]) -> Self {
        const { assert!(N * 16 <= POOL_STATE_BYTES) };
        let mut state = [0; POOL_STATE_BYTES];
        for (bytes, balance) in state.chunks_exact_mut(16).zip(balances) {
            bytes.copy_from_slice(&balance.to_le_bytes());
        }
        Self(state)
    }

    /// The balances that [`PoolState::of_balances`] made it of
    pub fn balances<const N: usize>(&self) -> [u128; N] {
        const { assert!(N * 16 <= POOL_STATE_BYTES) };
        std::array::from_fn(|index| {
            u128::from_le_bytes(self.0[index * 16..][..16].try_into().unwrap())
        })
    }
}

impl Serialize for PoolState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Base64VecU8(self.0.to_vec()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PoolState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = Base64VecU8::deserialize(deserializer)?.0;
        let length = bytes.len();
        bytes.try_into().map(Self).map_err(|_| {
            serde::de::Error::custom(format!(
                "A pool state is {POOL_STATE_BYTES} bytes, not {length}"
            ))
        })
    }
}

/// The pools of a [`PoolsSnapshot`], which the aggregator routes through and refers to by their
/// index in `pools`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookPools {
    pub pools: Vec<HookPool>,
    /// Of the oracles that pools read
    pub oracle_fees: Vec<OracleFees>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookPool {
    /// A swap refers to them by position
    pub tokens: [AssetId; 2],
    pub state: PoolState,
    /// What a swap burns for a hop through the pool, which routes weigh against what it gains
    pub hop_gas: NearGas,
    /// The oracle whose prices swaps through the pool read, which the operations ask first
    pub oracle_key: Option<PublicKeyHandle>,
}

/// What the oracle that has `oracle_key` takes for a request, in each asset it takes fees in
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleFees {
    pub oracle_key: PublicKeyHandle,
    pub fees: HashMap<AssetId, U128>,
}

/// A swap through pools of a [`PoolsSnapshot`] on its DEX, which the hook makes the message of
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSwapRequest {
    pub asset_in: AssetId,
    pub asset_out: AssetId,
    pub routes: Vec<HookSwapRoute>,
    /// `None` for a swap that swaps what the swap before it gave, which isn't known in advance,
    /// so it doesn't have a fee discount
    pub amount: Option<Amount>,
    /// Millionths of the pool and protocol fees that the swap doesn't pay
    pub fee_discount: Option<u32>,
    pub trader_account_id: Option<AccountId>,
    /// The fee discount is valid for some blocks after it
    pub block_height: BlockHeight,
}

/// A part of a swap through pools one after another, by their index in [`HookPools::pools`]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookSwapRoute {
    /// Of `intear_dex_common::FULL_ROUTE_FRACTION` that the route swaps. The fractions of a swap's
    /// routes add up to it, and the DEX gives the first route what rounding leaves.
    pub fraction: u32,
    pub pools: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSwap {
    pub dex_id: DexId,
    /// The swap's message to the DEX
    pub message: Base64VecU8,
    pub gas: Gas,
    /// The request to the oracle of the swap's pools, whose response the swap reads
    pub oracle_request: Option<Base64VecU8>,
}

/// Prices of the smallest units of tokens in the smallest unit of USDt
pub type Prices = HashMap<AssetId, BigDecimal>;

/// A hook, for [`export_hook!`](crate::export_hook) to make the ABI of. It quotes trades with
/// [`Hook::run`], or gives pools with [`Hook::pools`] that the aggregator routes through together
/// with the pools of the other hooks.
#[cfg(feature = "hook")]
pub trait Hook: Sized + Send + Sync + 'static {
    /// [`NoPools`] for a hook that only quotes
    type Pools: PoolsSnapshot;

    /// Called once when the aggregator loads the hook, to check its configuration and start keeping
    /// what it needs up to date. An error stops the aggregator from starting.
    fn init() -> Result<Self, String>;

    /// Contract storage that [`Hook::on_indexed_state`] gets after every block
    fn watches(&self) -> Vec<Watch> {
        Vec::new()
    }

    /// The watched storage after a block, by account. The next block's comes after this returns.
    /// An error stops the aggregator.
    fn on_indexed_state(&self, _states: Vec<(AccountId, AccountState)>) -> Result<(), String> {
        Ok(())
    }

    /// The prices that changed since the last call, all of them in the first call.
    fn on_prices(&self, _prices: Prices) {}

    /// The quotes to add on top of `input.quotes`. Ignored if it doesn't answer within
    /// [`HOOK_ANSWER_TIMEOUT`].
    fn run(&self, _input: HookInput<'static>) -> Result<Vec<HookQuote>, String> {
        Ok(Vec::new())
    }

    /// The pools to route through now, the same snapshot while they don't change. None while
    /// there are none, like when they're outdated.
    fn pools(&self) -> Option<std::sync::Arc<Self::Pools>> {
        None
    }
}

/// The pools of a hook at one moment, which the aggregator simulates swaps in while it searches
/// for routes, and then asks the messages of the swaps of the route it found
#[cfg(feature = "hook")]
pub trait PoolsSnapshot: Send + Sync + 'static {
    fn pools(&self) -> HookPools;

    /// Swaps between the tokens at the positions `token_in` and `token_out` of the pool at `pool`
    /// in [`HookPools::pools`], whose state is `state`, and changes `state` like the swap would.
    /// Gives the output for `AmountIn`, and the input for `AmountOut`, or None if the DEX fails the
    /// swap. A route search calls it tens of thousands of times, so it doesn't allocate.
    fn swap(
        &self,
        pool: u32,
        state: &mut PoolState,
        token_in: usize,
        token_out: usize,
        amount: Amount,
        fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<u128>;

    fn build_swap(&self, request: HookSwapRequest) -> Result<HookSwap, String>;
}

/// The pools of a hook that has none
#[cfg(feature = "hook")]
pub enum NoPools {}

#[cfg(feature = "hook")]
impl PoolsSnapshot for NoPools {
    fn pools(&self) -> HookPools {
        match *self {}
    }

    fn swap(
        &self,
        _pool: u32,
        _state: &mut PoolState,
        _token_in: usize,
        _token_out: usize,
        _amount: Amount,
        _fee_discount: Option<FeeDiscountFraction>,
    ) -> Option<u128> {
        match *self {}
    }

    fn build_swap(&self, _request: HookSwapRequest) -> Result<HookSwap, String> {
        match *self {}
    }
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct PricesJson(pub HashMap<AssetId, String>);

impl From<&Prices> for PricesJson {
    fn from(prices: &Prices) -> Self {
        Self(
            prices
                .iter()
                .map(|(asset_id, price)| (asset_id.clone(), price.to_string()))
                .collect(),
        )
    }
}

impl TryFrom<PricesJson> for Prices {
    type Error = String;

    fn try_from(prices: PricesJson) -> Result<Self, String> {
        prices
            .0
            .into_iter()
            .map(|(asset_id, price)| {
                let parsed = price
                    .parse()
                    .map_err(|err| format!("Price of {asset_id} {price} isn't a number: {err}"))?;
                Ok((asset_id, parsed))
            })
            .collect()
    }
}

/// Exports the ABI of a [`Hook`] from this library: `export_hook!(MyHook);`
#[cfg(feature = "hook")]
#[macro_export]
macro_rules! export_hook {
    ($hook:ty) => {
        const _: () = {
            static HOOK: ::std::sync::OnceLock<$hook> = ::std::sync::OnceLock::new();

            #[unsafe(no_mangle)]
            pub extern "C" fn hook_abi_version() -> u32 {
                $crate::hook::HOOK_ABI_VERSION
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn hook_init() -> *mut ::std::ffi::c_char {
                $crate::hook::__private::init(&HOOK)
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn hook_watches() -> *mut ::std::ffi::c_char {
                $crate::hook::__private::watches(&HOOK)
            }

            /// # Safety
            /// `states` is `len` bytes that outlive the call
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_on_indexed_state(
                states: *const u8,
                len: usize,
            ) -> *mut ::std::ffi::c_char {
                unsafe { $crate::hook::__private::on_indexed_state(&HOOK, states, len) }
            }

            /// # Safety
            /// `prices` is a NUL-terminated CStr that outlives the call
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_on_prices(
                prices: *const ::std::ffi::c_char,
            ) -> *mut ::std::ffi::c_char {
                unsafe { $crate::hook::__private::on_prices(&HOOK, prices) }
            }

            /// # Safety
            /// `input` is a NUL-terminated CStr that outlives the call
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_run(
                input: *const ::std::ffi::c_char,
            ) -> *mut ::std::ffi::c_char {
                unsafe { $crate::hook::__private::run(&HOOK, input) }
            }

            /// The current snapshot of the hook's pools, null without one. The aggregator releases
            /// it with `hook_release_pools_snapshot`, and it's the same pointer while the pools
            /// don't change.
            #[unsafe(no_mangle)]
            pub extern "C" fn hook_pools_snapshot() -> *const ::std::ffi::c_void {
                $crate::hook::__private::pools_snapshot(&HOOK)
            }

            /// # Safety
            /// `snapshot` was returned by `hook_pools_snapshot`, and isn't used after this
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_release_pools_snapshot(
                snapshot: *const ::std::ffi::c_void,
            ) {
                unsafe { $crate::hook::__private::release_pools_snapshot::<$hook>(snapshot) }
            }

            /// # Safety
            /// `snapshot` was returned by `hook_pools_snapshot` and isn't released yet
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_snapshot_pools(
                snapshot: *const ::std::ffi::c_void,
            ) -> *mut ::std::ffi::c_char {
                unsafe { $crate::hook::__private::snapshot_pools::<$hook>(snapshot) }
            }

            /// Writes the output of an exact-in swap, or the input of an exact-out one, to
            /// `result`, and returns whether the swap succeeded. A `fee_discount` of 0 is none.
            ///
            /// # Safety
            /// `snapshot` was returned by `hook_pools_snapshot` and isn't released yet, and
            /// `state` and `result` are valid for the call
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_swap(
                snapshot: *const ::std::ffi::c_void,
                pool: u32,
                state: *mut $crate::hook::PoolState,
                token_in: u8,
                token_out: u8,
                amount: u128,
                exact_in: bool,
                fee_discount: u32,
                result: *mut u128,
            ) -> bool {
                unsafe {
                    $crate::hook::__private::swap::<$hook>(
                        snapshot,
                        pool,
                        state,
                        token_in,
                        token_out,
                        amount,
                        exact_in,
                        fee_discount,
                        result,
                    )
                }
            }

            /// # Safety
            /// `snapshot` was returned by `hook_pools_snapshot` and isn't released yet, and
            /// `request` is a NUL-terminated CStr that outlives the call
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_build_swap(
                snapshot: *const ::std::ffi::c_void,
                request: *const ::std::ffi::c_char,
            ) -> *mut ::std::ffi::c_char {
                unsafe { $crate::hook::__private::build_swap::<$hook>(snapshot, request) }
            }

            /// # Safety
            /// `output` was returned by another function of the hook, and isn't used after this
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_free(output: *mut ::std::ffi::c_char) {
                unsafe { $crate::hook::__private::free(output) }
            }
        };
    };
}

#[cfg(feature = "hook")]
#[doc(hidden)]
pub mod __private {
    use std::{
        ffi::{c_char, c_void, CStr, CString},
        panic::AssertUnwindSafe,
        sync::{Arc, OnceLock},
    };

    use intear_dex_common::FeeDiscountFraction;
    use serde::Serialize;

    use super::{Hook, PoolState, PoolsSnapshot, Prices, PricesJson};
    use crate::types::Amount;

    pub fn init<H: Hook>(hook: &OnceLock<H>) -> *mut c_char {
        output(catch_panic(|| {
            let initialized = H::init()?;
            hook.set(initialized)
                .map_err(|_| "The hook is initialized once".to_string())
        }))
    }

    pub fn watches<H: Hook>(hook: &OnceLock<H>) -> *mut c_char {
        output(catch_panic(|| Ok(initialized(hook)?.watches())))
    }

    /// # Safety
    /// `states` is `len` bytes that outlive the call
    pub unsafe fn on_indexed_state<H: Hook>(
        hook: &OnceLock<H>,
        states: *const u8,
        len: usize,
    ) -> *mut c_char {
        let states = unsafe { std::slice::from_raw_parts(states, len) };
        output(catch_panic(|| {
            let states = borsh::from_slice(states)
                .map_err(|err| format!("Indexed state isn't Borsh of account states: {err}"))?;
            initialized(hook)?.on_indexed_state(states)
        }))
    }

    /// # Safety
    /// `prices` is a NUL-terminated CStr that outlives the call
    pub unsafe fn on_prices<H: Hook>(hook: &OnceLock<H>, prices: *const c_char) -> *mut c_char {
        let prices = unsafe { CStr::from_ptr(prices) };
        output(catch_panic(|| {
            let prices = prices
                .to_str()
                .map_err(|err| format!("Prices aren't UTF-8: {err}"))?;
            let prices: PricesJson = serde_json::from_str(prices)
                .map_err(|err| format!("Prices aren't prices: {err}"))?;
            initialized(hook)?.on_prices(Prices::try_from(prices)?);
            Ok(())
        }))
    }

    /// # Safety
    /// `input` is a NUL-terminated CStr that outlives the call
    pub unsafe fn run<H: Hook>(hook: &OnceLock<H>, input: *const c_char) -> *mut c_char {
        let input = unsafe { CStr::from_ptr(input) };
        output(catch_panic(|| {
            let hook = initialized(hook)?;
            let input = input
                .to_str()
                .map_err(|err| format!("Input isn't UTF-8: {err}"))?;
            let input = serde_json::from_str(input)
                .map_err(|err| format!("Input isn't a hook input: {err}"))?;
            hook.run(input)
        }))
    }

    pub fn pools_snapshot<H: Hook>(hook: &OnceLock<H>) -> *const c_void {
        let snapshot = catch_panic(|| Ok(initialized(hook)?.pools()));
        match snapshot {
            Ok(Some(snapshot)) => Arc::into_raw(snapshot).cast(),
            Ok(None) => std::ptr::null(),
            Err(err) => {
                eprintln!("Failed to get the pools snapshot: {err}");
                std::ptr::null()
            }
        }
    }

    /// # Safety
    /// `snapshot` was returned by [`pools_snapshot`] of `H`, and isn't used after this
    pub unsafe fn release_pools_snapshot<H: Hook>(snapshot: *const c_void) {
        drop(unsafe { Arc::from_raw(snapshot.cast::<H::Pools>()) });
    }

    /// # Safety
    /// `snapshot` was returned by [`pools_snapshot`] of `H` and isn't released yet
    pub unsafe fn snapshot_pools<H: Hook>(snapshot: *const c_void) -> *mut c_char {
        let snapshot = unsafe { &*snapshot.cast::<H::Pools>() };
        output(catch_panic(|| Ok(snapshot.pools())))
    }

    /// # Safety
    /// `snapshot` was returned by [`pools_snapshot`] of `H` and isn't released yet, and `state`
    /// and `result` are valid for the call
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn swap<H: Hook>(
        snapshot: *const c_void,
        pool: u32,
        state: *mut PoolState,
        token_in: u8,
        token_out: u8,
        amount: u128,
        exact_in: bool,
        fee_discount: u32,
        result: *mut u128,
    ) -> bool {
        let snapshot = unsafe { &*snapshot.cast::<H::Pools>() };
        let state = unsafe { &mut *state };
        let amount = match exact_in {
            true => Amount::AmountIn(amount),
            false => Amount::AmountOut(amount),
        };
        let fee_discount = (fee_discount != 0).then(|| FeeDiscountFraction::new(fee_discount));
        let swapped = std::panic::catch_unwind(AssertUnwindSafe(|| {
            snapshot.swap(
                pool,
                state,
                usize::from(token_in),
                usize::from(token_out),
                amount,
                fee_discount,
            )
        }));
        match swapped {
            Ok(Some(swapped)) => {
                unsafe { *result = swapped };
                true
            }
            Ok(None) | Err(_) => false,
        }
    }

    /// # Safety
    /// `snapshot` was returned by [`pools_snapshot`] of `H` and isn't released yet, and `request`
    /// is a NUL-terminated CStr that outlives the call
    pub unsafe fn build_swap<H: Hook>(
        snapshot: *const c_void,
        request: *const c_char,
    ) -> *mut c_char {
        let snapshot = unsafe { &*snapshot.cast::<H::Pools>() };
        let request = unsafe { CStr::from_ptr(request) };
        output(catch_panic(|| {
            let request = request
                .to_str()
                .map_err(|err| format!("Swap request isn't UTF-8: {err}"))?;
            let request = serde_json::from_str(request)
                .map_err(|err| format!("Swap request isn't a hook swap request: {err}"))?;
            snapshot.build_swap(request)
        }))
    }

    /// # Safety
    /// `output` was returned by another function of the hook, and isn't used after this
    pub unsafe fn free(output: *mut c_char) {
        drop(unsafe { CString::from_raw(output) });
    }

    fn initialized<H: Hook>(hook: &OnceLock<H>) -> Result<&H, String> {
        hook.get()
            .ok_or_else(|| "The hook isn't initialized".to_string())
    }

    fn catch_panic<T>(answer: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        std::panic::catch_unwind(AssertUnwindSafe(answer)).unwrap_or_else(|panic| {
            Err(panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    panic
                        .downcast_ref::<&str>()
                        .map(|message| message.to_string())
                })
                .unwrap_or_else(|| "Panicked".to_string()))
        })
    }

    fn output(result: Result<impl Serialize, String>) -> *mut c_char {
        // JSON escapes NUL so CString::new never fails
        CString::new(serde_json::to_string(&result).unwrap())
            .unwrap()
            .into_raw()
    }
}

#[cfg(test)]
mod tests {
    use intear_dex_common::FeeFraction;

    use super::*;

    const SWAP_REQUEST: &str = r#"{"token_in":"near","token_out":"nep141:usdc.near","amount_in":"1000","slippage_type":"Fixed","slippage":"0.01","dexes":"Rhea,Plach","trader_account_id":"trader.near","referrer_id":"app.near","fees":"{\"app.near\":0.005}"}"#;
    const QUOTE: &str = r#"{"estimated_amount":{"amount_out":"5"},"worst_case_amount":{"amount_out":"4"},"dex_id":"Rhea","execution_instructions":[],"needs_unwrap":false,"token_output":"nep141:usdc.near","estimated_max_gas_cost":"1","estimated_gas_usage":"1","storage_deposits":"0","fee_amount":"0"}"#;

    pub fn input_json() -> String {
        let request = RequestDetails {
            ip: "127.0.0.1".parse().unwrap(),
            uri: "/route?token_in=near".to_string(),
            headers: vec![("user-agent".to_string(), "test".to_string())],
        };
        let swap_request: SwapRequest = serde_json::from_str(SWAP_REQUEST).unwrap();
        let trade = IntearDexTrade {
            asset_in: AssetId::Near,
            asset_out: AssetId::Nep141("usdc.near".parse().unwrap()),
            amount: Amount::AmountIn(995),
            slippage: "0.01".parse().unwrap(),
            trader_account_id: Some("trader.near".parse().unwrap()),
        };
        let quotes = vec![serde_json::from_str::<Quote>(QUOTE).unwrap()];
        serde_json::to_string(&HookInput {
            request: Cow::Borrowed(&request),
            swap_request: Cow::Borrowed(&swap_request),
            trade: Cow::Borrowed(&trade),
            block_height: 100,
            quotes: Cow::Borrowed(&quotes),
            route_costs: Cow::Owned(IntearDexRouteCosts {
                without_oracle_request: NearToken::from_millinear(1),
                with_oracle_request: NearToken::from_millinear(2),
                gas_price: NearToken::from_yoctonear(100_000_000),
                prices: Some(CostPrices {
                    near_price_raw: "0.000000000000000000000002".parse().unwrap(),
                    quoted_token_price_raw: "0.000001".parse().unwrap(),
                }),
            }),
        })
        .unwrap()
    }

    #[test]
    fn hook_reads_the_input_the_aggregator_sends() {
        let input = input_json();
        let read: HookInput<'static> = serde_json::from_str(&input).unwrap();
        assert_eq!(read.request.headers[0].1, "test");
        assert_eq!(
            read.swap_request.dexes,
            Some(vec![
                crate::types::DexId::Rhea,
                crate::types::DexId::Hook("Plach".into())
            ])
        );
        assert_eq!(read.swap_request.fees.0[0].1, FeeFraction::new(5_000));
        assert_eq!(read.trade.amount, Amount::AmountIn(995));
        assert_eq!(read.trade.slippage, "0.01".parse::<BigDecimal>().unwrap());
        assert_eq!(read.quotes.len(), 1);
        assert_eq!(
            read.route_costs.prices.as_ref().unwrap().near_price_raw,
            "0.000000000000000000000002".parse::<BigDecimal>().unwrap()
        );
        assert_eq!(serde_json::to_string(&read).unwrap(), input);
    }

    #[test]
    fn pool_state_keeps_balances() {
        let state = PoolState::of_balances([1, u128::MAX]);
        assert_eq!(state.balances(), [1, u128::MAX]);
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<PoolState>(&json).unwrap(), state);
        assert!(serde_json::from_str::<PoolState>(r#""AAAA""#).is_err());
    }

    #[test]
    fn prices_keep_their_precision() {
        let prices = Prices::from([(
            AssetId::Near,
            "0.000000000000000000004556621431".parse().unwrap(),
        )]);
        let json = serde_json::to_string(&PricesJson::from(&prices)).unwrap();
        let read = Prices::try_from(serde_json::from_str::<PricesJson>(&json).unwrap()).unwrap();
        assert_eq!(read, prices);
    }
}

#[cfg(all(test, feature = "hook"))]
mod export_tests {
    use std::ffi::{c_void, CStr, CString};
    use std::sync::Arc;

    use super::*;
    use crate::indexed::BlockInfo;

    struct TestHook {
        last_price: std::sync::Mutex<Option<BigDecimal>>,
        indexed_keys: std::sync::Mutex<usize>,
        pools: Arc<TestPools>,
    }

    /// One pool of NEAR and USDC that gives twice the input, less with a fee discount
    struct TestPools;

    impl PoolsSnapshot for TestPools {
        fn pools(&self) -> HookPools {
            HookPools {
                pools: vec![HookPool {
                    tokens: [AssetId::Near, AssetId::Nep141("usdc.near".parse().unwrap())],
                    state: PoolState::of_balances([1_000, 2_000]),
                    hop_gas: NearGas::from_tgas(2),
                    oracle_key: None,
                }],
                oracle_fees: vec![],
            }
        }

        fn swap(
            &self,
            _pool: u32,
            state: &mut PoolState,
            token_in: usize,
            token_out: usize,
            amount: Amount,
            fee_discount: Option<FeeDiscountFraction>,
        ) -> Option<u128> {
            let Amount::AmountIn(amount_in) = amount else {
                return None;
            };
            let amount_out = 2 * amount_in + u128::from(fee_discount.map_or(0, |d| d.get()));
            let mut balances = state.balances::<2>();
            balances[token_in] += amount_in;
            balances[token_out] = balances[token_out].checked_sub(amount_out)?;
            *state = PoolState::of_balances(balances);
            Some(amount_out)
        }

        fn build_swap(&self, request: HookSwapRequest) -> Result<HookSwap, String> {
            Ok(HookSwap {
                dex_id: "owner.near/dex".parse().unwrap(),
                message: Base64VecU8(
                    request.routes[0]
                        .pools
                        .iter()
                        .map(|&pool| pool as u8)
                        .collect(),
                ),
                gas: Gas::from_ggas(5_000, 5_000),
                oracle_request: None,
            })
        }
    }

    impl Hook for TestHook {
        type Pools = TestPools;

        fn init() -> Result<Self, String> {
            Ok(TestHook {
                last_price: Default::default(),
                indexed_keys: Default::default(),
                pools: Arc::new(TestPools),
            })
        }

        fn watches(&self) -> Vec<Watch> {
            vec![Watch {
                account_id: "dex.near".parse().unwrap(),
                prefixes: vec![vec![0]],
            }]
        }

        fn on_indexed_state(&self, states: Vec<(AccountId, AccountState)>) -> Result<(), String> {
            *self.indexed_keys.lock().unwrap() = states[0].1.entries.len();
            Ok(())
        }

        fn on_prices(&self, prices: Prices) {
            *self.last_price.lock().unwrap() = prices.get(&AssetId::Near).cloned();
        }

        fn run(&self, input: HookInput<'static>) -> Result<Vec<HookQuote>, String> {
            if input.quotes.is_empty() {
                panic!("No quotes to beat");
            }
            Ok(vec![HookQuote {
                dex_id: "owner.near/dex".parse().unwrap(),
                message: Base64VecU8(vec![
                    *self.indexed_keys.lock().unwrap() as u8,
                    u8::from(*self.last_price.lock().unwrap() == Some("0.000001".parse().unwrap())),
                ]),
                estimated_amount: U128(input.block_height.into()),
                worst_case_amount: U128(1),
                gas: Gas::from_ggas(20_000, 25_000),
                discount: None,
                oracle_request: None,
            }])
        }

        fn pools(&self) -> Option<Arc<TestPools>> {
            Some(Arc::clone(&self.pools))
        }
    }

    crate::export_hook!(TestHook);

    // The exported symbols, found the way the aggregator finds them
    extern "C" {
        fn hook_abi_version() -> u32;
        fn hook_init() -> *mut std::ffi::c_char;
        fn hook_watches() -> *mut std::ffi::c_char;
        fn hook_on_indexed_state(states: *const u8, len: usize) -> *mut std::ffi::c_char;
        fn hook_on_prices(prices: *const std::ffi::c_char) -> *mut std::ffi::c_char;
        fn hook_run(input: *const std::ffi::c_char) -> *mut std::ffi::c_char;
        fn hook_pools_snapshot() -> *const c_void;
        fn hook_release_pools_snapshot(snapshot: *const c_void);
        fn hook_snapshot_pools(snapshot: *const c_void) -> *mut std::ffi::c_char;
        #[allow(improper_ctypes)]
        fn hook_swap(
            snapshot: *const c_void,
            pool: u32,
            state: *mut PoolState,
            token_in: u8,
            token_out: u8,
            amount: u128,
            exact_in: bool,
            fee_discount: u32,
            result: *mut u128,
        ) -> bool;
        fn hook_build_swap(
            snapshot: *const c_void,
            request: *const std::ffi::c_char,
        ) -> *mut std::ffi::c_char;
        fn hook_free(output: *mut std::ffi::c_char);
    }

    fn answer(output: *mut std::ffi::c_char) -> serde_json::Value {
        let answer = serde_json::from_str(unsafe { CStr::from_ptr(output) }.to_str().unwrap());
        unsafe { hook_free(output) };
        answer.unwrap()
    }

    #[test]
    fn exported_abi_runs_the_hook_and_answers_panics_as_errors() {
        assert_eq!(unsafe { hook_abi_version() }, HOOK_ABI_VERSION);
        let input = CString::new(super::tests::input_json()).unwrap();
        assert_eq!(
            answer(unsafe { hook_run(input.as_ptr()) }),
            serde_json::json!({ "Err": "The hook isn't initialized" })
        );
        assert!(unsafe { hook_pools_snapshot() }.is_null());
        assert_eq!(
            answer(unsafe { hook_init() }),
            serde_json::json!({ "Ok": null })
        );
        assert_eq!(
            answer(unsafe { hook_init() }),
            serde_json::json!({ "Err": "The hook is initialized once" })
        );

        let watches = answer(unsafe { hook_watches() });
        let watches: Vec<Watch> = serde_json::from_value(watches["Ok"].clone()).unwrap();
        assert_eq!(watches[0].prefixes, vec![vec![0]]);

        let states = borsh::to_vec(&vec![(
            AccountId::try_from("dex.near".to_string()).unwrap(),
            AccountState {
                block: BlockInfo {
                    height: 1,
                    hash: Default::default(),
                    timestamp_nanosec: 0,
                },
                entries: [(vec![0, 1], vec![2]), (vec![0, 2], vec![3])].into(),
            },
        )])
        .unwrap();
        assert_eq!(
            answer(unsafe { hook_on_indexed_state(states.as_ptr(), states.len()) }),
            serde_json::json!({ "Ok": null })
        );
        let prices = CString::new(r#"{"near":"0.000001"}"#).unwrap();
        assert_eq!(
            answer(unsafe { hook_on_prices(prices.as_ptr()) }),
            serde_json::json!({ "Ok": null })
        );

        let quotes = answer(unsafe { hook_run(input.as_ptr()) });
        let quotes: Vec<HookQuote> = serde_json::from_value(quotes["Ok"].clone()).unwrap();
        let [quote] = quotes.as_slice() else {
            panic!("Not one quote: {quotes:?}");
        };
        // Both indexed keys, and the price
        assert_eq!(quote.message.0, vec![2, 1]);
        assert_eq!(quote.estimated_amount, U128(100));

        let mut without_quotes: serde_json::Value =
            serde_json::from_str(&super::tests::input_json()).unwrap();
        without_quotes["quotes"] = serde_json::json!([]);
        let without_quotes = CString::new(without_quotes.to_string()).unwrap();
        assert_eq!(
            answer(unsafe { hook_run(without_quotes.as_ptr()) }),
            serde_json::json!({ "Err": "No quotes to beat" })
        );

        simulates_swaps_in_the_pools_of_a_snapshot();
    }

    /// Of the initialized hook
    fn simulates_swaps_in_the_pools_of_a_snapshot() {
        let snapshot = unsafe { hook_pools_snapshot() };
        assert!(!snapshot.is_null());
        let same_snapshot = unsafe { hook_pools_snapshot() };
        assert_eq!(snapshot, same_snapshot);
        unsafe { hook_release_pools_snapshot(same_snapshot) };

        let pools = answer(unsafe { hook_snapshot_pools(snapshot) });
        let pools: HookPools = serde_json::from_value(pools["Ok"].clone()).unwrap();
        let mut state = pools.pools[0].state;
        let mut result = 0;
        let swap = |state: &mut PoolState, amount, exact_in, fee_discount, result: &mut u128| unsafe {
            hook_swap(
                snapshot,
                0,
                state,
                0,
                1,
                amount,
                exact_in,
                fee_discount,
                result,
            )
        };
        assert!(swap(&mut state, 100, true, 0, &mut result));
        assert_eq!(result, 200);
        assert_eq!(state.balances(), [1_100, 1_800]);
        assert!(swap(&mut state, 100, true, 7, &mut result));
        assert_eq!(result, 207);
        // Fails without changing the state
        assert!(!swap(&mut state, 100, false, 0, &mut result));
        assert!(!swap(&mut state, 1_000, true, 0, &mut result));
        assert_eq!(state.balances(), [1_200, 1_593]);

        let request = CString::new(
            serde_json::to_string(&HookSwapRequest {
                asset_in: AssetId::Near,
                asset_out: AssetId::Nep141("usdc.near".parse().unwrap()),
                routes: vec![HookSwapRoute {
                    fraction: 1_000_000,
                    pools: vec![0],
                }],
                amount: Some(Amount::AmountIn(100)),
                fee_discount: None,
                trader_account_id: None,
                block_height: 1,
            })
            .unwrap(),
        )
        .unwrap();
        let swap = answer(unsafe { hook_build_swap(snapshot, request.as_ptr()) });
        let swap: HookSwap = serde_json::from_value(swap["Ok"].clone()).unwrap();
        assert_eq!(swap.message.0, vec![0]);
        unsafe { hook_release_pools_snapshot(snapshot) };
    }
}
