#![deny(clippy::arithmetic_side_effects)]

use std::{collections::HashMap, num::NonZeroU128};

use crypto_bigint::U256;
use intear_dex_types::{
    AssetId, AssetWithdrawRequest, AssetWithdrawalType, Dex, DexCallResponse, SwapRequest,
    SwapRequestAmount, SwapResponse, expect,
};
use near_sdk::{
    AccountId, BorshStorageKey, NearToken, PanicOnDefault, assert_one_yocto,
    json_types::U128,
    near,
    store::{LookupMap, Vector},
};

#[global_allocator]
static ALLOCATOR: talc::Talck<talc::locking::AssumeUnlockable, talc::ClaimOnOom> = {
    const MEMORY_SIZE: usize = 0x8000; // 32KB
    static mut MEMORY: [u8; MEMORY_SIZE] = [0; MEMORY_SIZE];
    let span = talc::Span::from_array(core::ptr::addr_of!(MEMORY).cast_mut());
    talc::Talc::new(unsafe { talc::ClaimOnOom::new(span) }).lock()
};

#[near(serializers=[borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Pools,
    PublicPoolUserShares { pool_id: PoolId },
    FeesCollectedByUsers,
}

type PoolId = u32;

/// A x*y=k pool with two assets
#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct XykDex {
    pools: Vector<Pool>,
    fees_collected_by_users: LookupMap<(AccountId, AssetId), U128>,
}

#[near(event_json(standard = "xyk"))]
enum XykDexEvent {
    #[event_version("1.0.0")]
    PoolUpdated { pool_id: PoolId, pool: PoolView },
    #[event_version("1.0.0")]
    Swap {
        pool_id: PoolId,
        request: SwapRequest,
        amount_in: U128,
        fees_breakdown: Vec<(FeeReceiver, U128)>,
        amount_out: U128,
    },
}

fn u128_to_u256(value: u128) -> U256 {
    U256::from(value)
}

fn u256_to_u128(value: U256) -> u128 {
    expect!(value.bits() <= 128, "Value must be less than 128 bits");
    let bytes = value.to_le_bytes();
    let first_chunk = bytes.first_chunk().unwrap();
    u128::from_le_bytes(*first_chunk)
}

fn shares_to_tokens(
    shares: SharesBalance,
    total_shares: SharesBalance,
    total_tokens: NonZeroU128,
) -> u128 {
    // total_shares is NonZeroU128; multiplication of u128's can not overflow u256
    #[allow(clippy::arithmetic_side_effects)]
    u256_to_u128(
        u128_to_u256(total_tokens.get()) * u128_to_u256(shares.get())
            / u128_to_u256(total_shares.get()),
    )
}

fn tokens_to_shares(tokens: u128, total_shares: SharesBalance, total_tokens: NonZeroU128) -> u128 {
    // total_shares is NonZeroU128; multiplication of u128's can not overflow u256
    #[allow(clippy::arithmetic_side_effects)]
    u256_to_u128(
        u128_to_u256(tokens) * u128_to_u256(total_shares.get()) / u128_to_u256(total_tokens.get()),
    )
}

