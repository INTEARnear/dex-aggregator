use futures_util::future::join_all;
use near_min_api::types::{AccountId, Action, Balance, NearToken};

use tracing::info;

use crate::{
    calls::CallBlueprint,
    providers::rhea_dcl::RHEA_DCL_CONTRACT_ID,
    shared_utils::{
        needs_storage_deposit_for_contract, register_assets_on_intear,
        rhea_fee_receivers_registration, rhea_registration, token_storage_deposit_amount,
        NetworkView,
    },
    types::{Amount, DexId, ExecutionInstruction, Gas, IntearAssetId, TokenId},
};

/// What a provider found: the swap, and what has to happen around it. What the trader and the
/// contracts lack is found out when it's resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteBlueprint {
    pub dex_id: DexId,
    pub estimated_amount: Amount,
    pub worst_case_amount: Amount,
    pub token_output: TokenId,
    pub discount: Option<Amount>,
    /// Executed one after another
    pub steps: Vec<RouteStep>,
    /// Pay referral fees out of the wallet, after `steps` and the output conversion, so the swap
    /// doesn't wait for them. Sent only if the swap went through.
    pub fee_steps: Vec<RouteStep>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RouteStep {
    Call(CallBlueprint),
    /// Registers the trader on the NEP-141 with its minimum storage, if they have none
    TraderStorageDepositOnNep141IfNeeded {
        token_id: AccountId,
        trader_account_id: AccountId,
    },
    /// Registers someone else than the trader on the NEP-141 with its minimum storage, if they have
    /// none, like a referrer that gets a fee or a DEX that gets a deposit
    StorageDepositOnNep141IfNeeded {
        token_id: AccountId,
        account_id: AccountId,
    },
    /// Registers the trader on Rhea DCL with its minimum storage, if they have none
    TraderStorageDepositOnRheaDclIfNeeded {
        trader_account_id: AccountId,
    },
    /// Registers the tokens the trader has no balance of on Rhea, with the storage they lack
    RegisterOnRheaIfNeeded {
        trader_account_id: AccountId,
        token_ids: Vec<AccountId>,
    },
    /// Storage for the receivers that have no balance of the token on Rhea, which a transfer of
    /// their fee then adds
    RegisterFeeReceiversOnRheaIfNeeded {
        token_id: AccountId,
        receiver_ids: Vec<AccountId>,
    },
    /// Registers the assets the trader has no balance of on Intear DEX, and the asset of each fee
    /// receiver that it lacks, with the storage they lack
    RegisterOnIntearDexIfNeeded {
        trader_account_id: AccountId,
        asset_ids: Vec<IntearAssetId>,
        fee_receivers: Vec<(AccountId, IntearAssetId)>,
    },
}

/// A route with what its steps need found out on the network: calls ready to cost and sign
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRoute {
    pub dex_id: DexId,
    pub estimated_amount: Amount,
    pub worst_case_amount: Amount,
    pub token_output: TokenId,
    pub discount: Option<Amount>,
    pub calls: Vec<CallBlueprint>,
    pub fee_calls: Vec<CallBlueprint>,
}

impl ResolvedRoute {
    pub fn gas(&self) -> Gas {
        self.calls
            .iter()
            .chain(&self.fee_calls)
            .map(CallBlueprint::gas)
            .sum()
    }

    pub fn storage_deposits(&self) -> NearToken {
        self.calls
            .iter()
            .chain(&self.fee_calls)
            .map(CallBlueprint::storage_deposit)
            .fold(NearToken::from_yoctonear(0), NearToken::saturating_add)
    }

    /// What the fee calls pay out of the wallet balance of the input token
    pub fn fee_amount(&self) -> Balance {
        self.fee_calls
            .iter()
            .map(CallBlueprint::wallet_fee_amount)
            .sum()
    }

    /// The fee transactions are last
    pub fn into_execution_instructions(self) -> Vec<ExecutionInstruction> {
        [
            into_transactions(self.calls, false),
            into_transactions(self.fee_calls, true),
        ]
        .concat()
    }
}

