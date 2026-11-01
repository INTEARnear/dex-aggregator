use near_min_api::types::{
    AccountId, Balance, BlockHeight,
    near_crypto::{SecretKey, Signature},
};
use sha2::{Digest, Sha256};
use xyk_dex_types::{
    FULL_FEE_DISCOUNT, FeeDiscount, FeeDiscountFraction, FeeDiscountSignaturePayload,
    SignedFeeDiscount,
};

use crate::{INTEAR_DEX_CONTRACT_ID, PLACH_DEX_ID};

pub const TEAR_TOKEN_ID: &str = "token.intear.near";
pub const TEAR_STAKING_CONTRACT_ID: &str = "stakeyourtears.intear.near";
const ONE_TEAR: Balance = 10u128.pow(24);

const TEAR_FEE_DISCOUNTS: [(Balance, FeeDiscountFraction); 3] = [
    // 100,000 = 75% discount
    (
        100_000 * ONE_TEAR,
        FeeDiscountFraction::new(FULL_FEE_DISCOUNT.get() / 4 * 3),
    ),
    // 1,000 = 25% discount
    (
        1_000 * ONE_TEAR,
        FeeDiscountFraction::new(FULL_FEE_DISCOUNT.get() / 4),
    ),
    // 10 = 10% discount
    (
        10 * ONE_TEAR,
        FeeDiscountFraction::new(FULL_FEE_DISCOUNT.get() / 10),
    ),
];

/// How long a quote's fee discount can be used, ~10 minutes
pub const FEE_DISCOUNT_VALIDITY_BLOCKS: BlockHeight = 1000;

/// The discount of a trader who holds and stakes `tear_balance` TEAR
pub fn tear_fee_discount(tear_balance: Balance) -> Option<FeeDiscountFraction> {
    TEAR_FEE_DISCOUNTS
        .into_iter()
        .find(|(min_balance, _)| tear_balance >= *min_balance)
        .map(|(_, fee_discount)| fee_discount)
}

/// Signs fee discounts with the key set as the xyk DEX's fee discount signer
pub struct FeeDiscounter {
    pub(crate) secret_key: SecretKey,
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
            "Signing Plach fee discounts as {account_id} with {}",
            secret_key.public_key()
        );
        Ok(Self { secret_key })
    }

    /// Signs sha256(borsh([`FeeDiscountSignaturePayload`])) of `discount` for Plach on mainnet
    /// Intear DEX, which the DEX verifies
    pub fn sign(&self, discount: FeeDiscount) -> SignedFeeDiscount {
        let hash =
            Sha256::digest(borsh::to_vec(&fee_discount_signature_payload(&discount)).unwrap());
        let signature = match self.secret_key.sign(&hash) {
            Signature::ED25519(signature) => signature.to_bytes().to_vec(),
            // Borsh puts the key type before the 65 bytes of r, s, and the recovery id
            signature @ Signature::SECP256K1(_) => borsh::to_vec(&signature).unwrap()[1..].to_vec(),
            Signature::MLDSA65(_) => panic!("The DEX can't verify ML-DSA-65 fee discounts"),
        };
        SignedFeeDiscount {
            discount,
            signature,
        }
    }
}

pub fn fee_discount_signature_payload(discount: &FeeDiscount) -> FeeDiscountSignaturePayload {
    FeeDiscountSignaturePayload {
        chain_id: "mainnet".to_string(),
        engine_contract_id: INTEAR_DEX_CONTRACT_ID.parse().unwrap(),
        dex_id: PLACH_DEX_ID.parse().unwrap(),
        discount: discount.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_discount_grows_with_tear_holdings() {
        let fee_discount_of =
            |tear_balance| tear_fee_discount(tear_balance).map(FeeDiscountFraction::get);
        assert_eq!(fee_discount_of(0), None);
        assert_eq!(fee_discount_of(10 * ONE_TEAR - 1), None);
        assert_eq!(fee_discount_of(10 * ONE_TEAR), Some(100_000));
        assert_eq!(fee_discount_of(1_000 * ONE_TEAR - 1), Some(100_000));
        assert_eq!(fee_discount_of(1_000 * ONE_TEAR), Some(250_000));
        assert_eq!(fee_discount_of(100_000 * ONE_TEAR), Some(750_000));
        assert_eq!(fee_discount_of(u128::MAX), Some(750_000));
    }
}