#[near]
impl Dex for XykDex {
    #[result_serializer(borsh)]
    fn swap(&mut self, #[serializer(borsh)] request: SwapRequest) -> SwapResponse {
        #[near(serializers=[borsh])]
        struct SwapArgs {
            pool_id: PoolId,
        }
        let Ok(SwapArgs { pool_id }) = near_sdk::borsh::from_slice(&request.message.0) else {
            panic!("Invalid message");
        };
        let Some(pool) = self.pools.get_mut(pool_id) else {
            panic!("Pool not found");
        };
        let (assets, fees) = match pool {
            Pool::Private {
                assets,
                owner_id: _,
                fees,
            } => (assets, fees),
            Pool::Public {
                assets,
                fees,
                user_shares: _,
                total_shares: _,
            } => (assets, fees),
        };
        expect!(
            assets.0.asset_id == request.asset_in || assets.1.asset_id == request.asset_in,
            "Invalid asset in"
        );
        expect!(
            assets.0.asset_id == request.asset_out || assets.1.asset_id == request.asset_out,
            "Invalid asset out"
        );
        expect!(
            assets.0.balance.0 > 0 && assets.1.balance.0 > 0,
            "Pool is empty"
        );
        let first_in = match (
            assets.0.asset_id == request.asset_in && assets.1.asset_id == request.asset_out,
            assets.1.asset_id == request.asset_in && assets.0.asset_id == request.asset_out,
        ) {
            (true, false) => true,
            (false, true) => false,
            _ => panic!("Invalid assets or pool ID"),
        };

        fn collect_fees(
            amount_in: u128,
            asset_in: &AssetId,
            fees: &FeeConfiguration,
            fees_collected_by_users: &mut LookupMap<(AccountId, AssetId), U128>,
        ) -> (u128, Vec<(FeeReceiver, U128)>) {
            let mut fees_breakdown = Vec::new();
            let mut total_fees = 0u128;
            for (receiver, fee_fraction) in fees.receivers.iter() {
                // MAX_FEE_FRACTION is constant, so no zero division; u128 * u128 can't
                // overflow u256
                #[allow(clippy::arithmetic_side_effects)]
                let fee_amount = u256_to_u128(
                    u128_to_u256(amount_in) * u128_to_u256(*fee_fraction as u128)
                        / u128_to_u256(MAX_FEE_FRACTION as u128),
                );
                total_fees = total_fees.checked_add(fee_amount).expect("Overflow");
                fees_breakdown.push((receiver.clone(), U128(fee_amount)));
                match receiver {
                    FeeReceiver::Account(account_id) => {
                        fees_collected_by_users
                            .entry((account_id.clone(), asset_in.clone()))
                            .and_modify(|balance| {
                                balance.0 = balance.0.checked_add(fee_amount).expect("Overflow")
                            })
                            .or_insert(U128(fee_amount));
                    }
                }
            }
            (
                amount_in.checked_sub(total_fees).expect("Fee exceeds 100%"),
                fees_breakdown,
            )
        }

        let (fees_breakdown, response) = match request.amount {
            SwapRequestAmount::ExactIn(exact_amount_in) => {
                expect!(exact_amount_in.0 > 0, "Amount must be greater than 0");
                let (in_balance, out_balance) = if first_in {
                    (&mut assets.0.balance.0, &mut assets.1.balance.0)
                } else {
                    (&mut assets.1.balance.0, &mut assets.0.balance.0)
                };
                let (amount_in_after_fees, fees_breakdown) = collect_fees(
                    exact_amount_in.0,
                    &request.asset_in,
                    fees,
                    &mut self.fees_collected_by_users,
                );
                // u128 * u128 or u128 + u128 can't overflow u256; in_balance was checked to be positive
                #[allow(clippy::arithmetic_side_effects)]
                let amount_out = u256_to_u128(
                    u128_to_u256(amount_in_after_fees) * u128_to_u256(*out_balance)
                        / (u128_to_u256(*in_balance) + u128_to_u256(amount_in_after_fees)),
                );
                *in_balance = in_balance
                    .checked_add(amount_in_after_fees)
                    .expect("Overflow");
                *out_balance = out_balance.checked_sub(amount_out).expect("Underflow");
                (
                    fees_breakdown,
                    SwapResponse {
                        amount_in: exact_amount_in,
                        amount_out: U128(amount_out),
                    },
                )
            }
            SwapRequestAmount::ExactOut(exact_amount_out) => {
                expect!(exact_amount_out.0 > 0, "Amount must be greater than 0");
                let (in_balance, out_balance) = if first_in {
                    (&mut assets.0.balance.0, &mut assets.1.balance.0)
                } else {
                    (&mut assets.1.balance.0, &mut assets.0.balance.0)
                };
                expect!(
                    exact_amount_out.0 < *out_balance,
                    "Amount must be less than out balance"
                );
                // u128 * u128 can't overflow u256; out_balance was checked to be
                // greater than exact_amount_out so no underflow or zero division
                // can happen
                #[allow(clippy::arithmetic_side_effects)]
                let amount_in_without_fees = u256_to_u128(
                    ((u128_to_u256(*in_balance) * u128_to_u256(exact_amount_out.0))
                        / (u128_to_u256(*out_balance) - u128_to_u256(exact_amount_out.0)))
                    .saturating_add(&U256::ONE),
                );
                let total_fee_fraction = fees
                    .receivers
                    .iter()
                    .map(|(_, fee)| *fee as u128)
                    .sum::<u128>();
                let fee_denominator = (MAX_FEE_FRACTION as u128)
                    .checked_sub(total_fee_fraction)
                    .expect("Fee fraction somehow above 100%");
                let fee_denominator_minus_one = fee_denominator
                    .checked_sub(1)
                    .expect("Fee fraction somehow equals 100%");
                // checked_sub would fail if denominator was 0; MAX_FEE_FRACTION is
                // constant, so multiplication of u128's will not be close to overflowing
                // u256 after adding another u1282
                #[allow(clippy::arithmetic_side_effects)]
                let amount_in = u256_to_u128(
                    (u128_to_u256(amount_in_without_fees) * u128_to_u256(MAX_FEE_FRACTION as u128)
                        + u128_to_u256(fee_denominator_minus_one))
                        / u128_to_u256(fee_denominator),
                );
                let (amount_in_after_fees, fees_breakdown) = collect_fees(
                    amount_in,
                    &request.asset_in,
                    fees,
                    &mut self.fees_collected_by_users,
                );
                *in_balance = in_balance
                    .checked_add(amount_in_after_fees)
                    .expect("Overflow");
                *out_balance = out_balance
                    .checked_sub(exact_amount_out.0)
                    .expect("Underflow");
                (
                    fees_breakdown,
                    SwapResponse {
                        amount_in: U128(amount_in),
                        amount_out: U128(exact_amount_out.0),
                    },
                )
            }
        };
        self.fees_collected_by_users.flush();
        XykDexEvent::PoolUpdated {
            pool_id,
            pool: (&*pool).into(),
        }
        .emit();
        XykDexEvent::Swap {
            pool_id,
            request,
            amount_in: response.amount_in,
            amount_out: response.amount_out,
            fees_breakdown,
        }
        .emit();
        response
    }
}

