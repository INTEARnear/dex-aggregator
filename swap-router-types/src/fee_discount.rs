use intear_dex_common::{
    DexId, FeeDiscount, FeeDiscountFraction, FeeDiscountSignaturePayload, SignedFeeDiscount,
    SignedTraderFeeDiscount, SwapFeeDiscount, SwapRequestAmount, TokenAmount, TraderFeeDiscount,
    TraderFeeDiscountSignaturePayload,
};
use near_min_api::types::{
    near_crypto::{SecretKey, Signature},
    AccountId, BlockHeight,
};
use near_sdk::borsh::BorshSerialize;
use sha2::{Digest, Sha256};

use crate::{hook::HookSwapRequest, types::Amount};

/// How long a quote's fee discount can be used, ~10 minutes
pub const FEE_DISCOUNT_VALIDITY_BLOCKS: BlockHeight = 1000;

/// Signs fee discounts with the key set as the fee discount signer of the DEXes
pub struct FeeDiscounter {
    pub secret_key: SecretKey,
}

impl FeeDiscounter {
    pub fn from_env() -> Result<Self, String> {
        let env = |name: &str| std::env::var(name).map_err(|_| format!("{name} must be set"));
        let account_id: AccountId = env("FEE_DISCOUNTER_ACCOUNT_ID")?
            .parse()
            .map_err(|err| format!("Invalid FEE_DISCOUNTER_ACCOUNT_ID: {err}"))?;
        let secret_key: SecretKey = env("FEE_DISCOUNTER_PRIVATE_KEY")?
            .parse()
            .map_err(|err| format!("Invalid FEE_DISCOUNTER_PRIVATE_KEY: {err}"))?;
        eprintln!(
            "Signing fee discounts as {account_id} with {}",
            secret_key.public_key()
        );
        Ok(Self { secret_key })
    }

    /// The fee discount of the swap of `request` on `route` of the DEX `dex_id`, signed
    pub fn sign_swap<Route: BorshSerialize + Clone>(
        &self,
        dex_id: &DexId,
        route: Route,
        request: &HookSwapRequest,
    ) -> Result<SwapFeeDiscount<Route>, String> {
        let Some(fee_discount) = request.fee_discount else {
            return Ok(SwapFeeDiscount::None);
        };
        // The DEX checks that the swap is of the amount it was signed for
        let amount = request
            .amount
            .ok_or("A swap of what the swap before it gave can't have a fee discount")?;
        let discount = FeeDiscount {
            route,
            asset_in: request.asset_in.clone(),
            asset_out: request.asset_out.clone(),
            trader_id: request.trader_account_id.clone(),
            amount: match amount {
                Amount::AmountIn(amount_in) => {
                    SwapRequestAmount::ExactIn(TokenAmount::new(amount_in))
                }
                Amount::AmountOut(amount_out) => {
                    SwapRequestAmount::ExactOut(TokenAmount::new(amount_out))
                }
            },
            fee_discount: FeeDiscountFraction::new(fee_discount),
            expiration_block_height: request.block_height + FEE_DISCOUNT_VALIDITY_BLOCKS,
        };
        Ok(SwapFeeDiscount::ForSwap(SignedFeeDiscount {
            signature: self.sign(&fee_discount_signature_payload(dex_id, &discount)),
            discount,
        }))
    }

    /// The discount on every swap of its trader on the DEX `dex_id`, signed
    pub fn sign_trader(
        &self,
        dex_id: &DexId,
        discount: TraderFeeDiscount,
    ) -> SignedTraderFeeDiscount {
        SignedTraderFeeDiscount {
            signature: self.sign(&trader_fee_discount_signature_payload(dex_id, &discount)),
            discount,
        }
    }

    /// Signs sha256(borsh(`payload`)), which the DEX verifies
    fn sign(&self, payload: &impl BorshSerialize) -> Vec<u8> {
        let hash = Sha256::digest(near_sdk::borsh::to_vec(payload).unwrap());
        match self.secret_key.sign(&hash) {
            Signature::ED25519(signature) => signature.to_bytes().to_vec(),
            // Borsh puts the key type before the 65 bytes of r, s, and the recovery id
            signature @ Signature::SECP256K1(_) => {
                near_sdk::borsh::to_vec(&signature).unwrap()[1..].to_vec()
            }
            Signature::MLDSA65(_) => panic!("The DEX can't verify ML-DSA-65 fee discounts"),
        }
    }
}

