use intear_dex_common::{FeeFraction, TokenAmount, FULL_FEE_FRACTION};
use intear_dex_types::ReferralFees;
use near_min_api::types::AccountIdRef;

use crate::types::QuoteFees;

pub const AGGREGATOR_FEE_RECEIVER_ID: &AccountIdRef =
    AccountIdRef::new_or_panic("dex-aggregator.intear.near");
/// The part of each referral fee that goes to [`AGGREGATOR_FEE_RECEIVER_ID`] instead of the
/// referrer
const AGGREGATOR_FEE_SHARE: FeeFraction = FeeFraction::new(FULL_FEE_FRACTION.get() / 5); // 20%

/// The referral fees of the request, without the ones of 0. [`AGGREGATOR_FEE_SHARE`] of each fee
/// goes to [`AGGREGATOR_FEE_RECEIVER_ID`] instead of the referrer.
pub fn payable_referral_fees(referral_fees: &QuoteFees) -> QuoteFees {
    let mut payable = Vec::new();
    let mut protocol_share = FeeFraction::ZERO;
    for (referrer_id, fee_fraction) in &referral_fees.0 {
        if *fee_fraction == FeeFraction::ZERO {
            continue;
        }
        // Rounded down
        let share = FeeFraction::new(
            (u64::from(fee_fraction.get()) * u64::from(AGGREGATOR_FEE_SHARE.get())
                / u64::from(FULL_FEE_FRACTION.get())) as u32,
        );
        protocol_share = protocol_share
            .checked_add(share)
            .expect("Referral fees add up to at most 100%");
        payable.push((
            referrer_id.clone(),
            fee_fraction
                .checked_sub(share)
                .expect("The share is a part of the fee"),
        ));
    }
    if protocol_share > FeeFraction::ZERO {
        match payable
            .iter_mut()
            .find(|(referrer_id, _)| referrer_id == AGGREGATOR_FEE_RECEIVER_ID)
        {
            Some((_, fee_fraction)) => {
                *fee_fraction = fee_fraction
                    .checked_add(protocol_share)
                    .expect("Referral fees add up to at most 100%")
            }
            None => payable.push((AGGREGATOR_FEE_RECEIVER_ID.to_owned(), protocol_share)),
        }
    }
    QuoteFees(payable)
}

/// What the DEX gets of `amount_in`, which the engine takes the referral fees out of
pub fn amount_in_after_referral_fees(referral_fees: &QuoteFees, amount_in: u128) -> u128 {
    let fees = ReferralFees::from(&payable_referral_fees(referral_fees))
        .amounts_out_of(TokenAmount::new(amount_in));
    amount_in - fees.iter().map(|(_, fee)| fee.get()).sum::<u128>()
}

pub fn amount_in_with_referral_fees(referral_fees: &QuoteFees, dex_amount_in: u128) -> u128 {
    let fees = ReferralFees::from(&payable_referral_fees(referral_fees))
        .amounts_on_top_of(TokenAmount::new(dex_amount_in));
    dex_amount_in + fees.iter().map(|(_, fee)| fee.get()).sum::<u128>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregator_gets_its_share_of_each_referral_fee() {
        let referral_fees: QuoteFees = r#"{"ref.near":0.01,"ref2.near":0.02,"zero.near":0}"#
            .parse()
            .unwrap();
        // 80% of each fee to its referrer, 20% of all of them together to the aggregator, and nothing
        // for the fee of 0
        assert_eq!(
            payable_referral_fees(&referral_fees).0,
            vec![
                ("ref.near".parse().unwrap(), FeeFraction::new(8_000)),
                ("ref2.near".parse().unwrap(), FeeFraction::new(16_000)),
                (
                    AGGREGATOR_FEE_RECEIVER_ID.to_owned(),
                    FeeFraction::new(6_000)
                ),
            ]
        );
    }
}