#[near]
impl XykDex {
    #[init]
    #[payable]
    pub fn new() -> Self {
        assert_one_yocto();
        Self {
            pools: Vector::new(StorageKey::Pools),
            fees_collected_by_users: LookupMap::new(StorageKey::FeesCollectedByUsers),
        }
    }

    #[payable]
    #[result_serializer(borsh)]
    pub fn create_pool(
        &mut self,
        #[serializer(borsh)]
        #[allow(unused_mut)]
        mut attached_assets: HashMap<AssetId, U128>,
        #[serializer(borsh)] args: Vec<u8>,
    ) -> DexCallResponse {
        assert_one_yocto();
        #[near(serializers=[borsh])]
        struct CreatePoolArgs {
            assets: (AssetId, AssetId),
            fees: FeeConfiguration,
            is_public: bool,
        }
        let Ok(CreatePoolArgs {
            assets,
            fees,
            is_public,
        }) = near_sdk::borsh::from_slice(&args)
        else {
            near_sdk::env::panic_str("Invalid args");
        };
        expect!(assets.0 != assets.1, "Assets must be different");

        fees.validate();

        let storage_usage_before = near_sdk::env::storage_usage();
        for (fee_receiver, _) in fees.receivers.iter() {
            match fee_receiver {
                FeeReceiver::Account(account_id) => {
                    self.fees_collected_by_users
                        .entry((account_id.clone(), assets.0.clone()))
                        .or_default();
                    self.fees_collected_by_users
                        .entry((account_id.clone(), assets.1.clone()))
                        .or_default();
                }
            }
        }
        self.fees_collected_by_users.flush();

        let pool_id = self.pools.len();
        self.pools.push(if is_public {
            Pool::Public {
                assets: (
                    AssetWithBalance {
                        asset_id: assets.0.clone(),
                        balance: U128(0),
                    },
                    AssetWithBalance {
                        asset_id: assets.1.clone(),
                        balance: U128(0),
                    },
                ),
                fees: fees.clone(),
                user_shares: LookupMap::new(StorageKey::PublicPoolUserShares { pool_id }),
                total_shares: None,
            }
        } else {
            Pool::Private {
                assets: (
                    AssetWithBalance {
                        asset_id: assets.0.clone(),
                        balance: U128(0),
                    },
                    AssetWithBalance {
                        asset_id: assets.1.clone(),
                        balance: U128(0),
                    },
                ),
                fees: fees.clone(),
                owner_id: near_sdk::env::predecessor_account_id(),
            }
        });
        self.pools.flush();

        let storage_usage_after = near_sdk::env::storage_usage();
        let storage_cost = near_sdk::env::storage_byte_cost().saturating_mul(
            (storage_usage_after as u128)
                .checked_sub(storage_usage_before as u128)
                .expect("Can't possibly be lower after inserting"),
        );

        let attached_near = NearToken::from_yoctonear(
            attached_assets
                .remove(&AssetId::Near)
                .expect("Near should be attached for storage")
                .0,
        );
        expect!(
            attached_near >= storage_cost,
            "Not enough near attached for storage. Required: {storage_cost}, attached: {attached_near}"
        );
        expect!(
            attached_assets.is_empty(),
            "No assets other than NEAR should be attached"
        );

        XykDexEvent::PoolUpdated {
            pool_id,
            pool: self.pools.get(pool_id).unwrap().into(),
        }
        .emit();

        #[near(serializers=[borsh])]
        struct CreatePoolResponse {
            pool_id: PoolId,
        }
        let response = CreatePoolResponse { pool_id };
        DexCallResponse {
            asset_withdraw_requests: if let Some(leftover) = attached_near.checked_sub(storage_cost)
            {
                vec![AssetWithdrawRequest {
                    asset_id: AssetId::Near,
                    amount: U128(leftover.as_yoctonear()),
                    withdrawal_type: AssetWithdrawalType::ToInternalUserBalance(
                        near_sdk::env::predecessor_account_id(),
                    ),
                }]
            } else {
                vec![]
            },
            add_storage_deposit: storage_cost,
            response: near_sdk::borsh::to_vec(&response).expect("Failed to serialize response"),
        }
    }

