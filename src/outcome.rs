use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::LazyLock,
    time::Duration,
};

use axum::http::StatusCode;
use futures_util::future::try_join_all;
use near_min_api::{
    types::{
        AccountId, ActionError, ActionErrorKind, ActionView, Balance, BlockHeight, BlockId,
        BlockReference, CryptoHash, ExecutionStatusView, FinalExecutionOutcomeWithReceiptView,
        FunctionCallError, ReceiptEnumView, ReceiptView, TxExecutionError, TxExecutionStatus,
    },
    ExperimentalTxDetails, QueryFinality, RpcClient,
};
use near_sdk::{json_types::U128, NearToken};
use serde::{de::DeserializeOwned, Serialize};
use tokio::time::Instant;

use inindexer::{transaction_trees::ExecutedReceipt, IndexerExecutionOutcomeWithReceipt};
use intear_dex_common::AssetId;
use trade_indexer::{receipt_events, ReceiptEvent};

use crate::{
    route::{into_transactions, resolve_steps},
    shared_utils::{
        base_token, convert_to, is_near, BaseTokenId, Mainnet, NetworkView, REQWEST_CLIENT,
        RPC_CLIENT, RPC_URLS, WRAP_NEAR,
    },
    types::{BalanceChange, IntearAssetId, OutcomeRequest, OutcomeWaitUntil, SwapOutcome, TokenId},
};

const EXECUTION_TIMEOUT: Duration = Duration::from_secs(60);
const EXECUTION_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Blocks are about 600ms apart, so a swap is seen at most this long after its block
const SWAP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const BALANCES_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_TRANSACTIONS: usize = 20;
const STORAGE_METHODS: &[&str] = &["storage_deposit", "storage_withdraw", "storage_unregister"];

const SLIPPAGE: &str = "The price moved more than your slippage allows while the transaction was on its way. Try again, or increase the slippage.";
const NOT_ENOUGH_LIQUIDITY: &str =
    "The pool doesn't have enough liquidity for this trade. Try a smaller amount.";
const NOT_ENOUGH_TOKENS: &str = "You don't have enough tokens for this trade.";
const NOT_REGISTERED: &str = "Your account isn't registered for one of the tokens. Try again.";
const NOT_ENOUGH_STORAGE: &str = "Your account doesn't have enough NEAR for storage. Try again.";

/// Substrings of errors that happen in normal use, and what to tell the trader about them. The
/// first error of the transactions that has one of these is described.
const KNOWN_FAILURES: &[(&str, &str)] = &[
    // Rhea
    ("E68: slippage error", SLIPPAGE),
    ("E22: not enough tokens in deposit", NOT_ENOUGH_TOKENS),
    ("E10: account not registered", NOT_REGISTERED),
    ("E11: insufficient $NEAR storage deposit", NOT_ENOUGH_STORAGE),
    // Rhea DCL
    ("E204: slippage error", SLIPPAGE),
    // Intear DEX
    ("Output amount is less than constraint", SLIPPAGE),
    ("Input amount is greater than constraint", SLIPPAGE),
    ("Amount must be less than out balance", NOT_ENOUGH_LIQUIDITY),
    ("Pool is empty", NOT_ENOUGH_LIQUIDITY),
    (
        "liquidity can't go lower than the initial phantom liquidity",
        NOT_ENOUGH_LIQUIDITY,
    ),
    ("Insufficient balance", NOT_ENOUGH_TOKENS),
    ("doesn't have a registered balance for this asset", NOT_REGISTERED),
    ("is not registered for", NOT_REGISTERED),
    ("exceeds total", NOT_ENOUGH_STORAGE),
    (
        "Fee discount expired",
        "The quote expired before the transaction executed. Request a new route and try again.",
    ),
    // Fungible tokens
    ("The account doesn't have enough balance", NOT_ENOUGH_TOKENS),
    ("is not registered", NOT_REGISTERED),
    // Runtime
    (
        "Exceeded the prepaid gas",
        "The transaction ran out of gas. Try again with a smaller amount.",
    ),
    (
        "wouldn't have enough balance to cover storage",
        "Your account doesn't have enough NEAR left for gas and storage. Deposit some NEAR and try again.",
    ),
];

static TX_STATUS_RPC_CLIENT: LazyLock<RpcClient> =
    LazyLock::new(|| RPC_CLIENT.clone().with_max_retries(0));

