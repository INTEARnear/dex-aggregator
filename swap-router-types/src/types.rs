use std::{collections::BTreeMap, fmt::Display, str::FromStr, sync::Arc};

use bigdecimal::BigDecimal;
use near_min_api::{
    types::{near_crypto::PublicKey, AccountId, Action, Balance, CryptoHash, NearGas, NearToken},
    utils::dec_format,
};
use num_traits::{ToPrimitive, Zero};
use serde::de::Error;
use serde::{Deserialize, Serialize};

use intear_dex_types::{AssetId, FeeFraction, FULL_FEE_FRACTION};

/// What a call, or a part of it, is expected to burn, and the gas it needs attached to not fail
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct Gas {
    pub estimated_burn: NearGas,
    pub needs_attached: NearGas,
}

impl Gas {
    pub const fn from_ggas(estimated_burn: u64, needs_attached: u64) -> Self {
        Self {
            estimated_burn: NearGas::from_ggas(estimated_burn),
            needs_attached: NearGas::from_ggas(needs_attached),
        }
    }

    /// For a function call action
    pub fn attached(self) -> near_min_api::types::Gas {
        near_min_api::types::Gas(self.needs_attached)
    }
}

impl std::ops::Add for Gas {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            estimated_burn: NearGas::from_gas(
                self.estimated_burn.as_gas() + other.estimated_burn.as_gas(),
            ),
            needs_attached: NearGas::from_gas(
                self.needs_attached.as_gas() + other.needs_attached.as_gas(),
            ),
        }
    }
}

impl std::ops::Mul<u64> for Gas {
    type Output = Self;

    fn mul(self, times: u64) -> Self {
        Self {
            estimated_burn: NearGas::from_gas(self.estimated_burn.as_gas() * times),
            needs_attached: NearGas::from_gas(self.needs_attached.as_gas() * times),
        }
    }
}

impl std::iter::Sum for Gas {
    fn sum<I: Iterator<Item = Self>>(gas: I) -> Self {
        gas.fold(Gas::from_ggas(0, 0), |total, gas| total + gas)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Amount {
    AmountIn(#[serde(with = "dec_format")] Balance),
    AmountOut(#[serde(with = "dec_format")] Balance),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenId {
    Near,
    Nep141(AccountId),
    Nep141OnRhea(AccountId),
    TokenOnIntearDex(IntearAssetId),
}

/// An asset on Intear DEX that routes can swap: NEAR or a NEP-141
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IntearAssetId {
    Near,
    Nep141(AccountId),
}

#[derive(Debug)]
pub struct AssetIdNotSupported;

impl TryFrom<AssetId> for IntearAssetId {
    type Error = AssetIdNotSupported;

    fn try_from(asset_id: AssetId) -> Result<Self, Self::Error> {
        match asset_id {
            AssetId::Near => Ok(IntearAssetId::Near),
            AssetId::Nep141(contract_id) => Ok(IntearAssetId::Nep141(contract_id)),
            AssetId::Nep171(_, _) | AssetId::Nep245(_, _) => Err(AssetIdNotSupported),
        }
    }
}

impl From<&IntearAssetId> for AssetId {
    fn from(asset_id: &IntearAssetId) -> Self {
        match asset_id {
            IntearAssetId::Near => AssetId::Near,
            IntearAssetId::Nep141(contract_id) => AssetId::Nep141(contract_id.clone()),
        }
    }
}

impl Display for IntearAssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        AssetId::from(self).fmt(f)
    }
}

impl Display for TokenId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenId::Near => f.write_str("near"),
            TokenId::Nep141(account_id) => write!(f, "nep141:{account_id}"),
            TokenId::Nep141OnRhea(account_id) => write!(f, "rhea-nep141:{account_id}"),
            TokenId::TokenOnIntearDex(asset_id) => write!(f, "intear-dex:{asset_id}"),
        }
    }
}