/// None if what a step needs can't be found out
pub async fn resolve(network: &impl NetworkView, route: RouteBlueprint) -> Option<ResolvedRoute> {
    let (calls, fee_calls) = futures_util::join!(
        resolve_steps(network, route.steps),
        resolve_steps(network, route.fee_steps)
    );
    Some(ResolvedRoute {
        dex_id: route.dex_id,
        estimated_amount: route.estimated_amount,
        worst_case_amount: route.worst_case_amount,
        token_output: route.token_output,
        discount: route.discount,
        calls: calls?,
        fee_calls: fee_calls?,
    })
}

/// The calls of the steps, in their order, with the network asked about all of them at once
pub async fn resolve_steps(
    network: &impl NetworkView,
    steps: Vec<RouteStep>,
) -> Option<Vec<CallBlueprint>> {
    let calls = join_all(steps.into_iter().map(|step| resolve_step(network, step)))
        .await
        .into_iter()
        .collect::<Option<Vec<_>>>()?;
    Some(calls.concat())
}

async fn resolve_step(network: &impl NetworkView, step: RouteStep) -> Option<Vec<CallBlueprint>> {
    match step {
        RouteStep::Call(call) => Some(vec![call]),
        RouteStep::TraderStorageDepositOnNep141IfNeeded {
            token_id,
            trader_account_id,
        } => {
            if !needs_storage_deposit_for_contract(network, &trader_account_id, &token_id).await {
                return Some(vec![]);
            }
            Some(vec![CallBlueprint::Nep141StorageDeposit {
                amount: token_storage_deposit_amount(network, &token_id).await?,
                token_id,
                account_id: None,
            }])
        }
        RouteStep::StorageDepositOnNep141IfNeeded {
            token_id,
            account_id,
        } => {
            if !needs_storage_deposit_for_contract(network, &account_id, &token_id).await {
                return Some(vec![]);
            }
            Some(vec![CallBlueprint::Nep141StorageDeposit {
                amount: token_storage_deposit_amount(network, &token_id).await?,
                token_id,
                account_id: Some(account_id),
            }])
        }
        RouteStep::TraderStorageDepositOnRheaDclIfNeeded { trader_account_id } => {
            let contract_id: AccountId = RHEA_DCL_CONTRACT_ID.parse().unwrap();
            if !needs_storage_deposit_for_contract(network, &trader_account_id, &contract_id).await
            {
                return Some(vec![]);
            }
            let min = network
                .storage_balance_bounds(contract_id.clone())
                .await
                .inspect_err(|e| {
                    info!("Failed to get the storage balance bounds of {contract_id}: {e}")
                })
                .ok()?
                .min();
            Some(vec![CallBlueprint::RheaDclStorageDeposit { amount: min }])
        }
        RouteStep::RegisterOnRheaIfNeeded {
            trader_account_id,
            token_ids,
        } => rhea_registration(network, &trader_account_id, &token_ids).await,
        RouteStep::RegisterFeeReceiversOnRheaIfNeeded {
            token_id,
            receiver_ids,
        } => rhea_fee_receivers_registration(network, &token_id, &receiver_ids).await,
        RouteStep::RegisterOnIntearDexIfNeeded {
            trader_account_id,
            asset_ids,
            fee_receivers,
        } => {
            register_assets_on_intear(network, &trader_account_id, &asset_ids, &fee_receivers).await
        }
    }
}