/// A block that a transaction or receipt executed in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockInfo {
    pub height: BlockHeight,
    pub prev_hash: CryptoHash,
}

pub trait ChainView: Send + Sync {
    /// The transaction and its receipts, `None` until they all executed, at least optimistically
    fn executed_transaction(
        &self,
        tx_hash: CryptoHash,
    ) -> impl Future<Output = Result<Option<FinalExecutionOutcomeWithReceiptView>, String>> + Send;

    /// The transaction and the receipts of it that executed so far, at least optimistically, and
    /// whether that's all of them. `None` until it's included.
    fn transaction_progress(
        &self,
        tx_hash: CryptoHash,
        signer_id: &AccountId,
    ) -> impl Future<Output = Result<Option<(FinalExecutionOutcomeWithReceiptView, bool)>, String>> + Send;

    fn block(
        &self,
        block_hash: CryptoHash,
    ) -> impl Future<Output = Result<BlockInfo, String>> + Send;

    /// The balance of a fungible token in the wallet or of an inner balance after a block. Not
    /// called for native NEAR.
    fn balance(
        &self,
        token_id: &TokenId,
        account_id: &AccountId,
        block_hash: CryptoHash,
    ) -> impl Future<Output = Result<Balance, String>> + Send;
}

impl ChainView for Mainnet {
    async fn executed_transaction(
        &self,
        tx_hash: CryptoHash,
    ) -> Result<Option<FinalExecutionOutcomeWithReceiptView>, String> {
        let details = TX_STATUS_RPC_CLIENT
            .EXPERIMENTAL_tx_status(tx_hash)
            .await
            .map_err(|err| format!("{err:?}"))?;
        // The RPC waits for optimistic execution, but answers earlier if that takes too long
        Ok(match details.final_execution_status {
            TxExecutionStatus::ExecutedOptimistic
            | TxExecutionStatus::Executed
            | TxExecutionStatus::Final => details.final_execution_outcome,
            TxExecutionStatus::None
            | TxExecutionStatus::Included
            | TxExecutionStatus::IncludedFinal => None,
        })
    }

    async fn transaction_progress(
        &self,
        tx_hash: CryptoHash,
        signer_id: &AccountId,
    ) -> Result<Option<(FinalExecutionOutcomeWithReceiptView, bool)>, String> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "dontcare",
            "method": "EXPERIMENTAL_tx_status",
            "params": {
                "tx_hash": tx_hash,
                "sender_account_id": signer_id,
                "wait_until": TxExecutionStatus::Included,
            },
        });
        let mut errors = Vec::new();
        for url in RPC_URLS.iter() {
            let response = match REQWEST_CLIENT.post(url).json(&request).send().await {
                Ok(response) => response,
                Err(err) => {
                    errors.push(format!("{url}: {err}"));
                    continue;
                }
            };
            let body = match response.json::<serde_json::Value>().await {
                Ok(body) => body,
                Err(err) => {
                    errors.push(format!("{url}: {err}"));
                    continue;
                }
            };
            if let Some(error) = body.get("error") {
                // Not seen yet, or not included before the RPC stopped waiting
                if matches!(
                    error["cause"]["name"].as_str(),
                    Some("UNKNOWN_TRANSACTION" | "TIMEOUT_ERROR")
                ) {
                    return Ok(None);
                }
                errors.push(format!("{url}: {error}"));
                continue;
            }
            let details: ExperimentalTxDetails = serde_json::from_value(body["result"].clone())
                .map_err(|err| format!("Failed to read transaction {tx_hash}: {err}"))?;
            let executed = matches!(
                details.final_execution_status,
                TxExecutionStatus::ExecutedOptimistic
                    | TxExecutionStatus::Executed
                    | TxExecutionStatus::Final
            );
            return Ok(details
                .final_execution_outcome
                .map(|outcome| (outcome, executed)));
        }
        Err(errors.join(", "))
    }

    async fn block(&self, block_hash: CryptoHash) -> Result<BlockInfo, String> {
        RPC_CLIENT
            .block(BlockReference::BlockId(BlockId::Hash(block_hash)))
            .await
            .map(|block| BlockInfo {
                height: block.header.height,
                prev_hash: block.header.prev_hash,
            })
            .map_err(|err| format!("Failed to get block {block_hash}: {err:?}"))
    }

    async fn balance(
        &self,
        token_id: &TokenId,
        account_id: &AccountId,
        block_hash: CryptoHash,
    ) -> Result<Balance, String> {
        let at_block = || QueryFinality::BlockId(BlockId::Hash(block_hash));
        match token_id {
            TokenId::Near => unreachable!("Native NEAR is added up from receipts"),
            TokenId::Nep141(contract_id) => RPC_CLIENT
                .call::<U128>(
                    contract_id.clone(),
                    "ft_balance_of",
                    serde_json::json!({ "account_id": account_id }),
                    at_block(),
                )
                .await
                .map(|balance| balance.0),
            TokenId::Nep141OnRhea(contract_id) => RPC_CLIENT
                .call::<U128>(
                    "v2.ref-finance.near".parse().unwrap(),
                    "get_deposit",
                    serde_json::json!({
                        "account_id": account_id,
                        "token_id": contract_id,
                    }),
                    at_block(),
                )
                .await
                .map(|balance| balance.0),
            TokenId::TokenOnIntearDex(asset_id) => RPC_CLIENT
                .call::<Option<U128>>(
                    "dex.intear.near".parse().unwrap(),
                    "asset_balance_of",
                    serde_json::json!({
                        "of": { "Account": account_id },
                        "asset_id": AssetId::from(asset_id),
                    }),
                    at_block(),
                )
                .await
                .map(|balance| balance.map_or(0, |balance| balance.0)),
        }
        .map_err(|err| format!("Failed to get the {token_id} balance of {account_id}: {err:?}"))
    }
}

