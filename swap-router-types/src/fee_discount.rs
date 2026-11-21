use std::sync::OnceLock;

use intear_dex_types::{
    FeeDiscount, FeeDiscountSignaturePayload, Operation, OperationsFeeDiscount, SignedFeeDiscount,
};
use near_min_api::types::{
    near_crypto::{SecretKey, Signature},
    AccountId, BlockHeight,
};
use near_sdk::json_types::Base64VecU8;
use sha2::{Digest, Sha256};

use crate::{hook::QuoteFeeDiscount, types::Gas};

/// How long a quote's fee discount can be used, ~10 minutes
pub const FEE_DISCOUNT_VALIDITY_BLOCKS: BlockHeight = 1000;
/// What Intear DEX burns to check the signature of a fee discount and the hash of the operations
/// it's for, once for all their swaps. Simulated swaps burnt 0.34 Tgas more with an ed25519 signer,
/// 0.42 with secp256k1 and 1.14 with ML-DSA-65.
pub const FEE_DISCOUNT_GAS: Gas = Gas::from_ggas(450, 1_200);

static FEE_DISCOUNTER: OnceLock<FeeDiscounter> = OnceLock::new();

/// Signs fee discounts with the key set as the fee discount signer of the engine
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

    /// Makes this the signer of [`FeeDiscounter::signing`], which can only be done once. Gives
    /// itself back if there already is one.
    pub fn start(self) -> Result<(), Self> {
        FEE_DISCOUNTER.set(self)
    }

    pub fn signing() -> &'static Self {
        FEE_DISCOUNTER
            .get()
            .expect("The fee discounter is started before routes are built")
    }

    /// The discount on every swap of `operations` of `trader_id`, signed
    pub fn sign_operations(
        &self,
        operations: &[Operation],
        trader_id: Option<AccountId>,
        discount: QuoteFeeDiscount,
    ) -> SignedFeeDiscount {
        let operations_hash = Sha256::digest(near_sdk::borsh::to_vec(operations).unwrap());
        self.sign(FeeDiscount::ForOperations(OperationsFeeDiscount {
            operations_hash: <[u8; 32]>::from(operations_hash).into(),
            trader_id,
            fee_discount: discount.fee_discount,
            expiration_block_height: discount.expiration_block_height,
        }))
    }

    /// Signs sha256(borsh([`FeeDiscountSignaturePayload`])) of `discount`, which the engine
    /// verifies
    pub fn sign(&self, discount: FeeDiscount) -> SignedFeeDiscount {
        let payload = fee_discount_signature_payload(&discount);
        let hash = Sha256::digest(near_sdk::borsh::to_vec(&payload).unwrap());
        let signature = match self.secret_key.sign(&hash) {
            Signature::ED25519(signature) => signature.to_bytes().to_vec(),
            // Borsh puts the key type before the 65 bytes of r, s, and the recovery id
            signature @ Signature::SECP256K1(_) => {
                near_sdk::borsh::to_vec(&signature).unwrap()[1..].to_vec()
            }
            Signature::MLDSA65(signature) => signature.0.to_vec(),
        };
        SignedFeeDiscount {
            discount,
            signature: Base64VecU8(signature),
        }
    }
}

/// For mainnet Intear DEX
pub fn fee_discount_signature_payload(discount: &FeeDiscount) -> FeeDiscountSignaturePayload {
    FeeDiscountSignaturePayload {
        chain_id: "mainnet".to_string(),
        engine_contract_id: "dex.intear.near".parse().unwrap(),
        discount: discount.clone(),
    }
}

#[cfg(test)]
mod tests {
    use intear_dex_common::{AssetId, DexId, FeeDiscountFraction};
    use intear_dex_types::{SwapOperationAmount, TraderFeeDiscount};
    use near_min_api::types::near_crypto::KeyType;

    use super::*;

    fn fee_discounter() -> FeeDiscounter {
        FeeDiscounter {
            secret_key: SecretKey::from_seed(KeyType::ED25519, "fee discounter"),
        }
    }

    fn is_signed(signed: &SignedFeeDiscount) -> bool {
        Signature::from_parts(KeyType::ED25519, &signed.signature.0)
            .unwrap()
            .verify(
                &Sha256::digest(
                    near_sdk::borsh::to_vec(&fee_discount_signature_payload(&signed.discount))
                        .unwrap(),
                ),
                &fee_discounter().secret_key.public_key(),
            )
    }

    #[test]
    fn signs_the_fee_discount_of_operations_for_their_hash() {
        let operations = vec![Operation::SwapSimple {
            dex_id: "slimedragon.near/xyk".parse::<DexId>().unwrap(),
            message: Base64VecU8(vec![1, 0, 0, 0]),
            asset_in: AssetId::Near,
            asset_out: AssetId::Nep141("ft".parse().unwrap()),
            amount: SwapOperationAmount::OutputOfLastIn,
            constraint: None,
            quote_improvement_fee: None,
        }];
        let signed = fee_discounter().sign_operations(
            &operations,
            Some("trader.near".parse().unwrap()),
            QuoteFeeDiscount {
                fee_discount: FeeDiscountFraction::new(250_000),
                expiration_block_height: 1_001_000,
            },
        );
        let FeeDiscount::ForOperations(discount) = &signed.discount else {
            panic!("Not a fee discount of operations");
        };
        assert_eq!(
            discount.operations_hash,
            near_sdk::json_types::Base58CryptoHash::from(<[u8; 32]>::from(Sha256::digest(
                near_sdk::borsh::to_vec(&operations).unwrap()
            )))
        );
        assert_eq!(discount.trader_id, Some("trader.near".parse().unwrap()));
        assert_eq!(discount.fee_discount.get(), 250_000);
        assert_eq!(discount.expiration_block_height, 1_001_000);
        assert!(is_signed(&signed));
    }

    #[test]
    fn signs_a_trader_fee_discount() {
        let signed = fee_discounter().sign(FeeDiscount::ForTrader(TraderFeeDiscount {
            trader_id: "trader.near".parse().unwrap(),
            fee_discount: FeeDiscountFraction::new(250_000),
            expiration_block_height: 2_008_000,
        }));
        assert!(is_signed(&signed));
    }
}
