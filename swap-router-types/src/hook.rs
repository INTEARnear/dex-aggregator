use std::{borrow::Cow, net::IpAddr, time::Duration};

use near_sdk::json_types::Base64VecU8;
use otc_dex_types::AuthorizedTradeIntent;
use serde::{Deserialize, Serialize};

use crate::types::{Quote, SwapRequest};

pub const HOOK_ABI_VERSION: u32 = 3;
pub const HOOK_ANSWER_TIMEOUT: Duration = Duration::from_millis(10);

/// The HTTP request the swap request came in, the websocket upgrade for subscriptions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestDetails {
    pub ip: IpAddr,
    pub uri: String,
    pub headers: Vec<(String, String)>,
}

/// What a hook is called with. The aggregator lends its data, and a hook gets it owned.
#[derive(Serialize, Deserialize)]
pub struct HookInput<'a> {
    pub request: Cow<'a, RequestDetails>,
    pub swap_request: Cow<'a, SwapRequest>,
    /// The quotes of the other DEXes and earlier hooks, best first
    pub quotes: Cow<'a, [Quote]>,
}

/// A quote a hook adds. The aggregator builds its route, so a hook only says what it trades.
#[derive(Debug, Serialize, Deserialize)]
pub enum HookQuote {
    IntearOtcIntent {
        /// The `AuthorizedTradeIntent`, Borsh in base64
        maker_intent: Base64VecU8,
    },
}

impl HookQuote {
    pub fn otc_intent(maker_intent: &AuthorizedTradeIntent) -> Self {
        Self::IntearOtcIntent {
            maker_intent: Base64VecU8(near_sdk::borsh::to_vec(maker_intent).unwrap()),
        }
    }
}

/// A hook, for [`export_hook!`](crate::export_hook) to make the ABI of
#[cfg(feature = "hook")]
pub trait Hook: Sized + Send + Sync + 'static {
    /// Called once when the aggregator loads the hook, to check its configuration and start keeping
    /// what it needs up to date. An error stops the aggregator from starting.
    fn init() -> Result<Self, String>;

    /// The quotes to add on top of `input.quotes`. Ignored if it doesn't answer within
    /// [`HOOK_ANSWER_TIMEOUT`].
    fn run(&self, input: HookInput<'static>) -> Result<Vec<HookQuote>, String>;
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

            /// # Safety
            /// `input` is a NUL-terminated CStr that outlives the call
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn hook_run(
                input: *const ::std::ffi::c_char,
            ) -> *mut ::std::ffi::c_char {
                unsafe { $crate::hook::__private::run(&HOOK, input) }
            }

            /// # Safety
            /// `output` was returned by `hook_init` or `hook_run`, and isn't used after this
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

    use super::Hook;

    pub fn init<H: Hook>(hook: &OnceLock<H>) -> *mut c_char {
        output(catch_panic(|| {
            let initialized = H::init()?;
            hook.set(initialized)
                .map_err(|_| "The hook is initialized once".to_string())
        }))
    }

    /// # Safety
    /// `input` is a NUL-terminated CStr that outlives the call
    pub unsafe fn run<H: Hook>(hook: &OnceLock<H>, input: *const c_char) -> *mut c_char {
        let input = unsafe { CStr::from_ptr(input) };
        output(catch_panic(|| {
            let hook = hook.get().ok_or("The hook isn't initialized")?;
            let input = input
                .to_str()
                .map_err(|err| format!("Input isn't UTF-8: {err}"))?;
            let input = serde_json::from_str(input)
                .map_err(|err| format!("Input isn't a hook input: {err}"))?;
            hook.run(input)
        }))
    }

    /// # Safety
    /// `output` was returned by [`init`] or [`run`], and isn't used after this
    pub unsafe fn free(output: *mut c_char) {
        drop(unsafe { CString::from_raw(output) });
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

    const SWAP_REQUEST: &str = r#"{"token_in":"near","token_out":"nep141:usdc.near","amount_in":"1000","slippage_type":"Fixed","slippage":"0.01","dexes":"Plach,hook:rfq","trader_account_id":"trader.near","referrer_id":"app.near","fees":"{\"app.near\":0.005}"}"#;
    const QUOTE: &str = r#"{"estimated_amount":{"amount_out":"5"},"worst_case_amount":{"amount_out":"4"},"dex_id":"Plach","execution_instructions":[],"needs_unwrap":false,"token_output":"nep141:usdc.near","estimated_max_gas_cost":"1","estimated_gas_usage":"1","storage_deposits":"0","fee_amount":"0"}"#;

    pub fn input_json() -> String {
        let request = RequestDetails {
            ip: "127.0.0.1".parse().unwrap(),
            uri: "/route?token_in=near".to_string(),
            headers: vec![("user-agent".to_string(), "test".to_string())],
        };
        let swap_request: SwapRequest = serde_json::from_str(SWAP_REQUEST).unwrap();
        let quotes = vec![serde_json::from_str::<Quote>(QUOTE).unwrap()];
        serde_json::to_string(&HookInput {
            request: Cow::Borrowed(&request),
            swap_request: Cow::Borrowed(&swap_request),
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
                crate::types::DexId::Plach,
                crate::types::DexId::Hook("rfq".into())
            ])
        );
        assert_eq!(read.swap_request.fees.0[0].1, 5_000);
        assert_eq!(read.quotes.len(), 1);
        assert_eq!(serde_json::to_string(&read).unwrap(), input);
    }
}

#[cfg(all(test, feature = "hook"))]
mod export_tests {
    use std::ffi::{CStr, CString};

    use super::*;

    struct TestHook;

    impl Hook for TestHook {
        fn init() -> Result<Self, String> {
            Ok(TestHook)
        }

        fn run(&self, input: HookInput<'static>) -> Result<Vec<HookQuote>, String> {
            if input.quotes.is_empty() {
                panic!("No quotes to beat");
            }
            Ok(vec![HookQuote::IntearOtcIntent {
                maker_intent: Base64VecU8(input.swap_request.token_in.to_string().into_bytes()),
            }])
        }
    }

    crate::export_hook!(TestHook);

    // The exported symbols, found the way the aggregator finds them
    extern "C" {
        fn hook_abi_version() -> u32;
        fn hook_init() -> *mut std::ffi::c_char;
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

        let quotes = answer(unsafe { hook_run(input.as_ptr()) });
        let quotes: Vec<HookQuote> = serde_json::from_value(quotes["Ok"].clone()).unwrap();
        let [HookQuote::IntearOtcIntent { maker_intent, .. }] = quotes.as_slice() else {
            panic!("Not one OTC intent: {quotes:?}");
        };
        assert_eq!(maker_intent.0, b"near");

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