/// What the outcome needs to know about an executed transaction
#[derive(Debug, Clone, PartialEq)]
struct ExecutedTransaction {
    hash: CryptoHash,
    signer_id: AccountId,
    /// The block the transaction was included in
    block_hash: CryptoHash,
    /// The blocks its receipts executed in
    receipt_block_hashes: Vec<CryptoHash>,
    /// NEAR the signer attached to the calls of the transaction, unless they failed and it was
    /// refunded
    native_spent: Balance,
    /// NEAR contracts transferred to the signer, other than storage deposits they sent back
    native_received: Balance,
    /// Errors of the transaction and its receipts, in the order they executed
    failures: Vec<String>,
    /// What the swaps of the signer took and gave of each token
    traded: HashMap<TokenId, i128>,
}

impl ExecutedTransaction {
    fn from_view(view: &FinalExecutionOutcomeWithReceiptView) -> Result<Self, String> {
        let outcome = &view.final_outcome;
        let transaction = &outcome.transaction;
        let statuses = outcome
            .receipts_outcome
            .iter()
            .map(|receipt| (receipt.id, &receipt.outcome.status))
            .collect::<HashMap<_, _>>();
        let parents = outcome
            .receipts_outcome
            .iter()
            .flat_map(|receipt| {
                receipt
                    .outcome
                    .receipt_ids
                    .iter()
                    .map(|child_id| (*child_id, receipt.id))
            })
            .collect::<HashMap<_, _>>();
        let mut actions = view
            .receipts
            .iter()
            .filter_map(|receipt| match &receipt.receipt {
                ReceiptEnumView::Action { actions, .. } => {
                    Some((receipt.receipt_id, actions.as_slice()))
                }
                ReceiptEnumView::Data { .. }
                | ReceiptEnumView::GlobalContractDistribution { .. } => None,
            })
            .collect::<HashMap<_, _>>();
        let succeeded = |receipt_id: &CryptoHash| match statuses.get(receipt_id) {
            Some(ExecutionStatusView::Failure(_)) => Ok(false),
            Some(
                ExecutionStatusView::SuccessValue(_) | ExecutionStatusView::SuccessReceiptId(_),
            ) => Ok(true),
            Some(ExecutionStatusView::Unknown) | None => Err(format!(
                "Receipt {receipt_id} of transaction {} hasn't executed",
                transaction.hash
            )),
        };

        // The transaction is converted to a receipt with its actions, which refunds the deposits
        // if it fails. wrap.near sends the yoctoNEAR of `near_withdraw` back with the NEAR, so it
        // counts as spent.
        let native_spent = match &outcome.transaction_outcome.outcome.status {
            ExecutionStatusView::SuccessReceiptId(receipt_id) => {
                actions.insert(*receipt_id, transaction.actions.as_slice());
                if succeeded(receipt_id)? {
                    transaction
                        .actions
                        .iter()
                        .map(|action| match action {
                            ActionView::FunctionCall {
                                method_name,
                                deposit,
                                ..
                            } if (*deposit > NearToken::from_yoctonear(1)
                                || method_name == "near_withdraw")
                                && !STORAGE_METHODS.contains(&method_name.as_str()) =>
                            {
                                deposit.as_yoctonear()
                            }
                            _ => 0,
                        })
                        .sum()
                } else {
                    0
                }
            }
            _ => 0,
        };

        let mut native_received = 0;
        for receipt in &view.receipts {
            let ReceiptEnumView::Action {
                actions: receipt_actions,
                ..
            } = &receipt.receipt
            else {
                continue;
            };
            // Gas and failed deposits are refunded by the system, and the transaction's own
            // receipt is sent by the signer
            if receipt.receiver_id != transaction.signer_id
                || receipt.predecessor_id.as_str() == "system"
                || receipt.predecessor_id == transaction.signer_id
                || !succeeded(&receipt.receipt_id)?
            {
                continue;
            }
            let parent_id = parents.get(&receipt.receipt_id).ok_or_else(|| {
                format!(
                    "Receipt {} of transaction {} has no parent",
                    receipt.receipt_id, transaction.hash
                )
            })?;
            let parent_actions = actions.get(parent_id).ok_or_else(|| {
                format!(
                    "Receipt {parent_id} of transaction {} is missing",
                    transaction.hash
                )
            })?;
            // A receipt that also does other things than storage can't be told apart from its
            // storage refunds, but routes only make storage deposits that are refunded nothing
            let refunds_storage = parent_actions.iter().all(|action| {
                matches!(
                    action,
                    ActionView::FunctionCall { method_name, .. }
                        if STORAGE_METHODS.contains(&method_name.as_str())
                )
            });
            if refunds_storage {
                continue;
            }
            native_received += receipt_actions
                .iter()
                .map(|action| match action {
                    ActionView::Transfer { deposit } => deposit.as_yoctonear(),
                    _ => 0,
                })
                .sum::<Balance>();
        }

        let failures = std::iter::once(&outcome.transaction_outcome)
            .chain(&outcome.receipts_outcome)
            .filter_map(|execution| match &execution.outcome.status {
                ExecutionStatusView::Failure(error) => Some(failure_message(error)),
                ExecutionStatusView::Unknown
                | ExecutionStatusView::SuccessValue(_)
                | ExecutionStatusView::SuccessReceiptId(_) => None,
            })
            .collect();

        Ok(Self {
            hash: transaction.hash,
            signer_id: transaction.signer_id.clone(),
            block_hash: outcome.transaction_outcome.block_hash,
            receipt_block_hashes: outcome
                .receipts_outcome
                .iter()
                .map(|receipt| receipt.block_hash)
                .collect(),
            native_spent,
            native_received,
            failures,
            traded: traded_amounts(view)?,
        })
    }
}

