use intear_dex_types::{AssetId, ReferralFees};
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
    fees::payable_referral_fees,
    types::{Amount, SwapRequest, TokenId},
};

/// What a market maker's intent takes the other side of: the trader sends `asset_in` and gets
/// `asset_out`
#[derive(Clone)]
pub struct OtcTrade {
    pub asset_in: AssetId,
    pub asset_out: AssetId,
    /// The market maker's share of the swap: the engine takes the app fees out of an exact-in input
    /// and adds them on top of an exact-out one. `AmountIn` is what the market maker gets,
    /// `AmountOut` what it gives.
    pub amount: Amount,
    pub trader_account_id: AccountId,
    /// The app fees with the aggregator's share, as the engine pays them
    pub referral_fees: ReferralFees,
}

impl OtcTrade {
    /// None when the swap can't go through the OTC DEX: without a trader to whitelist the intent
    /// to, or with a token that isn't NEAR or a NEP-141 in the trader's wallet
    pub fn of_request(request: &SwapRequest) -> Option<Self> {
        let trader_account_id = request.trader_account_id.clone()?;
        let asset_in = wallet_asset(&request.token_in)?;
        let asset_out = wallet_asset(&request.token_out)?;
        if asset_in == asset_out {
            return None;
        }
        let referral_fees = ReferralFees::from(&payable_referral_fees(&request.fees));
        let amount = match request.amount {
            Amount::AmountIn(amount_in) => Amount::AmountIn(
                amount_in
                    - referral_fees
                        .amounts_out_of(U128(amount_in))
                        .iter()
                        .map(|(_, fee)| fee.0)
                        .sum::<u128>(),
            ),
            Amount::AmountOut(amount_out) => Amount::AmountOut(amount_out),
        };
        Some(Self {
            asset_in,
            asset_out,
            amount,
            trader_account_id,
            referral_fees,
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

    /// Whether `maker_trade` takes exactly the other side of this trade
    pub fn check_maker_trade(&self, maker_trade: &TradeIntent) -> Result<(), String> {
        if maker_trade.asset_in != self.asset_out || maker_trade.asset_out != self.asset_in {
            return Err(format!(
                "The intent trades {} for {} on a {} to {} swap",
                maker_trade.asset_in, maker_trade.asset_out, self.asset_in, self.asset_out,
            ));
        }
        let quoted_requested_amount = match self.amount {
            Amount::AmountIn(maker_gets) => maker_trade.amount_out.0 == maker_gets,
            Amount::AmountOut(maker_gives) => maker_trade.amount_in.0 == maker_gives,
        };
        if !quoted_requested_amount {
            return Err(format!(
                "The intent {maker_trade:?} isn't for {:?}",
                self.amount
            ));
        }
        Ok(())
    }
}

/// The asset a wallet token is deposited into Intear DEX as. Other locations aren't supported yet.
fn wallet_asset(token_id: &TokenId) -> Option<AssetId> {
    match token_id {
        TokenId::Near => Some(AssetId::Near),
        TokenId::Nep141(account_id) => Some(AssetId::Nep141(account_id.clone())),
        TokenId::Nep141OnRhea(_) | TokenId::TokenOnIntearDex(_) => None,
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

    fn request(query: &str) -> SwapRequest {
        serde_json::from_str(query).unwrap()
    }

    #[test]
    fn exact_in_trade_is_what_is_left_after_app_fees() {
        let otc_trade = OtcTrade::of_request(&request(
            r#"{"token_in":"near","token_out":"nep141:usdc.near","amount_in":"1000000","slippage_type":"Fixed","slippage":"0.01","trader_account_id":"trader.near","fees":"{\"app.near\":0.01}"}"#,
        ))
        .unwrap();
        assert_eq!(otc_trade.asset_in, AssetId::Near);
        assert_eq!(otc_trade.asset_out, usdc());
        // 1% of the input: 0.8% to the app, 0.2% to the aggregator
        assert_eq!(otc_trade.amount, Amount::AmountIn(990_000));
        assert_eq!(
            otc_trade.referral_fees.0,
            vec![
                ("app.near".parse().unwrap(), 8_000),
                ("dex-aggregator.intear.near".parse().unwrap(), 2_000),
            ]
        );

        let maker_trade =
            otc_trade.maker_trade_intent("maker.near".parse().unwrap(), 42, Validity::default());
        // The market maker gives what the trader gets, for what's left of the trader's input
        assert_eq!(maker_trade.asset_in, usdc());
        assert_eq!(maker_trade.amount_in, U128(42));
        assert_eq!(maker_trade.asset_out, AssetId::Near);
        assert_eq!(maker_trade.amount_out, U128(990_000));
        otc_trade.check_maker_trade(&maker_trade).unwrap();

        let mut whole_input = maker_trade.clone();
        whole_input.amount_out = U128(1_000_000);
        assert!(otc_trade.check_maker_trade(&whole_input).is_err());
        let mut same_direction = maker_trade;
        (same_direction.asset_in, same_direction.asset_out) =
            (same_direction.asset_out, same_direction.asset_in);
        assert!(otc_trade.check_maker_trade(&same_direction).is_err());
    }

    #[test]
    fn exact_out_trade_is_the_requested_output() {
        let otc_trade = OtcTrade::of_request(&request(
            r#"{"token_in":"nep141:usdc.near","token_out":"near","amount_out":"7","slippage_type":"Fixed","slippage":"0.01","trader_account_id":"trader.near","fees":"{\"app.near\":0.01}"}"#,
        ))
        .unwrap();
        assert_eq!(otc_trade.amount, Amount::AmountOut(7));
        let maker_trade =
            otc_trade.maker_trade_intent("maker.near".parse().unwrap(), 30, Validity::default());
        assert_eq!(maker_trade.asset_in, AssetId::Near);
        assert_eq!(maker_trade.amount_in, U128(7));
        assert_eq!(maker_trade.asset_out, usdc());
        assert_eq!(maker_trade.amount_out, U128(30));
        otc_trade.check_maker_trade(&maker_trade).unwrap();
    }

    #[test]
    fn no_trade_without_a_trader_or_for_tokens_outside_the_wallet() {
        let request_without = |field: &str| {
            let mut request: serde_json::Value = serde_json::from_str(
                r#"{"token_in":"near","token_out":"nep141:usdc.near","amount_in":"1","slippage_type":"Fixed","slippage":"0.01","trader_account_id":"trader.near"}"#,
            )
            .unwrap();
            match field {
                "trader" => {
                    request.as_object_mut().unwrap().remove("trader_account_id");
                }
                token => request["token_in"] = token.into(),
            }
            serde_json::from_value::<SwapRequest>(request).unwrap()
        };
        assert!(OtcTrade::of_request(&request_without("trader")).is_none());
        assert!(OtcTrade::of_request(&request_without("rhea-nep141:usdc.near")).is_none());
        assert!(OtcTrade::of_request(&request_without("intear-dex:near")).is_none());
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
