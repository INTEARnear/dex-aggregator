use std::{collections::BTreeSet, sync::LazyLock};

use borsh::BorshSerialize;
use near_min_api::{
    types::{
        near_crypto::{KeyType, PublicKey},
        Account, AccountContract, AccountId, Action, CryptoHash, Finality, HandlerError, NearGas,
        NearToken, RpcError, RpcErrorKind, RpcQueryError, StateRecord, Transaction, TransactionV0,
    },
    Error, QueryFinality, SimulationStep,
};
use serde_json::Value;

use crate::{
    calls::CallBlueprint,
    route::{ResolvedRoute, RouteStep},
    shared_utils::{deposit_storage_if_needed, register_input_in_wallet, RPC_CLIENT, WRAP_NEAR},
    types::{IntearAssetId, TokenId},
};

/// Who routes without a trader are simulated as
pub static STAND_IN_TRADER: LazyLock<AccountId> =
    LazyLock::new(|| "simulated-trader.intear.near".parse().unwrap());

/// INTEARnear/global-ft-contract, the standard NEP-141 token of near-contract-standards
const STANDARD_TOKEN_CODE_HASH: &str = "8D1NEU2NC2hKhdtCkHyyAz2KVmVXRazm9ZQMC27D97jF";
/// Under which the standard token keeps the balance of each account, at its borsh
const BALANCES_PREFIX: u8 = 0;
const METADATA_KEY: u8 = 1;
/// The storage of a balance that the standard token measures when it's created
const ACCOUNT_STORAGE_USAGE: u64 = 125;
/// The supply of the replaced input, all of it the trader's, which leaves room for what the
/// accounts that it's sent to get
const TRADER_TOKEN_BALANCE: u128 = u128::MAX / 2;
const SIMULATED_NEAR: NearToken = NearToken::from_near(1_000_000_000);
/// Deposited to wrap.near by the trader when the route swaps wNEAR
const WRAPPED_NEAR: NearToken = NearToken::from_near(100_000_000);
/// Attached to each call that gives the trader what they swap, which isn't measured
const SETUP_CALL_GAS: NearGas = NearGas::from_tgas(100);
/// Of an account without access keys or data, `num_bytes_account` of the protocol
const NEW_ACCOUNT_STORAGE_USAGE: u64 = 100;

/// The state of the standard token, of its `FungibleToken` and the key of its metadata
#[derive(BorshSerialize)]
struct StandardTokenState {
    balances_prefix: Vec<u8>,
    total_supply: u128,
    account_storage_usage: u64,
    metadata_key: Vec<u8>,
}

/// A transaction of the trader with no key, whose access key and nonce the simulation doesn't check
pub fn keyless_transaction(
    trader: &AccountId,
    receiver_id: AccountId,
    actions: Vec<Action>,
    nonce: u64,
    block_hash: CryptoHash,
) -> SimulationStep {
    SimulationStep::Transaction(Transaction::V0(TransactionV0 {
        signer_id: trader.clone(),
        public_key: PublicKey::empty(KeyType::ED25519),
        nonce,
        receiver_id,
        block_hash,
        actions,
    }))
}

/// The steps before the trader's transactions of `calls`, which make `registrations` and give
/// them what they swap
pub async fn holdings_steps(
    trader: &AccountId,
    token_in: &TokenId,
    calls: &[&CallBlueprint],
    registrations: Vec<CallBlueprint>,
    block_hash: CryptoHash,
) -> Result<Vec<SimulationStep>, String> {
    let mut trader_account = trader_account(trader).await?;
    trader_account.set_amount(trader_account.amount().saturating_add(SIMULATED_NEAR));
    let mut records = vec![StateRecord::Account {
        account_id: trader.clone(),
        account: trader_account,
    }];
    let mut setup_calls = registrations;
    match token_in {
        TokenId::Nep141(token_id) if token_id.as_str() == WRAP_NEAR => {
            setup_calls.extend([
                CallBlueprint::Nep141StorageDeposit {
                    token_id: token_id.clone(),
                    account_id: None,
                    amount: NearToken::from_millinear(100),
                },
                CallBlueprint::WrapNear {
                    amount: WRAPPED_NEAR,
                },
            ]);
        }
        TokenId::Nep141(token_id) => records.extend(standard_token_records(
            token_id,
            account(token_id).await?,
            trader,
            &receivers_of(calls, token_id),
        )?),
        TokenId::Near | TokenId::Nep141OnRhea(_) | TokenId::TokenOnIntearDex(_) => {}
    }
    let setup_transactions = setup_calls.into_iter().map(|call| {
        keyless_transaction(
            trader,
            call.receiver_id(),
            vec![call.into_action(SETUP_CALL_GAS)],
            0,
            block_hash,
        )
    });
    Ok([SimulationStep::PatchState { records }]
        .into_iter()
        .chain(setup_transactions)
        .collect())
}