/// The panic message of a contract, or what the runtime says about another error
fn failure_message(error: &TxExecutionError) -> String {
    match error {
        TxExecutionError::ActionError(ActionError {
            kind: ActionErrorKind::FunctionCallError(FunctionCallError::ExecutionError(message)),
            ..
        }) => message.clone(),
        error => error.to_string(),
    }
}

/// What to tell the trader about why the swap failed
fn describe_failure(failures: &[&str]) -> String {
    let known = failures.iter().find_map(|failure| {
        KNOWN_FAILURES
            .iter()
            .find(|(pattern, _)| failure.contains(*pattern))
            .map(|(_, description)| description.to_string())
    });
    match (known, failures.first()) {
        (Some(description), _) => description,
        (None, Some(&failure)) => format!("The swap failed: {}", panic_message(failure)),
        (None, None) => "The swap didn't give any tokens".to_string(),
    }
}

/// What a contract panicked with, without what the runtime adds around it
fn panic_message(failure: &str) -> &str {
    let message = failure
        .strip_prefix("Smart contract panicked: ")
        .unwrap_or(failure);
    // Contracts built with older Rust, like Rhea, also tell where they panicked
    match message
        .strip_prefix("panicked at '")
        .and_then(|message| message.rsplit_once("', "))
    {
        Some((message, _location)) => message,
        None => message,
    }
}

