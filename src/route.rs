use near_min_api::{
    types::{AccountId, Action, Balance, FunctionCallAction, NearToken},
    utils::dec_format,
};
use serde::Deserialize;

use crate::types::{Amount, DexId, ExecutionInstruction, Gas, TokenId};

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRoute {
    pub dex_id: DexId,
    pub estimated_amount: Amount,
    pub worst_case_amount: Amount,
    /// Executed one after another
    pub transactions: Vec<RouteTransaction>,
    pub token_output: TokenId,
    pub discount: Option<Amount>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteTransaction {
    pub receiver_id: AccountId,
    pub calls: Vec<RouteCall>,
    /// Pays referral fees after the swap
    pub is_fee_transaction: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteCall {
    pub method_name: String,
    pub args: Vec<u8>,
    pub deposit: NearToken,
    pub gas: Gas,
}

impl RouteCall {
    pub fn new(method_name: &str, args: serde_json::Value, deposit: NearToken, gas: Gas) -> Self {
        Self {
            method_name: method_name.to_string(),
            args: serde_json::to_vec(&args).unwrap(),
            deposit,
            gas,
        }
    }
}

impl From<RouteTransaction> for ExecutionInstruction {
    fn from(transaction: RouteTransaction) -> Self {
        ExecutionInstruction::NearTransaction {
            receiver_id: transaction.receiver_id,
            actions: transaction
                .calls
                .into_iter()
                .map(|call| {
                    Action::FunctionCall(Box::new(FunctionCallAction {
                        method_name: call.method_name,
                        args: call.args,
                        gas: call.gas.attached(),
                        deposit: call.deposit,
                    }))
                })
                .collect(),
            is_fee_transaction: transaction.is_fee_transaction,
        }
    }
}

/// Merges transactions to the same receiver that follow each other into one, except fee
/// transactions with the others, so a failed swap's `ft_transfer_call` doesn't go with the fees'
/// `ft_transfer`s
pub fn merge_transactions(transactions: Vec<RouteTransaction>) -> Vec<RouteTransaction> {
    let mut merged: Vec<RouteTransaction> = vec![];
    for transaction in transactions {
        if transaction.calls.is_empty() {
            continue;
        }
        match merged.last_mut() {
            Some(previous)
                if previous.receiver_id == transaction.receiver_id
                    && previous.is_fee_transaction == transaction.is_fee_transaction =>
            {
                previous.calls.extend(transaction.calls);
            }
            _ => merged.push(transaction),
        }
    }
    merged
}

/// What the fee transactions transfer, all in the NEP-141 of the route's input token
pub fn fee_transactions_amount(transactions: &[RouteTransaction]) -> Balance {
    #[derive(Deserialize)]
    struct FtTransferArgs {
        #[serde(with = "dec_format")]
        amount: Balance,
    }
    transactions
        .iter()
        .filter(|transaction| transaction.is_fee_transaction)
        .flat_map(|transaction| &transaction.calls)
        .filter(|call| call.method_name == "ft_transfer")
        .map(|call| {
            serde_json::from_slice::<FtTransferArgs>(&call.args)
                .expect("Fee transfers are built with an amount")
                .amount
        })
        .sum()
}

/// NEAR the transactions attach to storage deposits
pub fn storage_deposits(transactions: &[RouteTransaction]) -> NearToken {
    transactions
        .iter()
        .flat_map(|transaction| &transaction.calls)
        .filter(|call| call.method_name == "storage_deposit")
        .map(|call| call.deposit)
        .fold(NearToken::from_yoctonear(0), NearToken::saturating_add)
}
