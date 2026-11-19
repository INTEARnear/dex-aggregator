use std::{collections::BTreeMap, sync::Arc};

use axum::{
    extract::{Json, Query},
    http::StatusCode,
    Extension,
};
use intear_dex_common::{
    FeeDiscountFraction, SwapFeeDiscount, TraderFeeDiscount, MAX_TRADER_FEE_DISCOUNT_BLOCKS,
};
use near_min_api::types::{AccountId, BlockHeight};
use near_sdk::json_types::Base64VecU8;
use serde::{Deserialize, Serialize};
use swap_router_types::fee_discount::FeeDiscounter;

use crate::{
    shared_utils::{Mainnet, NetworkView},
    tear_stakes,
};

/// The DEXes whose fee discount signer signs the discounts
const FEE_DISCOUNT_DEXES: [&str; 2] =
    [pathfinder::plach::PLACH_DEX_ID, pathfinder::qoq::QOQ_DEX_ID];

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
    /// The fee discount of a swap message of each DEX, borsh of its `SwapFeeDiscount`. A trader
    /// discount is only taken in swaps that the trader calls `execute_operations` for, not in
    /// `*_transfer_call`.
    fee_discounts: BTreeMap<String, Base64VecU8>,
}

pub async fn fee_discount_handler(
    Extension(fee_discounter): Extension<Arc<FeeDiscounter>>,
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
        &fee_discounter,
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
    let fee_discounts = FEE_DISCOUNT_DEXES
        .into_iter()
        .map(|dex_id| {
            let signed = fee_discounter.sign_trader(&dex_id.parse().unwrap(), discount.clone());
            // The route of a trader discount isn't signed, so it's the same for any route type
            let swap_fee_discount = SwapFeeDiscount::<()>::ForTrader(signed);
            (
                dex_id.to_string(),
                Base64VecU8(borsh::to_vec(&swap_fee_discount).unwrap()),
            )
        })
        .collect();
    TraderFeeDiscountResponse {
        trader_id,
        fee_discount: fee_discount.get(),
        expiration_block_height: discount.expiration_block_height,
        fee_discounts,
    }
}

#[cfg(test)]
mod tests {
    use intear_dex_common::{SignedTraderFeeDiscount, SwapFeeDiscount};
    use near_min_api::types::near_crypto::{KeyType, SecretKey, Signature};
    use sha2::{Digest, Sha256};
    use swap_router_types::fee_discount::trader_fee_discount_signature_payload;
    use xyk_dex_types::{SwapArgs, SwapRoute, FULL_ROUTE_FRACTION};

    use super::*;

    #[test]
    fn signs_a_discount_for_every_swap_of_the_trader_on_each_dex() {
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
        assert_eq!(
            response.fee_discounts.keys().collect::<Vec<_>>(),
            ["slimedragon.near/amp", "slimedragon.near/xyk"]
        );
        for (dex_id, fee_discount) in &response.fee_discounts {
            // Goes in a swap message of the DEX as it is
            let message = [
                borsh::to_vec(&vec![SwapRoute {
                    fraction: FULL_ROUTE_FRACTION,
                    pool_ids: vec![xyk_dex_types::PoolId::new(1)],
                }])
                .unwrap(),
                fee_discount.0.clone(),
            ]
            .concat();
            let SwapArgs {
                fee_discount:
                    SwapFeeDiscount::ForTrader(SignedTraderFeeDiscount {
                        discount,
                        signature,
                    }),
                ..
            } = borsh::from_slice(&message).unwrap()
            else {
                panic!("Not a trader fee discount");
            };
            assert_eq!(
                discount.trader_id,
                "trader.near".parse::<AccountId>().unwrap()
            );
            assert_eq!(discount.fee_discount.get(), 250_000);
            assert!(Signature::from_parts(KeyType::ED25519, &signature)
                .unwrap()
                .verify(
                    &Sha256::digest(
                        borsh::to_vec(&trader_fee_discount_signature_payload(
                            &dex_id.parse().unwrap(),
                            &discount
                        ))
                        .unwrap()
                    ),
                    &fee_discounter.secret_key.public_key(),
                ));
        }
    }
}