/// The trader's account, or a new one when it doesn't exist
async fn trader_account(trader: &AccountId) -> Result<Account, String> {
    match RPC_CLIENT
        .view_account(trader.clone(), QueryFinality::Finality(Finality::Final))
        .await
    {
        Ok(view) => Ok(Account::from(view)),
        Err(Error::JsonRpc(RpcError {
            error_struct:
                Some(RpcErrorKind::HandlerError(HandlerError::RpcQueryError(
                    RpcQueryError::UnknownAccount { .. },
                ))),
            ..
        })) => Ok(Account::new(
            NearToken::from_yoctonear(0),
            NearToken::from_yoctonear(0),
            AccountContract::None,
            NEW_ACCOUNT_STORAGE_USAGE,
        )),
        Err(err) => Err(format!("No account {trader}: {err:?}")),
    }
}

/// What a route of a new trader registers, which a route without a trader leaves out, for the
/// stand-in: where the output goes, the NEP-141 that the input becomes, and on each DEX of the
/// route, every asset that it swaps through, which may be more than the DEX needs, as what the
/// registrations burn isn't measured
pub fn stand_in_registrations(route: &ResolvedRoute, token_in: &TokenId) -> Vec<RouteStep> {
    let stand_in = &*STAND_IN_TRADER;
    let mut rhea_tokens = Vec::new();
    let mut on_rhea_dcl = false;
    let mut intear_dex_assets = Vec::new();
    for call in &route.calls {
        match call {
            CallBlueprint::RheaSwap { actions, .. }
            | CallBlueprint::InputNep141TransferCallRheaSwap { actions, .. } => {
                for action in actions {
                    for token_id in [&action.token_in, &action.token_out] {
                        if !rhea_tokens.contains(token_id) {
                            rhea_tokens.push(token_id.clone());
                        }
                    }
                }
            }
            CallBlueprint::InputNep141TransferCallRheaDclSwap { .. } => on_rhea_dcl = true,
            CallBlueprint::InputNep141TransferCallIntearDex { operations, .. }
            | CallBlueprint::IntearDexExecuteOperations { operations }
            | CallBlueprint::IntearDexDepositNear {
                operations: Some(operations),
                ..
            } => {
                for step in operations.swap.steps() {
                    for asset_id in [&step.asset_in, &step.asset_out] {
                        if !intear_dex_assets.contains(asset_id) {
                            intear_dex_assets.push(asset_id.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let input_nep141 = match token_in {
        TokenId::Near | TokenId::TokenOnIntearDex(IntearAssetId::Near) => {
            WRAP_NEAR.parse().unwrap()
        }
        TokenId::Nep141(token_id)
        | TokenId::Nep141OnRhea(token_id)
        | TokenId::TokenOnIntearDex(IntearAssetId::Nep141(token_id)) => token_id.clone(),
    };
    let mut steps = [
        deposit_storage_if_needed(&route.token_output, Some(stand_in)),
        register_input_in_wallet(token_in, input_nep141, Some(stand_in)),
    ]
    .concat();
    if !rhea_tokens.is_empty() {
        steps.push(RouteStep::RegisterOnRheaIfNeeded {
            trader_account_id: stand_in.clone(),
            token_ids: rhea_tokens,
        });
    }
    if on_rhea_dcl {
        steps.push(RouteStep::TraderStorageDepositOnRheaDclIfNeeded {
            trader_account_id: stand_in.clone(),
        });
    }
    if !intear_dex_assets.is_empty() {
        steps.push(RouteStep::RegisterOnIntearDexIfNeeded {
            trader_account_id: stand_in.clone(),
            asset_ids: intear_dex_assets,
            receivers: vec![],
        });
    }
    steps
}

async fn account(account_id: &AccountId) -> Result<Account, String> {
    RPC_CLIENT
        .view_account(account_id.clone(), QueryFinality::Finality(Finality::Final))
        .await
        .map(Account::from)
        .map_err(|err| format!("No account {account_id}: {err:?}"))
}

/// The accounts that the calls of the token send it to
fn receivers_of(calls: &[&CallBlueprint], token_id: &AccountId) -> BTreeSet<AccountId> {
    calls
        .iter()
        .filter(|call| &call.receiver_id() == token_id)
        .filter_map(
            |call| match (*call).clone().into_action(NearGas::from_gas(0)) {
                Action::FunctionCall(function_call) => {
                    let args: Value = serde_json::from_slice(&function_call.args)
                        .expect("calls of NEP-141 tokens have JSON args");
                    args["receiver_id"]
                        .as_str()
                        .map(|receiver_id| receiver_id.parse().expect("receivers are accounts"))
                }
                action => unreachable!("Routes make function calls, not {action:?}"),
            },
        )
        .collect()
}

/// The token account with the standard token's contract, which the trader holds all of and the
/// receivers are registered in
fn standard_token_records(
    token_id: &AccountId,
    mut token_account: Account,
    trader: &AccountId,
    receivers: &BTreeSet<AccountId>,
) -> Result<Vec<StateRecord>, String> {
    let code_hash: CryptoHash = STANDARD_TOKEN_CODE_HASH.parse().unwrap();
    token_account
        .set_contract(AccountContract::Global(code_hash))
        .map_err(|err| format!("No contract can be set on {token_id}: {err}"))?;
    let state = StandardTokenState {
        balances_prefix: vec![BALANCES_PREFIX],
        total_supply: TRADER_TOKEN_BALANCE,
        account_storage_usage: ACCOUNT_STORAGE_USAGE,
        metadata_key: vec![METADATA_KEY],
    };
    let balance = |account_id: &AccountId, balance: u128| StateRecord::Data {
        account_id: token_id.clone(),
        data_key: [vec![BALANCES_PREFIX], borsh::to_vec(account_id).unwrap()]
            .concat()
            .into(),
        value: borsh::to_vec(&balance).unwrap().into(),
    };
    let registered = receivers
        .iter()
        .filter(|receiver_id| *receiver_id != trader)
        .map(|receiver_id| balance(receiver_id, 0));
    Ok([
        StateRecord::Account {
            account_id: token_id.clone(),
            account: token_account,
        },
        StateRecord::Data {
            account_id: token_id.clone(),
            data_key: b"STATE".to_vec().into(),
            value: borsh::to_vec(&state).unwrap().into(),
        },
        balance(trader, TRADER_TOKEN_BALANCE),
    ]
    .into_iter()
    .chain(registered)
    .collect())
}

#[cfg(test)]
mod tests {
    use near_min_api::types::StoreKey;

    use super::*;
    use crate::{
        calls::{RheaOutput, RheaSwapAction},
        types::{Amount, DexId, Gas, PoolFee},
    };

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    #[test]
    fn standard_token_state_is_what_its_contract_writes() {
        // Of `new` with a supply of 1000, read back from near-sdk's mocked blockchain
        let written = [
            1, 0, 0, 0, 0, 232, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 125, 0, 0, 0, 0, 0, 0,
            0, 1, 0, 0, 0, 1,
        ];
        let state = StandardTokenState {
            balances_prefix: vec![BALANCES_PREFIX],
            total_supply: 1000,
            account_storage_usage: ACCOUNT_STORAGE_USAGE,
            metadata_key: vec![METADATA_KEY],
        };
        assert_eq!(borsh::to_vec(&state).unwrap(), written);
    }

    #[test]
    fn the_stand_in_registers_where_the_route_of_a_new_trader_would() {
        let swap = CallBlueprint::InputNep141TransferCallRheaSwap {
            token_id: account("wrap.near"),
            amount: 5,
            actions: vec![RheaSwapAction {
                pool_id: 3,
                token_in: account("wrap.near"),
                token_out: account("usdc.near"),
                amount_in: Some(5),
                amount_out: 7,
                min_amount_out: 6,
            }],
            swap_gas: Gas::from_ggas(0, 0),
            output: RheaOutput::Nep141,
            referral_id: account("app.near"),
        };
        let route = ResolvedRoute {
            dex_id: DexId::Rhea,
            estimated_amount: Amount::AmountOut(7),
            worst_case_amount: Amount::AmountOut(6),
            token_output: TokenId::Nep141(account("usdc.near")),
            discount: None,
            pool_fee: PoolFee::none(),
            calls: vec![
                CallBlueprint::WrapNear {
                    amount: NearToken::from_yoctonear(5),
                },
                swap,
            ],
            fee_calls: vec![],
        };
        let stand_in = STAND_IN_TRADER.clone();
        assert_eq!(
            stand_in_registrations(&route, &TokenId::Near),
            [
                RouteStep::TraderStorageDepositOnNep141IfNeeded {
                    token_id: account("usdc.near"),
                    trader_account_id: stand_in.clone(),
                },
                RouteStep::TraderStorageDepositOnNep141IfNeeded {
                    token_id: account("wrap.near"),
                    trader_account_id: stand_in.clone(),
                },
                RouteStep::RegisterOnRheaIfNeeded {
                    trader_account_id: stand_in,
                    token_ids: vec![account("wrap.near"), account("usdc.near")],
                },
            ]
        );
    }

    #[test]
    fn the_trader_holds_the_standard_token_and_its_receivers_are_registered() {
        let token = account("token.near");
        let trader = account("trader.near");
        let deposit = CallBlueprint::Nep141DepositOnRhea {
            token_id: token.clone(),
            amount: 5,
        };
        let swap = CallBlueprint::InputNep141TransferCallRheaSwap {
            token_id: token.clone(),
            amount: 5,
            actions: vec![],
            swap_gas: Gas::from_ggas(0, 0),
            output: RheaOutput::Nep141,
            referral_id: account("app.near"),
        };
        let storage_deposit = CallBlueprint::Nep141StorageDeposit {
            token_id: token.clone(),
            account_id: None,
            amount: NearToken::from_yoctonear(1),
        };
        let unwrap = CallBlueprint::UnwrapNear {
            amount: NearToken::from_yoctonear(1),
            trader_account_id: None,
        };
        let receivers = receivers_of(&[&storage_deposit, &deposit, &swap, &unwrap], &token);
        assert_eq!(receivers, BTreeSet::from([account("v2.ref-finance.near")]));

        let token_account = Account::new(
            NearToken::from_near(3),
            NearToken::from_yoctonear(0),
            AccountContract::Local(CryptoHash::default()),
            1000,
        );
        let records = standard_token_records(&token, token_account, &trader, &receivers).unwrap();
        let StateRecord::Account { account, .. } = &records[0] else {
            panic!("the token's account comes first");
        };
        assert_eq!(
            account.contract().into_owned(),
            AccountContract::Global(STANDARD_TOKEN_CODE_HASH.parse().unwrap())
        );
        assert_eq!(account.amount(), NearToken::from_near(3));
        let balances = records[2..]
            .iter()
            .map(|record| match record {
                StateRecord::Data {
                    data_key, value, ..
                } => (data_key.clone(), Vec::from(value.clone())),
                record => panic!("balances are data, not {record:?}"),
            })
            .collect::<Vec<_>>();
        let balance_key = |id: &str| {
            StoreKey::from([vec![0, id.len() as u8, 0, 0, 0], id.as_bytes().to_vec()].concat())
        };
        assert_eq!(
            balances,
            [
                (
                    balance_key("trader.near"),
                    (u128::MAX / 2).to_le_bytes().to_vec()
                ),
                (balance_key("v2.ref-finance.near"), vec![0; 16]),
            ]
        );
    }
}