impl Serialize for TokenId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TokenId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for TokenId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "near" {
            Ok(TokenId::Near)
        } else if let Some(account_id) = s.strip_prefix("rhea-nep141:") {
            Ok(TokenId::Nep141OnRhea(
                account_id.parse().map_err(|_| "Invalid token ID")?,
            ))
        } else if let Some(asset_id) = s.strip_prefix("intear-dex:") {
            let asset_id: AssetId = asset_id
                .parse()
                .map_err(|e| format!("Failed to parse intear-dex asset: {e}"))?;
            Ok(TokenId::TokenOnIntearDex(asset_id.try_into().map_err(
                |AssetIdNotSupported| "Only NEAR and NEP-141s on Intear DEX are supported",
            )?))
        } else if let Some(account_id) = s.strip_prefix("nep141:") {
            Ok(TokenId::Nep141(
                account_id.parse().map_err(|_| "Invalid token ID")?,
            ))
        } else {
            Ok(TokenId::Nep141(s.parse().map_err(|_| "Invalid token ID")?))
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SwapRequest {
    /// The token to swap from. This and token_out must be different.
    pub token_in: TokenId,
    /// The token to swap to. This and token_in must be different.
    pub token_out: TokenId,
    /// The amount to swap. If it's AmountOut, some dexes might not support this.
    #[serde(flatten)]
    pub amount: Amount,
    /// The slippage tolerance. `1.00` means 100%, `0.001` means 0.1%.
    #[serde(flatten)]
    pub slippage: Slippage,
    /// The dexes to use. If not provided, all dexes will be used. Must not be an empty array.
    #[serde(with = "comma_separated", default)]
    pub dexes: Option<Vec<DexId>>,
    /// The account ID of the trader. If provided, the route will include storage
    /// deposit actions.
    pub trader_account_id: Option<AccountId>,
    /// The public key to use for signing.
    pub signing_public_key: Option<PublicKey>,
    /// The account ID of the referrer. If provided, the route will include referral
    /// parameter for tracking.
    pub referrer_id: Option<AccountId>,
    /// Fees the route pays to referrers, as a JSON object of referrer account IDs and
    /// fractions of the input, e.g. `{"ref.near":0.005}` for 0.5%. Intear DEX takes them out
    /// of the input of swaps on it, like Plach's, into the referrers' balances on it, Rhea
    /// fast-path makes an internal balance transfer, Rhea slow-path and Rhea DCL routes send
    /// them in the input token after the swap call. Routes register the referrers to receive
    /// them, so the first swap on a new token for a referrer can be slower.
    #[serde(default)]
    pub fees: QuoteFees,
}

const MAX_REFERRAL_FEE_FRACTION: FeeFraction = FULL_FEE_FRACTION / 20; // 5%
const MAX_REFERRERS: usize = 4;

/// Referrers and the fee each charges, in fractions of [`FULL_FEE_FRACTION`]. With
/// [`AGGREGATOR_FEE_RECEIVER_ID`] and its share, at most [`MAX_REFERRERS`], adding up to at most
/// [`MAX_REFERRAL_FEE_FRACTION`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuoteFees(pub Vec<(AccountId, FeeFraction)>);

impl From<&QuoteFees> for intear_dex_types::ReferralFees {
    fn from(fees: &QuoteFees) -> Self {
        Self(fees.0.clone())
    }
}

impl FromStr for QuoteFees {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let fees: BTreeMap<AccountId, f64> =
            serde_json::from_str(s).map_err(|error| format!("Invalid referral fees: {error}"))?;
        let fees = fees
            .into_iter()
            .map(|(referrer_id, fee)| {
                // The shortest decimal that reads as the number, so 0.01 is exactly 0.01 and not
                // the binary f64 closest to it
                let fee_fraction = BigDecimal::from_str(&fee.to_string())
                    .map_err(|error| format!("Invalid referral fee of {referrer_id}: {error}"))?
                    * BigDecimal::from(FULL_FEE_FRACTION);
                if !fee_fraction.is_integer() {
                    return Err(format!(
                        "Referral fee of {referrer_id} must be a whole number of millionths"
                    ));
                }
                if fee_fraction < BigDecimal::zero() || fee_fraction > MAX_REFERRAL_FEE_FRACTION {
                    return Err(format!(
                        "Referral fee of {referrer_id} must be between 0 and {}",
                        (BigDecimal::from(MAX_REFERRAL_FEE_FRACTION)
                            / BigDecimal::from(FULL_FEE_FRACTION))
                        .normalized()
                    ));
                }
                let fee_fraction = fee_fraction
                    .to_u32()
                    .expect("Checked to be a whole number between 0 and the maximum fee");
                Ok((referrer_id, fee_fraction))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if fees.len() > MAX_REFERRERS {
            return Err(format!("At most {MAX_REFERRERS} accounts can be paid"));
        }
        let referral_fees = QuoteFees(fees);
        let total_fee_fraction: FeeFraction = referral_fees
            .0
            .iter()
            .map(|(_, fee_fraction)| fee_fraction)
            .sum();
        if total_fee_fraction > MAX_REFERRAL_FEE_FRACTION {
            return Err(format!(
                "Referral fees must add up to at most {}",
                (BigDecimal::from(MAX_REFERRAL_FEE_FRACTION) / BigDecimal::from(FULL_FEE_FRACTION))
                    .normalized()
            ));
        }
        Ok(referral_fees)
    }
}

impl Serialize for QuoteFees {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let map = self
            .0
            .iter()
            .map(|(account_id, fee_fraction)| {
                (
                    account_id.clone(),
                    (BigDecimal::from(*fee_fraction) / BigDecimal::from(FULL_FEE_FRACTION))
                        .to_f64()
                        .unwrap(),
                )
            })
            .collect::<BTreeMap<AccountId, f64>>();
        let string = serde_json::to_string(&map).unwrap();
        string.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for QuoteFees {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

mod comma_separated {
    use std::{fmt::Display, str::FromStr};

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, T>(values: &Option<Vec<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Display,
    {
        values
            .as_ref()
            .map(|values| {
                values
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .serialize(serializer)
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: Display,
    {
        let s = Option::<String>::deserialize(deserializer)?;
        if let Some(s) = s {
            let values = s
                .split(',')
                .map(|v| v.parse::<T>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(serde::de::Error::custom)?;
            Ok(Some(values))
        } else {
            Ok(None)
        }
    }
}

pub(crate) fn from_str<'de, D, S>(deserializer: D) -> Result<S, D::Error>
where
    D: serde::Deserializer<'de>,
    S: std::str::FromStr,
{
    // Owned, so it also reads values that were buffered or unescaped, like in a hook's JSON input
    let s = String::deserialize(deserializer)?;
    S::from_str(&s).map_err(|_| D::Error::custom("could not parse string"))
}

pub(crate) fn to_str<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: Display,
{
    serializer.serialize_str(&value.to_string())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "slippage_type")]
pub enum Slippage {
    /// Automatically determine the optimal slippage based on the current market
    /// conditions (liquidity, 24h volume, etc).
    Auto {
        #[serde(deserialize_with = "from_str", serialize_with = "to_str")]
        max_slippage: BigDecimal,
        #[serde(deserialize_with = "from_str", serialize_with = "to_str")]
        min_slippage: BigDecimal,
    },
    /// Fixed slippage percentage. Must be between 0.00 and 1.00.
    Fixed {
        #[serde(deserialize_with = "from_str", serialize_with = "to_str")]
        slippage: BigDecimal,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Route {
    /// The amount of tokens this route will swap. If you provided `Amount::AmountOut`,
    /// this amount will be `Amount::AmountIn` and vice versa.
    pub estimated_amount: Amount,
    /// The amount of tokens this route will swap in the worst case scenario (with
    /// slippage). If you provided `Amount::AmountOut`, this amount will be
    /// `Amount::AmountIn` and vice versa. If you set slippage to `0.01`, this will be
    /// 1% more / less than `estimated_amount`.
    pub worst_case_amount: Amount,
    /// The id of the dex that provided this route.
    pub dex_id: DexId,
    /// How to execute the swap. Need to be executed sequentially.
    pub execution_instructions: Vec<ExecutionInstruction>,
    // to be removed on Mar 11
    #[serde(rename = "needs_unwrap")]
    pub deprecated_needs_unwrap_always_false: bool,
    /// The location of the token to unwrap from. For example, if a certain dex returns
    /// NEP-141 tokens, but you want them to be native NEAR, you need to call near_withdraw
    /// or request a quote from this service again (usually it'll have 0% fee / slippage).
    /// The recommended behavior if this is different from your desired token output is to
    /// request a second quote, converting this token_output to your desired token output,
    /// after the received amount is known.
    pub token_output: TokenId,
    /// What the fee discount signed into this route saves: `Amount::AmountOut` of additional
    /// output if you provided `Amount::AmountIn`, `Amount::AmountIn` taken off the input if you
    /// provided `Amount::AmountOut`. The estimated amounts already include it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discount: Option<Amount>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Quote {
    #[serde(flatten)]
    pub route: Route,
    /// What the execution instructions cost in gas at most, if all gas attached to them is burnt
    pub estimated_max_gas_cost: NearToken,
    /// What the execution instructions are expected to cost in gas, the gas fee to show
    pub estimated_gas_usage: NearToken,
    /// NEAR the execution instructions attach to storage deposits, usually only needed for the
    /// first swap of a token.
    pub storage_deposits: NearToken,
    /// What the fee transactions pay out of the wallet balance of the input token (wNEAR for
    /// NEAR), to pass to `/route/outcome`
    #[serde(with = "dec_format")]
    pub fee_amount: Balance,
    /// What to sign the transactions with, so they can be sent without asking an RPC first: with
    /// `trader_account_id` and `signing_public_key` in the request, once the key is found
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<SigningContext>,
}

/// The nonce and block hash to sign a route's transactions with
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct SigningContext {
    /// The nonce `signing_public_key` is at, as of the latest block: the first transaction takes
    /// the one after. Read when the routes were found, so a transaction the key sent since makes
    /// it stale; sign from the higher of it and the last nonce you used.
    pub nonce: u64,
    /// A recent final block, which transactions signed for stay valid for 86400 blocks after
    pub block_hash: CryptoHash,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum ExecutionInstruction {
    /// Sign a transaction with the given actions and send it to the RPC.
    NearTransaction {
        receiver_id: AccountId,
        actions: Vec<Action>,
        /// Pays referral fees after the swap. Fee transactions are always the last ones of a
        /// route. A swap that fails on-chain still lets the transaction carrying it succeed, so
        /// send them only if `/route/outcome` of the transactions before them says `charge_fee`.
        #[serde(default)]
        is_fee_transaction: bool,
    },
}

/// The transactions of a route that a trader executed, to find out what they did
#[derive(Debug, Deserialize, Clone)]
pub struct OutcomeRequest {
    /// `token_in` of the route request
    pub token_in: TokenId,
    /// `token_out` of the route request
    pub token_out: TokenId,
    /// The account that signed the transactions
    pub trader_account_id: AccountId,
    /// Hashes of the transactions of the route that aren't fee transactions, comma-separated
    #[serde(with = "comma_separated", default)]
    pub tx_hashes: Option<Vec<CryptoHash>>,
    /// `fee_amount` of the quote, which `next_steps` leave in the wallet for the fee transactions
    #[serde(with = "dec_format", default)]
    pub fee_amount: Balance,
    /// When to answer, `executed` by default
    #[serde(default)]
    pub wait_until: OutcomeWaitUntil,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeWaitUntil {
    /// Once every transaction and receipt executed, at least optimistically, with the balances
    /// compared
    #[default]
    Executed,
    /// As soon as the swaps executed optimistically, from what the DEXes logged of them: before
    /// the output reaches the trader, callbacks and gas refunds. A swap that failed, or a route
    /// without a swap, is still answered once everything executed.
    Swap,
}

/// What the transactions of a route did, once they executed
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct SwapOutcome {
    /// Whether the swap went through, so the fee transactions of the route should be sent
    pub charge_fee: bool,
    /// How much of the input token the transactions spent, wherever it was held. NEAR doesn't
    /// count gas and storage deposits. Fees that the fee transactions pay aren't spent yet.
    #[serde(with = "dec_format")]
    pub amount_in: Balance,
    /// How much of the output token the transactions gave, wherever it ended up
    #[serde(with = "dec_format")]
    pub amount_out: Balance,
    /// The balances of the input and output tokens that changed, in every place a token can be
    /// held. Native NEAR doesn't count gas and storage deposits.
    pub balance_changes: Vec<BalanceChange>,
    /// Moves tokens that ended up somewhere else than `token_in` or `token_out` there, like
    /// wNEAR of a swap from NEAR that failed, or output a DEX left in its inner balance. Execute
    /// them sequentially after the fee transactions, if any.
    pub next_steps: Vec<ExecutionInstruction>,
    /// Why the swap failed, to show to the trader. Present when `charge_fee` is false.
    pub error: Option<String>,
    /// Whether every transaction and receipt executed and the balances were compared. False for
    /// an answer at the swap (`wait_until=swap`), whose amounts are what the DEXes logged and
    /// whose `balance_changes` and `next_steps` are empty: ask again with
    /// `wait_until=executed` for them.
    pub settled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct BalanceChange {
    pub token: TokenId,
    /// Signed, negative if the balance went down
    #[serde(with = "signed_dec_format")]
    pub change: i128,
}

mod signed_dec_format {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &i128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i128, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DexId {
    /// https://app.rhea.finance/
    /// AMM DEX
    ///
    /// Supports AmountIn, doesn't support AmountOut
    Rhea,
    /// Removed, still parses so requests that name it don't break, but never has a route
    Aidols,
    /// Directly wrap NEAR to wNEAR, or unwrap wNEAR to NEAR
    ///
    /// Supports both AmountIn and AmountOut
    Wrap,
    /// https://app.rhea.finance/
    /// AMM DEX
    ///
    /// Supports both AmountIn and AmountOut
    RheaDcl,
    /// Removed, still parses so requests that name it don't break, but never has a route
    MetaPool,
    /// Removed, still parses so requests that name it don't break, but never has a route
    Linear,
    /// Removed, still parses so requests that name it don't break, but never has a route
    XRhea,
    /// Removed, still parses so requests that name it don't break, but never has a route
    RNear,
    /// https://dex.intea.rs/
    /// AMM DEX
    ///
    /// Supports both AmountIn and AmountOut
    IntearDex,
    /// A hook loaded at startup from `hooks/<name>.so` that quotes swaps on Intear DEX after the
    /// other DEXes, like `Plach` (https://dex.intea.rs/). Any name that isn't one of the other
    /// DEXes parses, and the aggregator checks that it's loaded.
    ///
    /// Supports what the hook supports
    Hook(Arc<str>),
}

const RHEA_STR: &str = "Rhea";
const AIDOLS_STR: &str = "Aidols";
const WRAP_STR: &str = "Wrap";
const RHEA_DCL_STR: &str = "RheaDcl";
const METAPOOL_STR: &str = "MetaPool";
const LINEAR_STR: &str = "Linear";
const XRHEA_STR: &str = "XRhea";
const RNEAR_STR: &str = "RNear";
const INTEAR_DEX_STR: &str = "IntearDex";

impl Display for DexId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DexId::Rhea => f.write_str(RHEA_STR),
            DexId::Aidols => f.write_str(AIDOLS_STR),
            DexId::Wrap => f.write_str(WRAP_STR),
            DexId::RheaDcl => f.write_str(RHEA_DCL_STR),
            DexId::MetaPool => f.write_str(METAPOOL_STR),
            DexId::Linear => f.write_str(LINEAR_STR),
            DexId::XRhea => f.write_str(XRHEA_STR),
            DexId::RNear => f.write_str(RNEAR_STR),
            DexId::IntearDex => f.write_str(INTEAR_DEX_STR),
            DexId::Hook(name) => f.write_str(name),
        }
    }
}

impl Serialize for DexId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DexId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for DexId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            RHEA_STR => DexId::Rhea,
            AIDOLS_STR => DexId::Aidols,
            WRAP_STR => DexId::Wrap,
            RHEA_DCL_STR => DexId::RheaDcl,
            METAPOOL_STR => DexId::MetaPool,
            LINEAR_STR => DexId::Linear,
            XRHEA_STR => DexId::XRhea,
            RNEAR_STR => DexId::RNear,
            INTEAR_DEX_STR => DexId::IntearDex,
            "" => return Err("Empty dex id".to_string()),
            name => DexId::Hook(name.into()),
        })
    }
}