/// For `dex_id` on mainnet Intear DEX
pub fn trader_fee_discount_signature_payload(
    dex_id: &DexId,
    discount: &TraderFeeDiscount,
) -> TraderFeeDiscountSignaturePayload {
    TraderFeeDiscountSignaturePayload {
        chain_id: "mainnet".to_string(),
        engine_contract_id: "dex.intear.near".parse().unwrap(),
        dex_id: dex_id.clone(),
        discount: discount.clone(),
    }
}

/// For `dex_id` on mainnet Intear DEX
pub fn fee_discount_signature_payload<Route: Clone>(
    dex_id: &DexId,
    discount: &FeeDiscount<Route>,
) -> FeeDiscountSignaturePayload<Route> {
    FeeDiscountSignaturePayload {
        chain_id: "mainnet".to_string(),
        engine_contract_id: "dex.intear.near".parse().unwrap(),
        dex_id: dex_id.clone(),
        discount: discount.clone(),
    }
}

#[cfg(test)]
mod tests {
    use intear_dex_common::AssetId;
    use near_min_api::types::near_crypto::KeyType;

    use super::*;
    use crate::hook::HookSwapRoute;

    fn fee_discounter() -> FeeDiscounter {
        FeeDiscounter {
            secret_key: SecretKey::from_seed(KeyType::ED25519, "fee discounter"),
        }
    }

    fn request(amount: Option<Amount>, fee_discount: Option<u32>) -> HookSwapRequest {
        HookSwapRequest {
            asset_in: AssetId::Near,
            asset_out: AssetId::Nep141("ft".parse().unwrap()),
            routes: vec![HookSwapRoute {
                fraction: 1_000_000,
                pools: vec![1],
            }],
            amount,
            fee_discount,
            trader_account_id: Some("trader.near".parse().unwrap()),
            block_height: 1_000_000,
        }
    }

    #[test]
    fn signs_the_fee_discount_of_a_swap_for_its_dex() {
        let dex_id: DexId = "slimedragon.near/xyk".parse().unwrap();
        let SwapFeeDiscount::ForSwap(signed) = fee_discounter()
            .sign_swap(
                &dex_id,
                b"route".to_vec(),
                &request(Some(Amount::AmountIn(100)), Some(250_000)),
            )
            .unwrap()
        else {
            panic!("Not a fee discount of the swap");
        };
        let discount = &signed.discount;
        assert_eq!(discount.route, b"route".to_vec());
        assert_eq!(discount.asset_in, AssetId::Near);
        assert_eq!(discount.asset_out, AssetId::Nep141("ft".parse().unwrap()));
        assert_eq!(discount.trader_id, Some("trader.near".parse().unwrap()));
        assert_eq!(
            discount.amount,
            SwapRequestAmount::ExactIn(TokenAmount::new(100))
        );
        assert_eq!(discount.fee_discount.get(), 250_000);
        assert_eq!(discount.expiration_block_height, 1_001_000);
        assert!(Signature::from_parts(KeyType::ED25519, &signed.signature)
            .unwrap()
            .verify(
                &Sha256::digest(
                    near_sdk::borsh::to_vec(&fee_discount_signature_payload(&dex_id, discount))
                        .unwrap()
                ),
                &fee_discounter().secret_key.public_key(),
            ));
    }

    #[test]
    fn swaps_without_a_known_amount_have_no_fee_discount() {
        let dex_id: DexId = "slimedragon.near/xyk".parse().unwrap();
        let sign = |request| fee_discounter().sign_swap(&dex_id, b"route".to_vec(), &request);
        assert!(matches!(
            sign(request(None, None)),
            Ok(SwapFeeDiscount::None)
        ));
        assert!(matches!(
            sign(request(Some(Amount::AmountIn(100)), None)),
            Ok(SwapFeeDiscount::None)
        ));
        assert!(sign(request(None, Some(250_000))).is_err());
    }

    #[test]
    fn signs_a_trader_fee_discount_for_its_dex() {
        let dex_id: DexId = "slimedragon.near/amp".parse().unwrap();
        let discount = TraderFeeDiscount {
            trader_id: "trader.near".parse().unwrap(),
            fee_discount: FeeDiscountFraction::new(250_000),
            expiration_block_height: 2_008_000,
        };
        let signed = fee_discounter().sign_trader(&dex_id, discount.clone());
        assert_eq!(signed.discount.trader_id, discount.trader_id);
        assert!(Signature::from_parts(KeyType::ED25519, &signed.signature)
            .unwrap()
            .verify(
                &Sha256::digest(
                    near_sdk::borsh::to_vec(&trader_fee_discount_signature_payload(
                        &dex_id, &discount
                    ))
                    .unwrap()
                ),
                &fee_discounter().secret_key.public_key(),
            ));
    }
}