    #[payable]
    #[result_serializer(borsh)]
    pub fn register_liquidity(
        &mut self,
        #[serializer(borsh)]
        #[allow(unused_mut)]
        mut attached_assets: HashMap<AssetId, U128>,
        #[serializer(borsh)] args: Vec<u8>,
    ) -> DexCallResponse {
        assert_one_yocto();
        #[near(serializers=[borsh])]
        struct RegisterLiquidityArgs {
            pool_id: PoolId,
        }
        let Ok(RegisterLiquidityArgs { pool_id }) = near_sdk::borsh::from_slice(&args) else {
            near_sdk::env::panic_str("Invalid args");
        };
        let Some(pool) = self.pools.get_mut(pool_id) else {
            panic!("Pool not found");
        };
        let Pool::Public { user_shares, .. } = pool else {
            panic!("Liquidity registration is only needed for public pools");
        };
        let attached_near =
            NearToken::from_yoctonear(attached_assets.remove(&AssetId::Near).unwrap_or_default().0);
        expect!(
            attached_assets.is_empty(),
            "No assets other than NEAR should be attached"
        );

        let storage_usage_before = near_sdk::env::storage_usage();
        let predecessor_id = near_sdk::env::predecessor_account_id();
        user_shares.entry(predecessor_id).or_insert(None);
        user_shares.flush();
        let storage_usage_after = near_sdk::env::storage_usage();
        let storage_cost = near_sdk::env::storage_byte_cost().saturating_mul(
            (storage_usage_after as u128).saturating_sub(storage_usage_before as u128),
        );

        expect!(
            attached_near >= storage_cost,
            "Not enough near attached for storage. Required: {storage_cost}, attached: {attached_near}"
        );

        DexCallResponse {
            asset_withdraw_requests: if let Some(leftover) = attached_near.checked_sub(storage_cost)
            {
                vec![AssetWithdrawRequest {
                    asset_id: AssetId::Near,
                    amount: U128(leftover.as_yoctonear()),
                    withdrawal_type: AssetWithdrawalType::ToInternalUserBalance(
                        near_sdk::env::predecessor_account_id(),
                    ),
                }]
            } else {
                vec![]
            },
            add_storage_deposit: storage_cost,
            ..Default::default()
        }
    }

