use std::{fmt::Display, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use near_min_api::{
    types::{near_crypto::PublicKey, AccountId, Action, Balance},
    utils::dec_format,
};
use serde::de::Error;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
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
}

impl TokenId {
    pub fn get_account_id(&self) -> AccountId {
        match self {
            TokenId::Near => "wrap.near".parse().unwrap(),
            TokenId::Nep141(account_id) => account_id.to_owned(),
            TokenId::Nep141OnRhea(account_id) => account_id.to_owned(),
        }
    }
}

impl Serialize for TokenId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            TokenId::Near => "near".to_string(),
            TokenId::Nep141(account_id) => format!("nep141:{account_id}"),
            TokenId::Nep141OnRhea(account_id) => format!("rhea-nep141:{account_id}"),
        }
        .serialize(serializer)
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
    /// The maximum amount of time to wait for the route to be found. Some dexes
    /// might show a better quote if you wait a bit longer.
    /// Usually, 2-3 seconds is enough. Maximum is 60 seconds.
    pub max_wait_ms: u64,
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
    /// parameter for DEXes that support it (DexId::Rhea, DexId::Aidols, DexId::Plach)
    pub referrer_id: Option<AccountId>,
}

mod comma_separated {
    use std::{fmt::Display, str::FromStr};

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, T>(value: &Option<Vec<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Display,
    {
        if let Some(value) = value {
            let s = value
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Some(s).serialize(serializer)
        } else {
            None::<String>.serialize(serializer)
        }
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

fn from_str<'de, D, S>(deserializer: D) -> Result<S, D::Error>
where
    D: serde::Deserializer<'de>,
    S: std::str::FromStr,
{
    let s = <&str as serde::Deserialize>::deserialize(deserializer)?;
    S::from_str(s).map_err(|_| D::Error::custom("could not parse string"))
}

fn to_str<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Route {
    /// The deadline for the route. If provided, you should refresh the route by
    /// calling the /route endpoint again 2-3 seconds before the deadline to account
    /// for network and block production latency.
    pub deadline: Option<DateTime<Utc>>,
    /// Whether the route has slippage. Usually it's true for AMM models like Rhea
    /// and false for OTC / guaranteed-quote models.
    pub has_slippage: bool,
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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ExecutionInstruction {
    /// Sign a transaction with the given actions and send it to the RPC.
    NearTransaction {
        receiver_id: AccountId,
        actions: Vec<Action>,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DexId {
    /// https://app.rhea.finance/
    /// AMM DEX
    ///
    /// Supports AmountIn, doesn't support AmountOut
    Rhea,
    /// https://aidols.bot/
    /// bonding-curve launchpad
    ///
    /// Supports both AmountIn and AmountOut, only *.aidols.near tokens
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
    /// https://metapool.app/
    /// Liquid Staking provider
    ///
    /// Supports NEAR -> STNEAR and STNEAR -> NEAR, both AmountIn and AmountOut
    MetaPool,
    /// https://linearprotocol.org/
    /// Liquid Staking provider
    ///
    /// Supports NEAR -> LiNEAR and LiNEAR -> NEAR, both AmountIn and AmountOut
    Linear,
    /// https://app.rhea.finance/stake
    /// Staked $RHEA
    ///
    /// Supports RHEA -> XRHEA and XRHEA -> RHEA, both AmountIn and AmountOut
    XRhea,
    /// https://app.rhea.finance/stake
    /// Liquid Staking Provider
    ///
    /// Supports NEAR -> rNEAR and rNEAR -> NEAR, both AmountIn and AmountOut
    RNear,
    /// https://dex.intea.rs/
    /// AMM DEX
    ///
    /// Supports both AmountIn and AmountOut
    Plach,
}

const RHEA_STR: &str = "Rhea";
const AIDOLS_STR: &str = "Aidols";
const WRAP_STR: &str = "Wrap";
const RHEA_DCL_STR: &str = "RheaDcl";
const METAPOOL_STR: &str = "MetaPool";
const LINEAR_STR: &str = "Linear";
const XRHEA_STR: &str = "XRhea";
const RNEAR_STR: &str = "RNear";
const PLACH_STR: &str = "Plach";

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
            DexId::Plach => f.write_str(PLACH_STR),
        }
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
            PLACH_STR => DexId::Plach,
            _ => return Err(format!("Invalid dex id: {}", s)),
        })
    }
}