/// Every place a token can be held, where its balance is compared: the wallet (native NEAR or
/// the NEP-141), Rhea's inner balance, and Intear DEX's
fn locations(token_id: &TokenId) -> Vec<TokenId> {
    if is_near(token_id) {
        let wrap_near: AccountId = WRAP_NEAR.parse().unwrap();
        return vec![
            TokenId::Near,
            TokenId::Nep141(wrap_near.clone()),
            TokenId::Nep141OnRhea(wrap_near.clone()),
            TokenId::TokenOnIntearDex(IntearAssetId::Near),
            TokenId::TokenOnIntearDex(IntearAssetId::Nep141(wrap_near)),
        ];
    }
    match base_token(token_id) {
        BaseTokenId::Nep141(contract_id) => vec![
            TokenId::Nep141(contract_id.clone()),
            TokenId::Nep141OnRhea(contract_id.clone()),
            TokenId::TokenOnIntearDex(IntearAssetId::Nep141(contract_id)),
        ],
        BaseTokenId::Near => unreachable!("NEAR is handled above"),
    }
}

/// Where the fee transactions of a route pay from: the wallet NEP-141 of the input token, wNEAR
/// for NEAR
fn fee_location(token_in: &TokenId) -> TokenId {
    if is_near(token_in) {
        return TokenId::Nep141(WRAP_NEAR.parse().unwrap());
    }
    match base_token(token_in) {
        BaseTokenId::Nep141(contract_id) => TokenId::Nep141(contract_id),
        BaseTokenId::Near => unreachable!("NEAR is handled above"),
    }
}

fn signed_difference(minuend: Balance, subtrahend: Balance) -> Result<i128, (StatusCode, String)> {
    match (i128::try_from(minuend), i128::try_from(subtrahend)) {
        (Ok(minuend), Ok(subtrahend)) => Ok(minuend - subtrahend),
        _ => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Balances {minuend} and {subtrahend} are too large to compare"),
        )),
    }
}

/// How much a balance of the trader changed from before the transactions to after them
async fn balance_change(
    chain: &impl ChainView,
    transactions: &[ExecutedTransaction],
    location: &TokenId,
    account_id: &AccountId,
    before: CryptoHash,
    after: CryptoHash,
) -> Result<i128, (StatusCode, String)> {
    match location {
        TokenId::Near => signed_difference(
            transactions.iter().map(|tx| tx.native_received).sum(),
            transactions.iter().map(|tx| tx.native_spent).sum(),
        ),
        _ => {
            let (balance_before, balance_after) = tokio::try_join!(
                chain.balance(location, account_id, before),
                chain.balance(location, account_id, after),
            )
            .map_err(|err| (StatusCode::BAD_GATEWAY, err))?;
            signed_difference(balance_after, balance_before)
        }
    }
}

async fn wait_for_execution(
    chain: &impl ChainView,
    tx_hash: CryptoHash,
    deadline: Instant,
) -> Result<ExecutedTransaction, (StatusCode, String)> {
    let mut pending_reason = "still executing".to_string();
    loop {
        // The RPC itself waits for the execution, so it's cut off at the deadline too
        match tokio::time::timeout_at(deadline, chain.executed_transaction(tx_hash)).await {
            Ok(Ok(Some(view))) => {
                return ExecutedTransaction::from_view(&view)
                    .map_err(|err| (StatusCode::BAD_GATEWAY, err))
            }
            Ok(Ok(None)) => pending_reason = "still executing".to_string(),
            Ok(Err(err)) => pending_reason = err,
            Err(_elapsed) => {}
        }
        if Instant::now() >= deadline {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "Transaction {tx_hash} didn't execute in {}s: {pending_reason}",
                    EXECUTION_TIMEOUT.as_secs()
                ),
            ));
        }
        tokio::time::sleep(EXECUTION_POLL_INTERVAL).await;
    }
}

/// The same view in the types of the indexers, which have the same JSON
fn indexer_view<View: Serialize, IndexerView: DeserializeOwned>(
    view: &View,
) -> Result<IndexerView, String> {
    serde_json::to_value(view)
        .and_then(serde_json::from_value)
        .map_err(|err| format!("Failed to convert {}: {err}", std::any::type_name::<View>()))
}

