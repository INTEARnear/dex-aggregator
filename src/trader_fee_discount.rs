use axum::{
    extract::{Json, Query},
    http::StatusCode,
};
use intear_dex_common::FeeDiscountFraction;
use intear_dex_types::{
    FeeDiscount, SignedFeeDiscount, TraderFeeDiscount, MAX_TRADER_FEE_DISCOUNT_BLOCKS,
};
use near_min_api::types::{AccountId, BlockHeight};
use serde::{Deserialize, Serialize};
use swap_router_types::fee_discount::FeeDiscounter;

use crate::{
    shared_utils::{Mainnet, NetworkView},
    tear_stakes,
};

#[derive(Deserialize)]
pub struct TraderFeeDiscountRequest {
    trader_id: AccountId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TraderFeeDiscountResponse {
    trader_id: AccountId,
    /// Millionths of the pool and protocol fees that the trader's swaps don't pay
    fee_discount: u32,
    /// The last block the discount can be used in, as many blocks after the current one as a
    /// discount can be valid for
    expiration_block_height: BlockHeight,
    /// The `fee_discount` argument of `execute_operations` or `swap_simple` on Intear DEX, which
    /// takes the discount off every swap of the call. A trader discount is only taken in calls
    /// that the trader makes, not in `*_transfer_call` or `deposit_near`.
    signed_fee_discount: SignedFeeDiscount,
}

pub async fn fee_discount_handler(
    Query(TraderFeeDiscountRequest { trader_id }): Query<TraderFeeDiscountRequest>,
) -> Result<Json<TraderFeeDiscountResponse>, (StatusCode, String)> {
    let fee_discount = tear_stakes::fetch_fee_discount(&trader_id)
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to get the TEAR stake of {trader_id}: {err}"),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("{trader_id} doesn't stake enough TEAR for a fee discount"),
            )
        })?;
    let block_height = Mainnet
        .current_block_height()
        .map_err(|err| (StatusCode::SERVICE_UNAVAILABLE, err))?;
    Ok(Json(signed_fee_discount(
        FeeDiscounter::signing(),
        trader_id,
        fee_discount,
        block_height,
    )))
}

fn signed_fee_discount(
    fee_discounter: &FeeDiscounter,
    trader_id: AccountId,
    fee_discount: FeeDiscountFraction,
    block_height: BlockHeight,
) -> TraderFeeDiscountResponse {
    let discount = TraderFeeDiscount {
        trader_id: trader_id.clone(),
        fee_discount,
        expiration_block_height: block_height + MAX_TRADER_FEE_DISCOUNT_BLOCKS,
    };
    TraderFeeDiscountResponse {
        trader_id,
        fee_discount: fee_discount.get(),
        expiration_block_height: discount.expiration_block_height,
        signed_fee_discount: fee_discounter.sign(FeeDiscount::ForTrader(discount)),
    }
}

#[cfg(test)]
mod tests {
    use near_min_api::types::near_crypto::{KeyType, SecretKey, Signature};
    use sha2::{Digest, Sha256};
    use swap_router_types::fee_discount::fee_discount_signature_payload;

    use super::*;

    #[test]
    fn signs_a_discount_for_every_swap_of_the_trader() {
        let fee_discounter = FeeDiscounter {
            secret_key: SecretKey::from_seed(KeyType::ED25519, "fee discounter"),
        };
        let response = signed_fee_discount(
            &fee_discounter,
            "trader.near".parse().unwrap(),
            FeeDiscountFraction::new(250_000),
            1_000_000,
        );
        assert_eq!(response.fee_discount, 250_000);
        assert_eq!(response.expiration_block_height, 2_008_000);
        let signed = &response.signed_fee_discount;
        assert_eq!(
            signed.discount,
            FeeDiscount::ForTrader(TraderFeeDiscount {
                trader_id: "trader.near".parse().unwrap(),
                fee_discount: FeeDiscountFraction::new(250_000),
                expiration_block_height: 2_008_000,
            })
        );
        assert!(Signature::from_parts(KeyType::ED25519, &signed.signature.0)
            .unwrap()
            .verify(
                &Sha256::digest(
                    borsh::to_vec(&fee_discount_signature_payload(&signed.discount)).unwrap()
                ),
                &fee_discounter.secret_key.public_key(),
            ));
    }
}