    #[payable]
    #[result_serializer(borsh)]
    pub fn add_liquidity(
        &mut self,
        #[serializer(borsh)]
        #[allow(unused_mut)]
        mut attached_assets: HashMap<AssetId, U128>,
        #[serializer(borsh)] args: Vec<u8>,
    ) -> DexCallResponse {
        assert_one_yocto();
        #[near(serializers=[borsh])]
        struct AddLiquidityArgs {
            pool_id: PoolId,
            min_shares_received: Option<SharesBalance>,
        }
        let Ok(AddLiquidityArgs {
            pool_id,
            min_shares_received,
        }) = near_sdk::borsh::from_slice(&args)
        else {
            near_sdk::env::panic_str("Invalid args");
        };
        let Some(pool) = self.pools.get_mut(pool_id) else {
            panic!("Pool not found");
        };

        let asset_withdraw_requests = match pool {
            Pool::Private {
                assets,
                owner_id,
                fees: _,
            } => {
                expect!(
                    *owner_id == near_sdk::env::predecessor_account_id(),
                    "Only pool owner can add liquidity"
                );
                expect!(
                    min_shares_received.is_none(),
                    "Min shares received must not be specified for private pools"
                );
                let asset_0_amount = attached_assets
                    .remove(&assets.0.asset_id)
                    .expect("Asset 1 not found");
                let asset_1_amount = attached_assets
                    .remove(&assets.1.asset_id)
                    .expect("Asset 2 not found");
                expect!(
                    attached_assets.is_empty(),
                    "No assets other than the two pool assets should be attached"
                );
                assets.0.balance.0 = assets
                    .0
                    .balance
                    .0
                    .checked_add(asset_0_amount.0)
                    .expect("Overflow");
                assets.1.balance.0 = assets
                    .1
                    .balance
                    .0
                    .checked_add(asset_1_amount.0)
                    .expect("Overflow");

                Vec::new()
            }
            Pool::Public {
                assets,
                fees: _,
                user_shares,
                total_shares,
            } => {
                let asset_0_amount = attached_assets
                    .remove(&assets.0.asset_id)
                    .expect("Asset 1 not found");
                let asset_1_amount = attached_assets
                    .remove(&assets.1.asset_id)
                    .expect("Asset 2 not found");
                expect!(
                    attached_assets.is_empty(),
                    "No assets other than the two pool assets should be attached"
                );
                expect!(
                    asset_0_amount.0 > 0 && asset_1_amount.0 > 0,
                    "Amounts must be greater than 0"
                );
                expect!(
                    user_shares.contains_key(&near_sdk::env::predecessor_account_id()),
                    "User has not registered using register_liquidity"
                );
                let mut asset_withdraw_requests = Vec::new();
                match total_shares {
                    None => {
                        expect!(
                            min_shares_received.is_none(),
                            "Min shares received must not be specified for new pools"
                        );
                        assets.0.balance.0 = assets
                            .0
                            .balance
                            .0
                            .checked_add(asset_0_amount.0)
                            .expect("Overflow");
                        assets.1.balance.0 = assets
                            .1
                            .balance
                            .0
                            .checked_add(asset_1_amount.0)
                            .expect("Overflow");
                        *total_shares = Some(INITIAL_SHARES);
                        expect!(
                            user_shares
                                .insert(
                                    near_sdk::env::predecessor_account_id(),
                                    Some(INITIAL_SHARES)
                                )
                                .is_some_and(|s| s.is_none()),
                            "User already has shares but there are no total shares"
                        );
                    }
                    Some(total_shares) => {
                        expect!(
                            let Some(pool_balance_0) = NonZeroU128::new(assets.0.balance.0),
                            let Some(pool_balance_1) = NonZeroU128::new(assets.1.balance.0),
                            "Pool is empty"
                        );

                        let shares_from_asset_0 = NonZeroU128::new(tokens_to_shares(
                            asset_0_amount.0.checked_sub(1).expect("Underflow"),
                            *total_shares,
                            pool_balance_0,
                        ))
                        .expect("Can't mint zero shares");
                        let shares_from_asset_1 = NonZeroU128::new(tokens_to_shares(
                            asset_1_amount.0.checked_sub(1).expect("Underflow"),
                            *total_shares,
                            pool_balance_1,
                        ))
                        .expect("Can't mint zero shares");
                        let shares_to_mint = shares_from_asset_0.min(shares_from_asset_1);
                        if let Some(min_shares_received) = min_shares_received {
                            expect!(shares_to_mint >= min_shares_received, "Slippage error");
                        }

                        let used_asset_0 =
                            shares_to_tokens(shares_to_mint, *total_shares, pool_balance_0)
                                .checked_add(1)
                                .expect("Overflow");
                        let used_asset_1 =
                            shares_to_tokens(shares_to_mint, *total_shares, pool_balance_1)
                                .checked_add(1)
                                .expect("Overflow");

                        assets.0.balance.0 = assets
                            .0
                            .balance
                            .0
                            .checked_add(used_asset_0)
                            .expect("Overflow");
                        assets.1.balance.0 = assets
                            .1
                            .balance
                            .0
                            .checked_add(used_asset_1)
                            .expect("Overflow");

                        *total_shares = total_shares
                            .checked_add(shares_to_mint.get())
                            .expect("Overflow");
                        user_shares
                            .entry(near_sdk::env::predecessor_account_id())
                            .and_modify(|shares| match shares {
                                Some(shares) => {
                                    *shares =
                                        shares.checked_add(shares_to_mint.get()).expect("Overflow");
                                }
                                None => {
                                    *shares = Some(shares_to_mint);
                                }
                            })
                            .or_insert(Some(shares_to_mint));

                        if let Some(leftover) = asset_0_amount.0.checked_sub(used_asset_0) {
                            asset_withdraw_requests.push(AssetWithdrawRequest {
                                asset_id: assets.0.asset_id.clone(),
                                amount: U128(leftover),
                                withdrawal_type: AssetWithdrawalType::ToInternalUserBalance(
                                    near_sdk::env::predecessor_account_id(),
                                ),
                            });
                        }
                        if let Some(leftover) = asset_1_amount.0.checked_sub(used_asset_1) {
                            asset_withdraw_requests.push(AssetWithdrawRequest {
                                asset_id: assets.1.asset_id.clone(),
                                amount: U128(leftover),
                                withdrawal_type: AssetWithdrawalType::ToInternalUserBalance(
                                    near_sdk::env::predecessor_account_id(),
                                ),
                            });
                        }
                    }
                }
                asset_withdraw_requests
            }
        };

        XykDexEvent::PoolUpdated {
            pool_id,
            pool: (&*pool).into(),
        }
        .emit();

        #[near(serializers=[borsh])]
        struct AddLiquidityResponse;
        let response = AddLiquidityResponse;
        DexCallResponse {
            asset_withdraw_requests,
            response: near_sdk::borsh::to_vec(&response).expect("Failed to serialize response"),
            ..Default::default()
        }
    }