/// What the swaps of the signer took and gave of each token, in the receipts of the transaction
/// that succeeded. Native NEAR traded on Intear DEX is `TokenId::Near`.
fn traded_amounts(
    view: &FinalExecutionOutcomeWithReceiptView,
) -> Result<HashMap<TokenId, i128>, String> {
    let transaction = &view.final_outcome.transaction;
    let transaction_id = indexer_view(&transaction.hash)?;
    // A transaction to the signer's own account is converted to a local receipt, which the RPC
    // doesn't list with the others
    let transaction_outcome = &view.final_outcome.transaction_outcome.outcome;
    let transaction_receipt =
        transaction_outcome
            .receipt_ids
            .first()
            .map(|receipt_id| ReceiptView {
                predecessor_id: transaction.signer_id.clone(),
                receiver_id: transaction.receiver_id.clone(),
                receipt_id: *receipt_id,
                receipt: ReceiptEnumView::Action {
                    signer_id: transaction.signer_id.clone(),
                    signer_public_key: transaction.public_key.clone(),
                    gas_price: transaction_outcome.tokens_burnt
                        / Balance::from(transaction_outcome.gas_burnt.as_gas()),
                    output_data_receivers: Vec::new(),
                    input_data_ids: Vec::new(),
                    actions: transaction.actions.clone(),
                    is_promise_yield: false,
                    refund_to: None,
                },
                priority: 0,
            });
    let receipts = transaction_receipt
        .iter()
        .chain(&view.receipts)
        .map(|receipt| (receipt.receipt_id, receipt))
        .collect::<HashMap<_, _>>();
    let mut traded = HashMap::new();
    for outcome in &view.final_outcome.receipts_outcome {
        let receipt = receipts.get(&outcome.id).ok_or_else(|| {
            format!(
                "Receipt {} of transaction {} is missing",
                outcome.id, transaction.hash
            )
        })?;
        let receipt = ExecutedReceipt {
            receipt: IndexerExecutionOutcomeWithReceipt {
                execution_outcome: indexer_view(outcome)?,
                receipt: indexer_view(receipt)?,
                tx_hash: transaction_id,
            },
            // Only what the receipt traded is read, not when
            block_height: 0,
            block_timestamp_nanosec: 0,
        };
        for event in receipt_events(&receipt, transaction_id, None, false) {
            let ReceiptEvent::BalanceChangeSwap {
                context,
                balance_changes,
                ..
            } = event
            else {
                continue;
            };
            if context.trader != transaction.signer_id {
                continue;
            }
            for (token, change) in balance_changes.balance_changes {
                let token = if token == "near" {
                    TokenId::Near
                } else {
                    TokenId::Nep141(token)
                };
                *traded.entry(token).or_insert(0) += change;
            }
        }
    }
    Ok(traded)
}

/// What the swaps of a route did, from what the DEXes logged of them, once each transaction has
/// either executed or swapped, nothing failed, and some of the output came out. None until then.
/// Each route swaps in one call to its DEX, which logs every hop of it at once, so a transaction
/// that logged a swap has no other swap still to execute.
fn swapped(
    request: &OutcomeRequest,
    transactions: &[(FinalExecutionOutcomeWithReceiptView, bool)],
) -> Result<Option<SwapOutcome>, (StatusCode, String)> {
    let trader_account_id = &request.trader_account_id;
    let input_locations = locations(&request.token_in);
    let output_locations = locations(&request.token_out);
    let mut amount_in: Balance = 0;
    let mut amount_out: Balance = 0;
    for (view, executed) in transactions {
        let outcome = &view.final_outcome;
        if &outcome.transaction.signer_id != trader_account_id {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "Transaction {} is signed by {}, not {trader_account_id}",
                    outcome.transaction.hash, outcome.transaction.signer_id
                ),
            ));
        }
        // Why it failed is told once everything executed
        if std::iter::once(&outcome.transaction_outcome)
            .chain(&outcome.receipts_outcome)
            .any(|execution| matches!(execution.outcome.status, ExecutionStatusView::Failure(_)))
        {
            return Ok(None);
        }
        let traded = traded_amounts(view).map_err(|err| (StatusCode::BAD_GATEWAY, err))?;
        if !executed && traded.is_empty() {
            return Ok(None);
        }
        for (token, change) in traded {
            if change < 0 && input_locations.contains(&token) {
                amount_in = amount_in.saturating_add(change.unsigned_abs());
            }
            if change > 0 && output_locations.contains(&token) {
                amount_out = amount_out.saturating_add(change.unsigned_abs());
            }
        }
    }
    Ok((amount_out > 0).then_some(SwapOutcome {
        charge_fee: true,
        amount_in,
        amount_out,
        balance_changes: Vec::new(),
        next_steps: Vec::new(),
        error: None,
        settled: false,
    }))
}

/// How far waiting for a route's swaps got
enum SwapProgress {
    /// Before everything executed
    Swapped(SwapOutcome),
    /// Everything executed first, or nothing swapped
    Executed(Vec<ExecutedTransaction>),
}

