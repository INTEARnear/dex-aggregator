use intear_dex_types::AssetId;
use near_min_api::types::{
    near_crypto::{SecretKey, Signature},
    AccountId,
};
use near_sdk::json_types::{Base64VecU8, U128};
use otc_dex_types::{
    AuthorizationMethod, AuthorizedTradeIntent, TradeIntent, TradeIntentSignaturePayload, Validity,
};
use sha2::{Digest, Sha256};

use crate::{
    hook::IntearDexTrade,
    types::{Amount, Gas},
};

pub const OTC_SWAP_GAS: Gas = Gas::from_ggas(22_000, 50_000);

/// What a market maker's intent takes the other side of: the trader sends `asset_in` and gets
/// `asset_out`
#[derive(Clone)]
pub struct OtcTrade {
    pub asset_in: AssetId,
    pub asset_out: AssetId,
    /// The market maker's share of the swap, after the app fees: `AmountIn` is what the market
    /// maker gets, `AmountOut` what it gives
    pub amount: Amount,
    pub trader_account_id: AccountId,
}

impl OtcTrade {
    /// None without a trader to whitelist the intent to
    pub fn of_trade(trade: &IntearDexTrade) -> Option<Self> {
        Some(Self {
            asset_in: trade.asset_in.clone(),
            asset_out: trade.asset_out.clone(),
            amount: trade.amount,
            trader_account_id: trade.trader_account_id.clone()?,
        })
    }

    /// The market maker's side of the trade, at `quoted_amount`: what the market maker gives for an
    /// exact-in swap, or wants for an exact-out one
    pub fn maker_trade_intent(
        &self,
        maker_account_id: AccountId,
        quoted_amount: u128,
        validity: Validity,
    ) -> TradeIntent {
        let (maker_gets, maker_gives) = match self.amount {
            Amount::AmountIn(maker_gets) => (maker_gets, quoted_amount),
            Amount::AmountOut(maker_gives) => (quoted_amount, maker_gives),
        };
        TradeIntent {
            user_id: maker_account_id,
            asset_in: self.asset_out.clone(),
            amount_in: U128(maker_gives),
            asset_out: self.asset_in.clone(),
            amount_out: U128(maker_gets),
            validity,
        }
    }
}

/// Signs `trade_intent` the way the OTC DEX `otc_dex_id` on mainnet's Intear DEX checks it, with a
/// `nonce` the intent can't be filled with again while it's valid. Only ED25519 and SECP256K1 keys
/// can sign for the OTC DEX.
pub fn sign_intent(
    secret_key: &SecretKey,
    otc_dex_id: &intear_dex_types::DexId,
    trade_intent: TradeIntent,
    nonce: u128,
) -> AuthorizedTradeIntent {
    let payload = TradeIntentSignaturePayload {
        chain_id: "mainnet".to_string(),
        engine_contract_id: "dex.intear.near".parse().unwrap(),
        dex_id: otc_dex_id.clone(),
        trade_intent,
        nonce: U128(nonce),
    };
    let payload_hash = Sha256::digest(near_sdk::borsh::to_vec(&payload).unwrap());
    let signature = match secret_key.sign(&payload_hash) {
        Signature::ED25519(signature) => signature.to_bytes().to_vec(),
        // Borsh puts the key type before the 65 bytes of r, s, and the recovery id
        signature @ Signature::SECP256K1(_) => {
            near_sdk::borsh::to_vec(&signature).unwrap()[1..].to_vec()
        }
        Signature::MLDSA65(_) => panic!("The OTC DEX can't verify ML-DSA-65 signatures"),
    };
    AuthorizedTradeIntent {
        trade_intent: payload.trade_intent,
        authorization_method: AuthorizationMethod::Signature {
            signature: Base64VecU8(signature),
            nonce: payload.nonce,
        },
    }
}

#[cfg(test)]
mod tests {
    use near_min_api::types::near_crypto::KeyType;
    use otc_dex_types::ExpiryCondition;

    use super::*;

    fn usdc() -> AssetId {
        AssetId::Nep141("usdc.near".parse().unwrap())
    }

    fn trade(amount: Amount, trader_account_id: Option<&str>) -> IntearDexTrade {
        IntearDexTrade {
            asset_in: AssetId::Near,
            asset_out: usdc(),
            amount,
            slippage: "0.01".parse().unwrap(),
            trader_account_id: trader_account_id.map(|id| id.parse().unwrap()),
        }
    }

    #[test]
    fn exact_in_maker_gives_what_the_trader_gets() {
        let otc_trade =
            OtcTrade::of_trade(&trade(Amount::AmountIn(990_000), Some("trader.near"))).unwrap();
        let maker_trade =
            otc_trade.maker_trade_intent("maker.near".parse().unwrap(), 42, Validity::default());
        assert_eq!(maker_trade.asset_in, usdc());
        assert_eq!(maker_trade.amount_in, U128(42));
        assert_eq!(maker_trade.asset_out, AssetId::Near);
        assert_eq!(maker_trade.amount_out, U128(990_000));
    }

    #[test]
    fn exact_out_maker_wants_the_quoted_input() {
        let otc_trade =
            OtcTrade::of_trade(&trade(Amount::AmountOut(7), Some("trader.near"))).unwrap();
        let maker_trade =
            otc_trade.maker_trade_intent("maker.near".parse().unwrap(), 30, Validity::default());
        assert_eq!(maker_trade.amount_in, U128(7));
        assert_eq!(maker_trade.amount_out, U128(30));
    }

    #[test]
    fn no_trade_without_a_trader() {
        assert!(OtcTrade::of_trade(&trade(Amount::AmountIn(1), None)).is_none());
    }
    #[test]
    fn signs_the_payload_the_otc_dex_verifies_with_both_key_types() {
        let otc_dex_id: intear_dex_types::DexId = "owner.near/otc".parse().unwrap();
        let trade_intent = TradeIntent {
            user_id: "maker.near".parse().unwrap(),
            asset_in: usdc(),
            asset_out: AssetId::Near,
            amount_in: U128(1),
            amount_out: U128(2),
            validity: Validity {
                expiry: Some(ExpiryCondition::BlockHeight(100)),
                only_for_whitelisted_parties: Some(vec!["trader.near".parse().unwrap()]),
            },
        };
        for key_type in [KeyType::ED25519, KeyType::SECP256K1] {
            let secret_key = SecretKey::from_seed(key_type, "maker");
            let AuthorizedTradeIntent {
                authorization_method: AuthorizationMethod::Signature { signature, nonce },
                ..
            } = sign_intent(&secret_key, &otc_dex_id, trade_intent.clone(), 7)
            else {
                panic!("Not signed");
            };
            assert_eq!(nonce, U128(7));
            let payload = TradeIntentSignaturePayload {
                chain_id: "mainnet".to_string(),
                engine_contract_id: "dex.intear.near".parse().unwrap(),
                dex_id: otc_dex_id.clone(),
                trade_intent: trade_intent.clone(),
                nonce: U128(7),
            };
            let payload_hash = Sha256::digest(near_sdk::borsh::to_vec(&payload).unwrap());
            assert!(
                Signature::from_parts(key_type, &signature.0)
                    .unwrap()
                    .verify(&payload_hash, &secret_key.public_key()),
                "{key_type} signature doesn't verify"
            );
        }
    }
}