/// Calls to the same receiver that follow each other go in one transaction
pub fn into_transactions(
    calls: Vec<CallBlueprint>,
    is_fee_transaction: bool,
) -> Vec<ExecutionInstruction> {
    let mut transactions: Vec<(AccountId, Vec<Action>)> = Vec::new();
    for call in calls {
        let receiver_id = call.receiver_id();
        let action = call.into_action();
        match transactions.last_mut() {
            Some((previous_receiver_id, actions)) if *previous_receiver_id == receiver_id => {
                actions.push(action)
            }
            _ => transactions.push((receiver_id, vec![action])),
        }
    }
    transactions
        .into_iter()
        .map(
            |(receiver_id, actions)| ExecutionInstruction::NearTransaction {
                receiver_id,
                actions,
                is_fee_transaction,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn wrap(amount: u128) -> CallBlueprint {
        CallBlueprint::WrapNear {
            amount: NearToken::from_yoctonear(amount),
        }
    }

    fn storage_deposit(token_id: &str, amount: NearToken) -> CallBlueprint {
        CallBlueprint::Nep141StorageDeposit {
            token_id: account(token_id),
            account_id: None,
            amount,
        }
    }

    fn fee_transfer(amount: u128) -> CallBlueprint {
        CallBlueprint::InputNep141FeeTransfer {
            token_id: account("wrap.near"),
            receiver_id: account("ref.near"),
            amount,
            swap_referrer_id: account("app.near"),
        }
    }

    fn resolved_route(calls: Vec<CallBlueprint>, fee_calls: Vec<CallBlueprint>) -> ResolvedRoute {
        ResolvedRoute {
            dex_id: DexId::Wrap,
            estimated_amount: Amount::AmountOut(1),
            worst_case_amount: Amount::AmountOut(1),
            token_output: TokenId::Near,
            discount: None,
            calls,
            fee_calls,
        }
    }

    /// The receivers of the transactions, the number of calls in each, and whether they pay fees
    fn shape(transactions: &[ExecutionInstruction]) -> Vec<(&str, usize, bool)> {
        transactions
            .iter()
            .map(|transaction| match transaction {
                ExecutionInstruction::NearTransaction {
                    receiver_id,
                    actions,
                    is_fee_transaction,
                } => (receiver_id.as_str(), actions.len(), *is_fee_transaction),
            })
            .collect()
    }

    #[test]
    fn calls_to_the_same_receiver_that_follow_each_other_share_a_transaction() {
        assert!(into_transactions(vec![], false).is_empty());
        assert_eq!(
            shape(&into_transactions(vec![wrap(1), wrap(2), wrap(3)], false)),
            [("wrap.near", 3, false)]
        );
        assert_eq!(
            shape(&into_transactions(
                vec![
                    wrap(1),
                    storage_deposit("ft", NearToken::from_millinear(1)),
                    wrap(2),
                ],
                false
            )),
            [
                ("wrap.near", 1, false),
                ("ft", 1, false),
                ("wrap.near", 1, false)
            ]
        );
    }

    #[test]
    fn fee_calls_are_their_own_transactions_after_the_others() {
        let route = resolved_route(vec![wrap(1)], vec![fee_transfer(2), fee_transfer(3)]);
        assert_eq!(
            shape(&route.into_execution_instructions()),
            [("wrap.near", 1, false), ("wrap.near", 2, true)]
        );
    }

    #[test]
    fn costs_add_up_every_call() {
        let route = resolved_route(
            vec![
                storage_deposit("ft", NearToken::from_micronear(1_250)),
                CallBlueprint::RheaDclStorageDeposit {
                    amount: NearToken::from_millinear(500),
                },
                wrap(1),
            ],
            vec![
                storage_deposit("wrap.near", NearToken::from_micronear(1_250)),
                fee_transfer(8),
                fee_transfer(2),
            ],
        );
        assert_eq!(route.storage_deposits(), NearToken::from_micronear(502_500));
        assert_eq!(route.fee_amount(), 10);
        // The fee calls' gas counts too
        assert_eq!(
            route.gas(),
            Gas::from_ggas(2_300, 11_000) * 2
                + Gas::from_ggas(3_000, 4_000)
                + Gas::from_ggas(1_700, 2_500)
                + Gas::from_ggas(2_000, 10_000) * 2
        );
    }
}