/// Waits until the swaps of the transactions executed, or until everything did
async fn wait_for_swap(
    chain: &impl ChainView,
    request: &OutcomeRequest,
    tx_hashes: &[CryptoHash],
    deadline: Instant,
) -> Result<SwapProgress, (StatusCode, String)> {
    let mut pending_reason = "not included yet".to_string();
    loop {
        let progress =
            tokio::time::timeout_at(
                deadline,
                try_join_all(tx_hashes.iter().map(|tx_hash| {
                    chain.transaction_progress(*tx_hash, &request.trader_account_id)
                })),
            )
            .await;
        match progress {
            Ok(Ok(progress)) => {
                let included = progress.iter().zip(tx_hashes).try_fold(
                    Vec::new(),
                    |mut included, (progress, tx_hash)| match progress {
                        Some(progress) => {
                            included.push(progress.clone());
                            Ok(included)
                        }
                        None => Err(format!("transaction {tx_hash} isn't included yet")),
                    },
                );
                match included {
                    Ok(included) if included.iter().all(|(_, executed)| *executed) => {
                        return included
                            .iter()
                            .map(|(view, _)| ExecutedTransaction::from_view(view))
                            .collect::<Result<_, _>>()
                            .map(SwapProgress::Executed)
                            .map_err(|err| (StatusCode::BAD_GATEWAY, err));
                    }
                    Ok(included) => match swapped(request, &included)? {
                        Some(outcome) => return Ok(SwapProgress::Swapped(outcome)),
                        None => pending_reason = "still executing".to_string(),
                    },
                    Err(reason) => pending_reason = reason,
                }
            }
            Ok(Err(err)) => pending_reason = err,
            Err(_elapsed) => {}
        }
        if Instant::now() >= deadline {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "The transactions didn't execute in {}s: {pending_reason}",
                    EXECUTION_TIMEOUT.as_secs()
                ),
            ));
        }
        tokio::time::sleep(SWAP_POLL_INTERVAL).await;
    }
}

/// Waits for the transactions of a route to execute, or only their swaps with
/// `wait_until=swap`, and finds out what they did
pub async fn swap_outcome(
    network: &impl NetworkView,
    chain: &impl ChainView,
    request: OutcomeRequest,
) -> Result<SwapOutcome, (StatusCode, String)> {
    let tx_hashes = request.tx_hashes.as_deref().unwrap_or_default();
    if tx_hashes.is_empty() || tx_hashes.len() > MAX_TRANSACTIONS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("tx_hashes must have 1 to {MAX_TRANSACTIONS} transaction hashes"),
        ));
    }
    if tx_hashes.iter().collect::<HashSet<_>>().len() != tx_hashes.len() {
        return Err((
            StatusCode::BAD_REQUEST,
            "tx_hashes must not repeat a transaction".to_string(),
        ));
    }
    if request.token_in == request.token_out {
        return Err((
            StatusCode::BAD_REQUEST,
            "Token in and token out must be different".to_string(),
        ));
    }
    let fee_amount = i128::try_from(request.fee_amount).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "fee_amount is too large".to_string(),
        )
    })?;

    let deadline = Instant::now() + EXECUTION_TIMEOUT;
    let transactions = match request.wait_until {
        OutcomeWaitUntil::Executed => {
            try_join_all(
                tx_hashes
                    .iter()
                    .map(|tx_hash| wait_for_execution(chain, *tx_hash, deadline)),
            )
            .await?
        }
        OutcomeWaitUntil::Swap => {
            match wait_for_swap(chain, &request, tx_hashes, deadline).await? {
                SwapProgress::Swapped(outcome) => return Ok(outcome),
                SwapProgress::Executed(transactions) => transactions,
            }
        }
    };

    tokio::time::timeout(
        BALANCES_TIMEOUT,
        outcome_of(network, chain, &request, fee_amount, &transactions),
    )
    .await
    .map_err(|_elapsed| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Reading the balances took more than {}s",
                BALANCES_TIMEOUT.as_secs()
            ),
        )
    })?
}