    #[payable]
    #[result_serializer(borsh)]
    pub fn remove_liquidity(
        &mut self,
        #[serializer(borsh)] attached_assets: HashMap<AssetId, U128>,
        #[serializer(borsh)] args: Vec<u8>,
    ) -> DexCallResponse {
        assert_one_yocto();
        #[near(serializers=[borsh])]
        struct RemoveLiquidityArgs {
            pool_id: PoolId,
            shares_to_remove: Option<SharesBalance>,
            min_assets_received: Option<(U128, U128)>,
        }
        let Ok(RemoveLiquidityArgs {
            pool_id,
            shares_to_remove,
            min_assets_received,
        }) = near_sdk::borsh::from_slice(&args)
        else {
            near_sdk::env::panic_str("Invalid args");
        };
        expect!(attached_assets.is_empty(), "No assets should be attached");
        let Some(pool) = self.pools.get_mut(pool_id) else {
            panic!("Pool not found");
        };

        let (withdraw_to, (asset_id_0, amount_0), (asset_id_1, amount_1)) = match pool {
            Pool::Private {
                assets,
                owner_id,
                fees: _,
            } => {
                expect!(
                    *owner_id == near_sdk::env::predecessor_account_id(),
                    "Only pool owner can remove liquidity"
                );
                expect!(
                    shares_to_remove.is_none(),
                    "Shares must be None for private pools"
                );
                expect!(
                    min_assets_received.is_none(),
                    "Min assets received must not be specified for private pools"
                );
                let amount_0 = assets.0.balance.0;
                let amount_1 = assets.1.balance.0;
                assets.0.balance.0 = 0;
                assets.1.balance.0 = 0;
                (
                    owner_id.clone(),
                    (assets.0.asset_id.clone(), amount_0),
                    (assets.1.asset_id.clone(), amount_1),
                )
            }
            Pool::Public {
                assets,
                fees: _,
                user_shares,
                total_shares,
            } => {
                let current_shares = user_shares
                    .get(&near_sdk::env::predecessor_account_id())
                    .copied()
                    .flatten()
                    .expect("User has no shares");
                let shares_to_remove = shares_to_remove.unwrap_or(current_shares);
                expect!(
                    shares_to_remove <= current_shares,
                    "Not enough shares to remove"
                );
                expect!(let Some(pool_total_shares) = total_shares, "Pool has no shares");
                let (amount_0, amount_1) = if shares_to_remove == *pool_total_shares {
                    let amount_0 = assets.0.balance.0;
                    let amount_1 = assets.1.balance.0;
                    assets.0.balance.0 = 0;
                    assets.1.balance.0 = 0;
                    (amount_0, amount_1)
                } else {
                    let amount_0 = NonZeroU128::new(assets.0.balance.0)
                        .map(|balance| {
                            shares_to_tokens(shares_to_remove, *pool_total_shares, balance)
                        })
                        .unwrap_or_default();
                    let amount_1 = NonZeroU128::new(assets.1.balance.0)
                        .map(|balance| {
                            shares_to_tokens(shares_to_remove, *pool_total_shares, balance)
                        })
                        .unwrap_or_default();
                    assets.0.balance.0 = assets
                        .0
                        .balance
                        .0
                        .checked_sub(amount_0)
                        .expect("Somehow not enough balance for asset 1 withdrawal");
                    assets.1.balance.0 = assets
                        .1
                        .balance
                        .0
                        .checked_sub(amount_1)
                        .expect("Somehow not enough balance for asset 2 withdrawal");
                    (amount_0, amount_1)
                };
                if let Some((min_asset_0_received, min_asset_1_received)) = min_assets_received {
                    expect!(
                        amount_0 >= min_asset_0_received.0 && amount_1 >= min_asset_1_received.0,
                        "Slippage error"
                    );
                }
                *total_shares = NonZeroU128::new(
                    pool_total_shares
                        .get()
                        .checked_sub(shares_to_remove.get())
                        .expect("Underflow"),
                );
                let updated_shares = NonZeroU128::new(
                    current_shares
                        .get()
                        .checked_sub(shares_to_remove.get())
                        .expect("Underflow"),
                );
                user_shares.insert(near_sdk::env::predecessor_account_id(), updated_shares);
                (
                    near_sdk::env::predecessor_account_id(),
                    (assets.0.asset_id.clone(), amount_0),
                    (assets.1.asset_id.clone(), amount_1),
                )
            }
        };

        XykDexEvent::PoolUpdated {
            pool_id,
            pool: (&*pool).into(),
        }
        .emit();

        #[near(serializers=[borsh])]
        struct RemoveLiquidityResponse;
        let response = RemoveLiquidityResponse;
        DexCallResponse {
            asset_withdraw_requests: vec![
                AssetWithdrawRequest {
                    asset_id: asset_id_0,
                    amount: U128(amount_0),
                    withdrawal_type: AssetWithdrawalType::ToInternalUserBalance(
                        withdraw_to.clone(),
                    ),
                },
                AssetWithdrawRequest {
                    asset_id: asset_id_1,
                    amount: U128(amount_1),
                    withdrawal_type: AssetWithdrawalType::ToInternalUserBalance(withdraw_to),
                },
            ],
            response: near_sdk::borsh::to_vec(&response).expect("Failed to serialize response"),
            ..Default::default()
        }
    }

