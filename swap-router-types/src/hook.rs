use std::{borrow::Cow, collections::HashMap, net::IpAddr, time::Duration};

use bigdecimal::BigDecimal;
use intear_dex_types::AssetId;
use near_min_api::types::{AccountId, BlockHeight};
use near_sdk::json_types::{Base64VecU8, U128};
use serde::{Deserialize, Serialize};

#[cfg(feature = "hook")]
use crate::indexed::{AccountState, Watch};
use crate::types::{Amount, Gas, Quote, SwapRequest};

pub const HOOK_ABI_VERSION: u32 = 5;
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

/// What a hook is called with. The aggregator lends its data, and a hook gets it owned.
#[derive(Serialize, Deserialize)]
pub struct HookInput<'a> {
    pub request: Cow<'a, RequestDetails>,
    pub swap_request: Cow<'a, SwapRequest>,
    pub trade: Cow<'a, IntearDexTrade>,
    pub block_height: BlockHeight,
    /// The quotes of the other DEXes and earlier hooks, best first
    pub quotes: Cow<'a, [Quote]>,
}

/// A swap on Intear DEX that a hook quotes for the [`IntearDexTrade`]. The aggregator builds its
/// route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookQuote {
    pub dex_id: intear_dex_types::DexId,
    /// The swap's message to the DEX
    pub message: Base64VecU8,
    /// What the DEX gives for an exact-in trade, or takes for an exact-out one, without the app
    /// fees
    pub estimated_amount: U128,
    /// The least the DEX gives, or the most it takes, which the swap is constrained to
    pub worst_case_amount: U128,
    /// Of the DEX's swap on Intear DEX
    pub gas: Gas,
    /// What a discount that the message carries saves, already in the estimated amount
    pub discount: Option<U128>,
}

/// Prices of the smallest units of tokens in the smallest unit of USDt
pub type Prices = HashMap<AssetId, BigDecimal>;

/// A hook, for [`export_hook!`](crate::export_hook) to make the ABI of
#[cfg(feature = "hook")]
pub trait Hook: Sized + Send + Sync + 'static {
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
    fn run(&self, input: HookInput<'static>) -> Result<Vec<HookQuote>, String>;
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
        ffi::{c_char, CStr, CString},
        panic::AssertUnwindSafe,
        sync::OnceLock,
    };

    use serde::Serialize;

    use super::{Hook, Prices, PricesJson};

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
        assert_eq!(read.swap_request.fees.0[0].1, 5_000);
        assert_eq!(read.trade.amount, Amount::AmountIn(995));
        assert_eq!(read.trade.slippage, "0.01".parse::<BigDecimal>().unwrap());
        assert_eq!(read.quotes.len(), 1);
        assert_eq!(serde_json::to_string(&read).unwrap(), input);
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
    use std::ffi::{CStr, CString};

    use super::*;
    use crate::indexed::BlockInfo;

    struct TestHook {
        last_price: std::sync::Mutex<Option<BigDecimal>>,
        indexed_keys: std::sync::Mutex<usize>,
    }

    impl Hook for TestHook {
        fn init() -> Result<Self, String> {
            Ok(TestHook {
                last_price: Default::default(),
                indexed_keys: Default::default(),
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
            }])
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
    }
}