/// What the executed transactions of a route did
async fn outcome_of(
    network: &impl NetworkView,
    chain: &impl ChainView,
    request: &OutcomeRequest,
    fee_amount: i128,
    transactions: &[ExecutedTransaction],
) -> Result<SwapOutcome, (StatusCode, String)> {
    let trader_account_id = &request.trader_account_id;
    if let Some(transaction) = transactions
        .iter()
        .find(|transaction| &transaction.signer_id != trader_account_id)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Transaction {} is signed by {}, not {trader_account_id}",
                transaction.hash, transaction.signer_id
            ),
        ));
    }

    // Balances are compared before the first transaction and after the last receipt
    let mut block_hashes = transactions
        .iter()
        .flat_map(|transaction| {
            std::iter::once(transaction.block_hash)
                .chain(transaction.receipt_block_hashes.iter().copied())
        })
        .collect::<Vec<_>>();
    block_hashes.sort();
    block_hashes.dedup();
    let blocks = try_join_all(block_hashes.iter().map(|block_hash| async move {
        chain
            .block(*block_hash)
            .await
            .map(|block| (*block_hash, block))
    }))
    .await
    .map_err(|err| (StatusCode::BAD_GATEWAY, err))?
    .into_iter()
    .collect::<HashMap<_, _>>();
    let before = transactions
        .iter()
        .map(|transaction| blocks[&transaction.block_hash])
        .min_by_key(|block| block.height)
        .expect("Checked that there are transactions")
        .prev_hash;
    let after = *blocks
        .iter()
        .max_by_key(|(_, block)| block.height)
        .expect("Checked that there are transactions")
        .0;

    let input_locations = locations(&request.token_in);
    let output_locations = locations(&request.token_out);
    // A conversion between two places of the same token, like a Wrap route
    let same_token = input_locations == output_locations;
    let mut all_locations = input_locations.clone();
    all_locations.extend(
        output_locations
            .iter()
            .filter(|location| !input_locations.contains(location))
            .cloned(),
    );
    let changes = try_join_all(all_locations.iter().map(|location| {
        balance_change(
            chain,
            transactions,
            location,
            trader_account_id,
            before,
            after,
        )
    }))
    .await?;
    let changes = all_locations.into_iter().zip(changes).collect::<Vec<_>>();
    let change_of = |token_id: &TokenId| {
        changes
            .iter()
            .find(|(location, _)| location == token_id)
            .map_or(0, |(_, change)| *change)
    };

    let (input_change, output_change) = if same_token {
        (change_of(&request.token_in), change_of(&request.token_out))
    } else {
        (
            input_locations.iter().map(change_of).sum::<i128>(),
            output_locations.iter().map(change_of).sum::<i128>(),
        )
    };
    let amount_in = (-input_change).max(0).unsigned_abs();
    let amount_out = output_change.max(0).unsigned_abs();
    let output_swapped = same_token
        || transactions
            .iter()
            .flat_map(|transaction| &transaction.traded)
            .any(|(token, change)| output_locations.contains(token) && *change > 0);
    let charge_fee = output_swapped && amount_out > 0;

    // Whatever of a token ended up elsewhere is moved to where the trader asked for it, but for
    // what the fee transactions pay out of the wallet after a swap that went through
    let fee_location = charge_fee.then(|| fee_location(&request.token_in));
    let targets = if same_token {
        vec![(&output_locations, &request.token_out)]
    } else {
        vec![
            (&input_locations, &request.token_in),
            (&output_locations, &request.token_out),
        ]
    };
    let mut next_steps = Vec::new();
    for (token_locations, target) in targets {
        for location in token_locations {
            let mut change = change_of(location);
            if fee_location.as_ref() == Some(location) {
                change = change.saturating_sub(fee_amount);
            }
            if location == target || change <= 0 {
                continue;
            }
            let conversion = resolve_steps(
                network,
                convert_to(
                    location,
                    target,
                    change.unsigned_abs(),
                    Some(trader_account_id),
                ),
            )
            .await
            .ok_or_else(|| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to build moving {change} of {location} to {target}"),
                )
            })?;
            next_steps.extend(conversion);
        }
    }

    let error = (!charge_fee).then(|| {
        describe_failure(
            &transactions
                .iter()
                .flat_map(|transaction| transaction.failures.iter().map(String::as_str))
                .collect::<Vec<_>>(),
        )
    });

    Ok(SwapOutcome {
        charge_fee,
        amount_in,
        amount_out,
        balance_changes: changes
            .into_iter()
            .filter(|(_, change)| *change != 0)
            .map(|(token, change)| BalanceChange { token, change })
            .collect(),
        next_steps: into_transactions(next_steps, false),
        error,
        settled: true,
    })
}