    #[payable]
    #[result_serializer(borsh)]
    pub fn edit_fees(
        &mut self,
        #[serializer(borsh)]
        #[allow(unused_mut)]
        mut attached_assets: HashMap<AssetId, U128>,
        #[serializer(borsh)] args: Vec<u8>,
    ) -> DexCallResponse {
        assert_one_yocto();
        #[near(serializers=[borsh])]
        struct EditFeesArgs {
            pool_id: PoolId,
            fees: FeeConfiguration,
        }
        let Ok(EditFeesArgs { pool_id, fees }) = near_sdk::borsh::from_slice(&args) else {
            near_sdk::env::panic_str("Invalid args");
        };
        let Some(pool) = self.pools.get_mut(pool_id) else {
            panic!("Pool not found");
        };
        let (assets, pool_fees) = match pool {
            Pool::Private {
                assets,
                owner_id,
                fees,
            } => {
                expect!(
                    *owner_id == near_sdk::env::predecessor_account_id(),
                    "Only pool owner can edit fees"
                );
                (assets, fees)
            }
            Pool::Public { .. } => {
                panic!("Fees cannot be edited for public pools");
            }
        };
        fees.validate();

        let storage_usage_before = near_sdk::env::storage_usage();
        for (fee_receiver, _) in fees.receivers.iter() {
            match fee_receiver {
                FeeReceiver::Account(account_id) => {
                    self.fees_collected_by_users
                        .entry((account_id.clone(), assets.0.asset_id.clone()))
                        .or_default();
                    self.fees_collected_by_users
                        .entry((account_id.clone(), assets.1.asset_id.clone()))
                        .or_default();
                }
            }
        }
        self.fees_collected_by_users.flush();

        *pool_fees = fees.clone();
        XykDexEvent::PoolUpdated {
            pool_id,
            pool: (&*pool).into(),
        }
        .emit();
        self.pools.flush();

        let storage_usage_after = near_sdk::env::storage_usage();
        let storage_cost = near_sdk::env::storage_byte_cost().saturating_mul(
            (storage_usage_after as u128).saturating_sub(storage_usage_before as u128),
        );

        let attached_near =
            NearToken::from_yoctonear(attached_assets.remove(&AssetId::Near).unwrap_or_default().0);
        expect!(
            attached_near >= storage_cost,
            "Not enough near attached for storage. Required: {storage_cost}, attached: {attached_near}"
        );
        expect!(
            attached_assets.is_empty(),
            "No assets other than NEAR should be attached"
        );

        DexCallResponse {
            asset_withdraw_requests: if let Some(leftover) = attached_near.checked_sub(storage_cost)
            {
                vec![AssetWithdrawRequest {
                    asset_id: AssetId::Near,
                    amount: U128(leftover.as_yoctonear()),
                    withdrawal_type: AssetWithdrawalType::ToInternalUserBalance(
                        near_sdk::env::predecessor_account_id(),
                    ),
                }]
            } else {
                vec![]
            },
            add_storage_deposit: storage_cost,
            ..Default::default()
        }
    }

    #[payable]
    #[result_serializer(borsh)]
    pub fn withdraw_fees(
        &mut self,
        #[serializer(borsh)] attached_assets: HashMap<AssetId, U128>,
        #[serializer(borsh)] args: Vec<u8>,
    ) -> DexCallResponse {
        assert_one_yocto();
        #[near(serializers=[borsh])]
        struct WithdrawFeesArgs {
            assets: Vec<AssetId>,
        }
        let Ok(WithdrawFeesArgs { assets }) = near_sdk::borsh::from_slice(&args) else {
            near_sdk::env::panic_str("Invalid args");
        };
        expect!(attached_assets.is_empty(), "No assets should be attached");

        let mut asset_withdraw_requests = Vec::new();
        for asset_id in assets {
            let Some(balance) = self
                .fees_collected_by_users
                .get(&(near_sdk::env::predecessor_account_id(), asset_id.clone()))
                .cloned()
            else {
                continue;
            };
            if balance.0 == 0 {
                continue;
            }
            self.fees_collected_by_users.insert(
                (near_sdk::env::predecessor_account_id(), asset_id.clone()),
                U128(0),
            );
            asset_withdraw_requests.push(AssetWithdrawRequest {
                asset_id,
                amount: balance,
                withdrawal_type: AssetWithdrawalType::ToInternalUserBalance(
                    near_sdk::env::predecessor_account_id(),
                ),
            });
        }

        DexCallResponse {
            asset_withdraw_requests,
            ..Default::default()
        }
    }

    #[result_serializer(borsh)]
    pub fn get_pool(&self, #[serializer(borsh)] pool_id: PoolId) -> Option<PoolView> {
        self.pools.get(pool_id).map(|pool| pool.into())
    }

    #[result_serializer(borsh)]
    pub fn get_pools(
        &self,
        #[serializer(borsh)] start_index: PoolId,
        #[serializer(borsh)] limit: PoolId,
    ) -> Vec<PoolView> {
        self.pools
            .iter()
            .skip(start_index as usize)
            .take(limit as usize)
            .map(|pool| pool.into())
            .collect()
    }

    #[result_serializer(borsh)]
    pub fn get_pool_shares(
        &self,
        #[serializer(borsh)] pool_id: PoolId,
        #[serializer(borsh)] account_id: AccountId,
    ) -> Option<U128> {
        let pool = self.pools.get(pool_id)?;
        match pool {
            Pool::Public { user_shares, .. } => user_shares
                .get(&account_id)
                .map(|shares| shares.map(|shares| U128(shares.get())).unwrap_or_default()),
            Pool::Private { .. } => None,
        }
    }

    #[result_serializer(borsh)]
    pub fn get_pending_fees(
        &self,
        #[serializer(borsh)] account_id: AccountId,
        #[serializer(borsh)] asset_ids: Vec<AssetId>,
    ) -> Vec<(AssetId, U128)> {
        asset_ids
            .into_iter()
            .filter_map(|asset_id| {
                self.fees_collected_by_users
                    .get(&(account_id.clone(), asset_id.clone()))
                    .cloned()
                    .map(|balance| (asset_id, balance))
            })
            .collect()
    }

    #[result_serializer(borsh)]
    pub fn get_pool_count(&self) -> PoolId {
        self.pools.len()
    }
}

#[near(serializers=[borsh])]
pub enum Pool {
    Private {
        assets: (AssetWithBalance, AssetWithBalance),
        owner_id: AccountId,
        fees: FeeConfiguration,
    },
    Public {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: FeeConfiguration,
        user_shares: LookupMap<AccountId, Option<SharesBalance>>,
        total_shares: Option<SharesBalance>,
    },
}

#[near(serializers=[borsh, json])]
pub enum PoolView {
    Private {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: FeeConfiguration,
        owner_id: AccountId,
    },
    Public {
        assets: (AssetWithBalance, AssetWithBalance),
        fees: FeeConfiguration,
        total_shares: Option<U128>,
    },
}

impl From<&Pool> for PoolView {
    fn from(pool: &Pool) -> Self {
        match pool {
            Pool::Private {
                assets,
                owner_id,
                fees,
            } => PoolView::Private {
                assets: assets.clone(),
                fees: fees.clone(),
                owner_id: owner_id.clone(),
            },
            Pool::Public {
                assets,
                fees,
                total_shares,
                user_shares: _,
            } => PoolView::Public {
                assets: assets.clone(),
                fees: fees.clone(),
                total_shares: total_shares.map(|s| U128(s.get())),
            },
        }
    }
}

/// When a public pool is created, the creator gets
/// 1e18 shares, which represents 100% of the pool.
/// When someone adds liquidity, the rate of tokens
/// per share is calculated by dividing the total
/// pool value by the total shares, and shares are
/// minted or burnt accordingly.
type SharesBalance = NonZeroU128;

/// Should add up to less than 1000000 (1% = 10000)
#[near(serializers=[borsh, json])]
#[derive(Clone)]
pub struct FeeConfiguration {
    receivers: Vec<(FeeReceiver, FeeFraction)>,
}

/// 100% = 1000000
type FeeFraction = u32;

const MAX_FEE_FRACTION: FeeFraction = 1000000;
const INITIAL_SHARES: SharesBalance = NonZeroU128::new(10u128.pow(18)).unwrap();

impl FeeConfiguration {
    fn validate(&self) {
        expect!(self.receivers.len() <= 42, "Too many fee receivers");
        expect!(
            self.receivers
                .iter()
                .all(|(_, fee)| *fee < MAX_FEE_FRACTION),
            "Fee must be less than 100% per receiver"
        );
        expect!(
            self.receivers.iter().map(|(_, fee)| *fee).sum::<u32>() < MAX_FEE_FRACTION,
            "Fees must add up to less than 100%"
        );
    }
}

#[near(serializers=[borsh, json])]
#[derive(PartialEq, Clone)]
pub enum FeeReceiver {
    Account(AccountId),
    // Pool, // unimplemented
}

#[near(serializers=[borsh, json])]
#[derive(Clone)]
pub struct AssetWithBalance {
    asset_id: AssetId,
    balance: U128,
}
