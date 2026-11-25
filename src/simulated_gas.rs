use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use amp_dex_types::{ExchangePrices, PricesRequest, PricesResponse};
use base64::{prelude::BASE64_STANDARD, Engine};
use futures_util::{
    future::{BoxFuture, Shared},
    FutureExt,
};
use intear_dex_common::AssetId;
use near_min_api::{
    types::{
        near_account_id::AccountType,
        near_crypto::{PublicKey, PublicKeyHandle},
        AccountId, Action, ActionView, BlockHeight, BlockId, BlockReference, CryptoHash,
        ExecutionOutcomeWithIdView, ExecutionStatusView, FinalExecutionOutcomeWithReceiptView,
        Finality, FunctionCallAction, NearGas, NearToken, NonceMode, Transaction, TransactionNonce,
        TransactionV1,
    },
    RpcClient, SimulatedTransaction, SimulationStep,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::{
    calls::CallBlueprint,
    hooks::inteardex::INTEAR_DEX_CONTRACT_ID,
    route::{in_same_transaction, resolve_steps, ResolvedRoute, RouteGas, RouteStep},
    shared_utils::{final_block_hash, Mainnet, RPC_URLS},
    simulated_holdings::{
        holdings_steps, keyless_transaction, stand_in_registrations, STAND_IN_TRADER,
    },
    types::TokenId,
};

const REFRESH_AFTER: Duration = Duration::from_secs(5 * 60);
/// Of each request that a simulation makes, which the quote waits for
const SIMULATION_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Attached to each call of a simulated transaction, more than they burn, unless the transaction
/// can't attach that much to all of its calls
const SIMULATED_CALL_GAS: NearGas = NearGas::from_tgas(300);
/// The most a transaction can attach to its calls in all, `max_total_prepaid_gas` of the protocol
const MAX_TRANSACTION_GAS: NearGas = NearGas::from_tgas(1_000);
const ORACLE_RESPONSE_GAS: NearGas = NearGas::from_tgas(100);
/// Measured swaps of a DEX that what a hop of its route search burns is fitted to, fewer of which
/// would be weighed by the tokens they transfer more than by their hops
const MIN_FITTED_SWAPS: usize = 20;

/// Of an RPC that simulates transactions: the first of the RPCs that does, unless it's set
static SIMULATION_RPC_URL: LazyLock<String> = LazyLock::new(|| {
    std::env::var("SIMULATION_RPC_URL").unwrap_or_else(|_| {
        RPC_URLS
            .iter()
            .find(|url| RpcClient::new([url.as_str()]).supports_intear_methods())
            .expect("No RPC of RPC_URLS simulates transactions, set SIMULATION_RPC_URL to one")
            .clone()
    })
});
static SIMULATION_RPC: LazyLock<RpcClient> = LazyLock::new(|| {
    RpcClient::new([SIMULATION_RPC_URL.as_str()])
        .with_max_retries(0)
        .with_request_timeout(SIMULATION_REQUEST_TIMEOUT)
});
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(SIMULATION_REQUEST_TIMEOUT)
        .build()
        .unwrap()
});
/// What transactions burnt, by their shape
static MEASURED: LazyLock<Mutex<HashMap<String, Measured>>> = LazyLock::new(Mutex::default);
/// Simulations that are running, by their trader and the shape of their route
static SIMULATING: LazyLock<Mutex<HashMap<String, Simulation>>> = LazyLock::new(Mutex::default);

type Simulation = Shared<BoxFuture<'static, Result<(), String>>>;

struct Measured {
    burnt: NearGas,
    at: Instant,
    /// Of a swap in the transaction on a DEX whose route search is weighed by what its hops burn
    hops: Option<(SearchedDex, u64)>,
}

#[derive(Clone, Copy, PartialEq)]
enum SearchedDex {
    Rhea,
    RheaDcl,
}

/// What the transactions of the route burn, measured by simulations, and what each of its calls
/// needs attached. None when the simulation of a transaction of a shape that wasn't measured
/// fails.
pub async fn route_gas(
    route: &ResolvedRoute,
    trader: Option<&AccountId>,
    token_in: &TokenId,
) -> Option<RouteGas> {
    let shapes = shapes_of(route, trader);
    let route_shape = shapes.join("\n");
    match measured_gas(route, &shapes) {
        Some((gas, false)) => Some(gas),
        Some((gas, true)) => {
            // Refreshed for the quotes after this one
            drop(simulation(route, trader, token_in, &route_shape));
            Some(gas)
        }
        None => match simulation(route, trader, token_in, &route_shape).await {
            Ok(()) => match measured_gas(route, &shapes) {
                Some((gas, _)) => Some(gas),
                None => {
                    warn!(
                        "The simulation of a {} route didn't measure it, leaving it out",
                        route.dex_id
                    );
                    None
                }
            },
            Err(err) => {
                warn!(
                    "Simulating a {} route failed, leaving it out: {}",
                    route.dex_id,
                    without_urls(&err)
                );
                None
            }
        },
    }
}

/// The error with its URLs left out, as the URL of an RPC can have its key
fn without_urls(error: &str) -> String {
    let ends_url = |c: char| c.is_whitespace() || matches!(c, '"' | '(' | ')');
    error
        .split_inclusive(ends_url)
        .map(|part| {
            if !part.starts_with("http://") && !part.starts_with("https://") {
                return part.to_string();
            }
            match part.chars().last().filter(|&last| ends_url(last)) {
                Some(end) => format!("<url>{end}"),
                None => "<url>".to_string(),
            }
        })
        .collect()
}

/// A transaction of the simulation of a route, of calls that it measures or of calls of fixed gas
struct RouteTransaction {
    calls: Vec<CallBlueprint>,
    measured: bool,
}

/// The transactions of the route as its quote sends them, before its fee transactions, with the
/// calls of fixed gas in transactions of their own
fn transactions_of(route: &ResolvedRoute) -> Vec<RouteTransaction> {
    let of_fixed_gas = |call: &CallBlueprint| call.fixed_gas().is_some();
    route
        .calls
        .chunk_by(|call, next| {
            in_same_transaction(call, next) && of_fixed_gas(call) == of_fixed_gas(next)
        })
        .map(|calls| RouteTransaction {
            calls: calls.to_vec(),
            measured: !of_fixed_gas(&calls[0]),
        })
        .collect()
}

/// Of each measured transaction of the route
fn shapes_of(route: &ResolvedRoute, trader: Option<&AccountId>) -> Vec<String> {
    transactions_of(route)
        .iter()
        .filter(|transaction| transaction.measured)
        .map(|transaction| shape_of_transaction(&transaction.calls, trader))
        .collect()
}

/// Of each call of a transaction of `calls`, which shares what it can attach with the others
fn most_attached_per_call(calls: usize) -> NearGas {
    NearGas::from_gas(MAX_TRANSACTION_GAS.as_gas() / calls as u64)
}

/// The gas of the route's transactions, and whether any was measured more than [`REFRESH_AFTER`]
/// ago. None when the shape of a measured transaction wasn't measured. What one call of a
/// transaction burns isn't told apart from the others, so each call attaches what its whole
/// transaction burnt.
fn measured_gas(route: &ResolvedRoute, shapes: &[String]) -> Option<(RouteGas, bool)> {
    let measured = MEASURED.lock().unwrap();
    let mut stale = false;
    let mut estimated_burn = NearGas::from_gas(0);
    let mut calls = Vec::with_capacity(route.calls.len());
    let mut shapes = shapes.iter();
    for transaction in transactions_of(route) {
        if !transaction.measured {
            for gas in transaction
                .calls
                .iter()
                .filter_map(CallBlueprint::fixed_gas)
            {
                estimated_burn = estimated_burn.saturating_add(gas.estimated_burn);
                calls.push(gas.needs_attached);
            }
            continue;
        }
        let measured = measured.get(shapes.next().expect("measured transactions have shapes"))?;
        stale |= measured.at.elapsed() > REFRESH_AFTER;
        estimated_burn = estimated_burn.saturating_add(measured.burnt);
        calls.extend(transaction.calls.iter().map(|call| {
            measured
                .burnt
                .saturating_add(call.margin())
                .min(most_attached_per_call(transaction.calls.len()))
        }));
    }
    let fee_calls = route
        .fee_calls
        .iter()
        .map(|call| {
            let gas = call
                .fixed_gas()
                .expect("fee calls are storage deposits and transfers");
            estimated_burn = estimated_burn.saturating_add(gas.estimated_burn);
            gas.needs_attached
        })
        .collect();
    let gas = RouteGas {
        calls,
        fee_calls,
        estimated_burn,
    };
    Some((gas, stale))
}

/// The trader's simulation of the route that's running, or a new one, which stores what the calls
/// burnt when it succeeds
fn simulation(
    route: &ResolvedRoute,
    trader: Option<&AccountId>,
    token_in: &TokenId,
    route_shape: &str,
) -> Simulation {
    let simulated_as = trader.unwrap_or(&STAND_IN_TRADER);
    let key = format!("{simulated_as} {route_shape}");
    let mut simulating = SIMULATING.lock().unwrap();
    if let Some(simulation) = simulating.get(&key) {
        return simulation.clone();
    }
    let registrations = match trader {
        Some(_) => Vec::new(),
        // A route without a trader leaves out the registrations, which its stand-in makes
        None => stand_in_registrations(route, token_in),
    };
    let (transactions, trader, token_in, simulated_key) = (
        transactions_of(route),
        simulated_as.clone(),
        token_in.clone(),
        key.clone(),
    );
    let simulated = tokio::spawn(async move {
        let result = simulate(&transactions, registrations, &trader, &token_in).await;
        // The next quote of the route simulates it again, also when it failed
        SIMULATING.lock().unwrap().remove(&simulated_key);
        result
    });
    let simulation = simulated
        .map(|joined| joined.unwrap_or_else(|err| Err(format!("The simulation panicked: {err}"))))
        .boxed()
        .shared();
    simulating.insert(key, simulation.clone());
    simulation
}

async fn simulate(
    transactions: &[RouteTransaction],
    registrations: Vec<RouteStep>,
    trader: &AccountId,
    token_in: &TokenId,
) -> Result<(), String> {
    let burnt = measure(transactions, registrations, trader, token_in).await?;
    let mut measured = MEASURED.lock().unwrap();
    let measured_transactions = transactions
        .iter()
        .filter(|transaction| transaction.measured);
    for (transaction, burnt) in measured_transactions.zip(burnt) {
        measured.insert(
            shape_of_transaction(&transaction.calls, Some(trader)),
            Measured {
                burnt,
                at: Instant::now(),
                hops: transaction.calls.iter().find_map(hops_of),
            },
        );
    }
    fit_search_hop_gas(&measured);
    Ok(())
}

/// What each measured transaction burns as `trader`, after `registrations`, holding what the route
/// swaps, and with the oracle responding to the request that a call makes. The others run too.
async fn measure(
    transactions: &[RouteTransaction],
    registrations: Vec<RouteStep>,
    trader: &AccountId,
    token_in: &TokenId,
) -> Result<Vec<NearGas>, String> {
    let block_hash = final_block_hash()?;
    let calls = transactions
        .iter()
        .flat_map(|transaction| &transaction.calls)
        .collect::<Vec<_>>();
    let registrations = resolve_steps(&Mainnet, registrations)
        .await
        .ok_or("The registrations of the stand-in couldn't be found out")?;
    let holdings = holdings_steps(trader, token_in, &calls, registrations, block_hash).await?;
    let holdings_transactions = holdings
        .iter()
        .filter(|step| matches!(step, SimulationStep::Transaction(_)))
        .count();
    // The holdings give the trader more than a route sends of an input in their wallet, but
    // nothing of their balances on Rhea and Intear DEX
    let input_in_wallet = matches!(token_in, TokenId::Near | TokenId::Nep141(_));
    let route_transactions = transactions.iter().zip(1..).map(|(transaction, nonce)| {
        let measured_call_gas =
            SIMULATED_CALL_GAS.min(most_attached_per_call(transaction.calls.len()));
        let actions = transaction
            .calls
            .iter()
            .map(|call| {
                let attached = call
                    .fixed_gas()
                    .map_or(measured_call_gas, |gas| gas.needs_attached);
                let call = call.clone().with_99_percent_slippage();
                let call = match input_in_wallet {
                    true => call.with_100x_max_input(),
                    false => call,
                };
                call.into_action(attached)
            })
            .collect();
        keyless_transaction(
            trader,
            transaction.calls[0].receiver_id(),
            actions,
            nonce,
            block_hash,
        )
    });
    let simulated_transactions = holdings
        .into_iter()
        .chain(route_transactions)
        .collect::<Vec<_>>();
    let simulated = SIMULATION_RPC
        .INTEAR_simulate_transactions(
            std::slice::from_ref(&simulated_transactions),
            BlockReference::Finality(Finality::Final),
        )
        .await
        .map_err(|err| format!("Simulating failed: {err:?}"))?;
    let sequence = match oracle_request_of(&simulated.sequences[0])? {
        None => simulated.sequences.into_iter().next().unwrap(),
        Some(request) => {
            let response = oracle_response(&request, block_hash).await?;
            // The response is sent the block after the request is made
            let wait = request.height.saturating_sub(simulated.block_height);
            let with_response = SIMULATION_RPC
                .INTEAR_simulate_transactions(
                    &[
                        simulated_transactions,
                        vec![
                            SimulationStep::Wait { blocks: wait },
                            SimulationStep::Transaction(response),
                        ],
                    ],
                    BlockReference::BlockId(BlockId::Height(simulated.block_height)),
                )
                .await
                .map_err(|err| format!("Simulating with the oracle's response failed: {err:?}"))?;
            let mut sequences = with_response.sequences.into_iter();
            let sequence = sequences.next().unwrap();
            for responded in sequences.flatten() {
                burnt_by(&responded).map_err(|err| format!("The oracle's response: {err}"))?;
            }
            sequence
        }
    };
    if sequence.len() != holdings_transactions + transactions.len() {
        return Err(format!(
            "{} of {} transactions were simulated",
            sequence.len(),
            holdings_transactions + transactions.len()
        ));
    }
    let (holdings_outcomes, route_outcomes) = sequence.split_at(holdings_transactions);
    for outcome in holdings_outcomes {
        burnt_by(outcome).map_err(|err| format!("Giving the trader what they swap: {err}"))?;
    }
    let burnt = route_outcomes
        .iter()
        .map(burnt_by)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(transactions
        .iter()
        .zip(burnt)
        .filter(|(transaction, _)| transaction.measured)
        .map(|(_, burnt)| burnt)
        .collect())
}

/// What the transaction and its receipts burnt, if none failed
fn burnt_by(transaction: &SimulatedTransaction) -> Result<NearGas, String> {
    let outcome = transaction
        .details
        .final_execution_outcome
        .as_ref()
        .ok_or("A transaction has no outcome")?;
    total_burnt(outcome)
}

/// Of what the signer pays for: refunds burn no tokens, though those to implicit accounts have
/// gas burnt
fn total_burnt(outcome: &FinalExecutionOutcomeWithReceiptView) -> Result<NearGas, String> {
    let outcomes = [&outcome.final_outcome.transaction_outcome]
        .into_iter()
        .chain(&outcome.final_outcome.receipts_outcome);
    let mut burnt = NearGas::from_gas(0);
    for ExecutionOutcomeWithIdView { outcome, .. } in outcomes {
        if let ExecutionStatusView::Failure(failure) = &outcome.status {
            return Err(format!("{} failed: {failure:?}", outcome.executor_id));
        }
        if outcome.tokens_burnt > 0 {
            burnt = burnt.saturating_add(outcome.gas_burnt.0);
        }
    }
    Ok(burnt)
}

/// An oracle request that a simulated transaction made, of the event Intear DEX logs
struct OracleRequestEvent {
    oracle_key: PublicKeyHandle,
    request: Vec<u8>,
    nonce: String,
    /// Of the receipt that made it
    height: BlockHeight,
}

#[derive(Deserialize)]
struct OracleRequestData {
    oracle_key: PublicKeyHandle,
    request: String,
    nonce: String,
}

/// The request the transactions made, as one call of a route asks the oracle at most
fn oracle_request_of(
    transactions: &[SimulatedTransaction],
) -> Result<Option<OracleRequestEvent>, String> {
    let mut requests = Vec::new();
    for transaction in transactions {
        let Some(outcome) = &transaction.details.final_execution_outcome else {
            continue;
        };
        for receipt in &outcome.final_outcome.receipts_outcome {
            for log in &receipt.outcome.logs {
                let Some(event) = log.strip_prefix("EVENT_JSON:") else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<Value>(event) else {
                    continue;
                };
                if event["standard"] != "inteardex" || event["event"] != "oracle_request" {
                    continue;
                }
                let data = OracleRequestData::deserialize(&event["data"])
                    .map_err(|err| format!("An oracle request event isn't one: {err}"))?;
                let height = *transaction
                    .outcome_block_heights
                    .get(&receipt.id)
                    .ok_or("No height of the receipt that asked the oracle")?;
                requests.push(OracleRequestEvent {
                    oracle_key: data.oracle_key,
                    request: BASE64_STANDARD
                        .decode(&data.request)
                        .map_err(|err| format!("An oracle request isn't base64: {err}"))?,
                    nonce: data.nonce,
                    height,
                });
            }
        }
    }
    match <[_; 1]>::try_from(requests) {
        Ok([request]) => Ok(Some(request)),
        Err(requests) if requests.is_empty() => Ok(None),
        Err(_) => Err("A route asked the oracle more than once".to_string()),
    }
}

/// What `GET /prices` of the oracle's HTTP API answers: every asset it prices, and the response to
/// a request of the prices of all of them
#[derive(Deserialize)]
struct OraclePricesView {
    oracle_key: PublicKeyHandle,
    asset_ids: Vec<AssetId>,
    response: PricesResponse,
}

/// The transaction of the oracle's response to the request, with its prices now
async fn oracle_response(
    request: &OracleRequestEvent,
    block_hash: CryptoHash,
) -> Result<Transaction, String> {
    let prices_url = format!("{}/prices", pathfinder::qoq::ORACLE_URL);
    let view: OraclePricesView = HTTP
        .get(&prices_url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| format!("No prices from {prices_url}: {err}"))?
        .json()
        .await
        .map_err(|err| format!("No prices from {prices_url}: {err}"))?;
    if view.oracle_key != request.oracle_key {
        return Err(format!("No oracle of {}", request.oracle_key));
    }
    let asked: PricesRequest = borsh::from_slice(&request.request)
        .map_err(|err| format!("An oracle request isn't one of prices: {err}"))?;
    let indices = asked
        .asset_ids
        .iter()
        .map(|asset_id| view.asset_ids.iter().position(|priced| priced == asset_id))
        .collect::<Option<Vec<_>>>()
        .ok_or("The oracle doesn't price an asset that's asked")?;
    let response = PricesResponse {
        exchanges: view
            .response
            .exchanges
            .into_iter()
            .map(|exchange| ExchangePrices {
                prices: indices
                    .iter()
                    .map(|&index| exchange.prices[index])
                    .collect(),
                exchange: exchange.exchange,
            })
            .collect(),
    };
    let oracle_key = request
        .oracle_key
        .to_string()
        .parse::<PublicKey>()
        .map_err(|err| format!("The oracle's key isn't one to sign with: {err}"))?;
    let nonce = gas_key_nonce(&request.oracle_key).await?;
    let args = json!({
        "request_hash": CryptoHash(Sha256::digest(&request.request).into()).to_string(),
        "response": BASE64_STANDARD.encode(borsh::to_vec(&response).unwrap()),
        "nonces": [request.nonce],
    });
    let engine: AccountId = INTEAR_DEX_CONTRACT_ID.parse().unwrap();
    Ok(Transaction::V1(TransactionV1 {
        signer_id: engine.clone(),
        public_key: oracle_key,
        nonce: TransactionNonce::from_nonce_and_index(nonce + 1, 0),
        receiver_id: engine,
        block_hash,
        actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "oracle_respond".to_string(),
            args: serde_json::to_vec(&args).unwrap(),
            gas: near_min_api::types::Gas(ORACLE_RESPONSE_GAS),
            deposit: NearToken::from_yoctonear(0),
        }))],
        nonce_mode: NonceMode::Monotonic,
    }))
}

/// Of the oracle's gas key on Intear DEX, which signs its responses
async fn gas_key_nonce(oracle_key: &PublicKeyHandle) -> Result<u64, String> {
    let response: Value = HTTP
        .post(SIMULATION_RPC_URL.as_str())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "query",
            "params": {
                "request_type": "view_gas_key_nonces",
                "finality": "final",
                "account_id": INTEAR_DEX_CONTRACT_ID,
                "public_key": oracle_key.to_string(),
            },
        }))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| format!("No nonce of the oracle's key: {err}"))?
        .json()
        .await
        .map_err(|err| format!("No nonce of the oracle's key: {err}"))?;
    response["result"]["nonces"][0]
        .as_u64()
        .ok_or_else(|| format!("No nonce of the oracle's key: {response}"))
}

fn shape_of_transaction(calls: &[CallBlueprint], trader: Option<&AccountId>) -> String {
    let call_shapes = calls
        .iter()
        .map(|call| {
            let call_shape = match call.clone().into_action(NearGas::from_gas(0)) {
                Action::FunctionCall(function_call) => shape(
                    &call.receiver_id(),
                    &function_call.method_name,
                    &function_call.args,
                    trader,
                ),
                action => unreachable!("Routes make function calls, not {action:?}"),
            };
            match call {
                // What a swap on Rhea DCL burns depends on the points of its pools that it goes
                // through, which its amounts don't tell
                CallBlueprint::InputNep141TransferCallRheaDclSwap { work, .. } => {
                    format!("{call_shape} {work:?}")
                }
                _ => call_shape,
            }
        })
        .collect();
    transaction_shape(call_shapes, trader)
}

/// Of the calls of a transaction, one after another, and of the type of the trader's account, as
/// NEAR sent to an implicit account pays for creating it, which burns 7.3 Tgas more. Quotes
/// without a trader take what transactions of named accounts burn.
fn transaction_shape(call_shapes: Vec<String>, trader: Option<&AccountId>) -> String {
    let trader_account_type = trader.map_or(AccountType::NamedAccount, |trader| {
        trader.get_account_type()
    });
    format!("{} by {trader_account_type:?}", call_shapes.join("; "))
}

/// The call without what doesn't change what it burns: the trader, amounts, and the signature of a
/// fee discount
fn shape(
    receiver_id: &AccountId,
    method_name: &str,
    args: &[u8],
    trader: Option<&AccountId>,
) -> String {
    let args = match serde_json::from_slice::<Value>(args) {
        Ok(args) => normalized(args, trader).to_string(),
        Err(_) => format!("<{} bytes>", args.len()),
    };
    format!("{receiver_id} {method_name} {args}")
}

fn normalized(value: Value, trader: Option<&AccountId>) -> Value {
    match value {
        Value::String(string) => normalized_string(string, trader),
        Value::Array(values) => values
            .into_iter()
            .map(|value| normalized(value, trader))
            .collect(),
        Value::Object(fields) => fields
            .into_iter()
            .map(|(name, value)| {
                let value = match name.as_str() {
                    "fee_discount" if !value.is_null() => "<fee discount>".into(),
                    _ => normalized(value, trader),
                };
                (name, value)
            })
            .collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) => value,
    }
}

/// Amounts are strings of digits, messages to DEXes JSON or base64
fn normalized_string(string: String, trader: Option<&AccountId>) -> Value {
    if trader.is_some_and(|trader| trader.as_str() == string) {
        return "<trader>".into();
    }
    if string.parse::<u128>().is_ok() {
        return "<amount>".into();
    }
    if string.parse::<AccountId>().is_ok() {
        return string.into();
    }
    if let Ok(message @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str(&string) {
        return normalized(message, trader);
    }
    if let Ok(bytes) = BASE64_STANDARD.decode(&string) {
        return format!("<{} bytes>", bytes.len()).into();
    }
    string.into()
}

/// Of a swap on a DEX whose route search weighs its hops
fn hops_of(call: &CallBlueprint) -> Option<(SearchedDex, u64)> {
    match call {
        CallBlueprint::InputNep141TransferCallRheaSwap { actions, .. }
        | CallBlueprint::RheaSwap { actions, .. } => {
            Some((SearchedDex::Rhea, actions.len() as u64))
        }
        CallBlueprint::InputNep141TransferCallRheaDclSwap { pool_ids, .. } => {
            Some((SearchedDex::RheaDcl, pool_ids.len() as u64))
        }
        _ => None,
    }
}

/// Weighs the hops of the route searches of Rhea and Rhea DCL by what a hop of their measured
/// swaps burnt
fn fit_search_hop_gas(measured: &HashMap<String, Measured>) {
    for dex in [SearchedDex::Rhea, SearchedDex::RheaDcl] {
        let points = measured
            .values()
            .filter_map(|measured| match measured.hops {
                Some((hops_dex, hops)) if hops_dex == dex => Some((hops, measured.burnt.as_gas())),
                _ => None,
            })
            .collect::<Vec<_>>();
        if points.len() < MIN_FITTED_SWAPS {
            continue;
        }
        let Some(hop_gas) = slope(&points) else {
            continue;
        };
        match dex {
            SearchedDex::Rhea => pathfinder::rhea::set_search_hop_gas(hop_gas),
            SearchedDex::RheaDcl => pathfinder::dcl::set_search_hop_gas(hop_gas),
        }
    }
}

/// Of the least squares line through the points of hops and what they burnt, when there are
/// points of more than one number of hops and it rises
fn slope(points: &[(u64, u64)]) -> Option<NearGas> {
    let (mut count, mut hops, mut burnt, mut hops_squared, mut hops_burnt) = (0, 0, 0, 0, 0);
    for &(point_hops, point_burnt) in points {
        let (point_hops, point_burnt) = (i128::from(point_hops), i128::from(point_burnt));
        count += 1;
        hops += point_hops;
        burnt += point_burnt;
        hops_squared += point_hops * point_hops;
        hops_burnt += point_hops * point_burnt;
    }
    let spread = count * hops_squared - hops * hops;
    if spread == 0 {
        return None;
    }
    let slope = (count * hops_burnt - hops * burnt) / spread;
    u64::try_from(slope)
        .ok()
        .filter(|slope| *slope > 0)
        .map(NearGas::from_gas)
}

/// Compares what an executed transaction burnt with what transactions of its shape were measured
/// to burn, which it then counts as
pub fn record_executed(executed: &FinalExecutionOutcomeWithReceiptView) {
    let transaction = &executed.final_outcome.transaction;
    let call_shapes = transaction
        .actions
        .iter()
        .map(|action| match action {
            ActionView::FunctionCall {
                method_name, args, ..
            } => Some(shape(
                &transaction.receiver_id,
                method_name,
                args,
                Some(&transaction.signer_id),
            )),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    let Some(call_shapes) = call_shapes else {
        return;
    };
    let Ok(burnt) = total_burnt(executed) else {
        return;
    };
    let shape = transaction_shape(call_shapes, Some(&transaction.signer_id));
    let mut measured = MEASURED.lock().unwrap();
    let Some(measured) = measured.get_mut(&shape) else {
        return;
    };
    measured.burnt = burnt;
    measured.at = Instant::now();
}

#[cfg(test)]
mod tests {
    use pathfinder::dcl::DclWork;

    use super::*;
    use crate::{
        calls::{RheaDclSwap, RheaOutput},
        types::{Amount, DexId, Gas, TokenId},
    };

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    #[test]
    fn logged_errors_leave_out_urls() {
        assert_eq!(
            without_urls(
                r#"Simulating failed: Reqwest(reqwest::Error { kind: Status(429, None), url: "https://rpc.example.com/key" })"#
            ),
            r#"Simulating failed: Reqwest(reqwest::Error { kind: Status(429, None), url: "<url>" })"#
        );
        assert_eq!(
            without_urls(
                "No nonce of the oracle's key: HTTP status client error (429 Too Many Requests) for url (https://rpc.example.com/key)"
            ),
            "No nonce of the oracle's key: HTTP status client error (429 Too Many Requests) for url (<url>)"
        );
        assert_eq!(
            without_urls("No prices from http://prices.example.com/key"),
            "No prices from <url>"
        );
    }

    #[test]
    fn shapes_leave_out_the_trader_and_amounts() {
        let swap = |trader: &str, amount: &str| {
            let message = json!({
                "receiver_id": trader,
                "min_amount_out": amount,
                "pool_ids": ["usdc.near|wrap.near|2000"],
            });
            let args = json!({
                "receiver_id": "dex.intear.near",
                "amount": amount,
                "msg": message.to_string(),
                "fee_discount": {"signature": trader},
            });
            shape(
                &account("usdc.near"),
                "ft_transfer_call",
                &serde_json::to_vec(&args).unwrap(),
                Some(&account(trader)),
            )
        };
        assert_eq!(swap("alice.near", "1000"), swap("bob.near", "3000000"));
        assert_eq!(
            swap("alice.near", "1000"),
            r#"usdc.near ft_transfer_call {"receiver_id":"dex.intear.near","amount":"<amount>","msg":{"receiver_id":"<trader>","min_amount_out":"<amount>","pool_ids":["usdc.near|wrap.near|2000"]},"fee_discount":"<fee discount>"}"#
        );
    }

    #[test]
    fn swaps_on_rhea_dcl_are_of_the_shape_of_the_points_they_go_through() {
        let swap = |amount, crossings| {
            shape_of_transaction(
                &[CallBlueprint::InputNep141TransferCallRheaDclSwap {
                    token_id: account("wrap.near"),
                    amount,
                    pool_ids: vec!["usdc.near|wrap.near|2000".to_string()],
                    token_out: account("usdc.near"),
                    swap: RheaDclSwap::ExactIn {
                        min_amount_out: amount,
                    },
                    swap_gas: Gas::from_ggas(20_000, 20_000),
                    work: DclWork {
                        ranges: crossings + 1,
                        crossings,
                        ..DclWork::default()
                    },
                    output: RheaOutput::Nep141,
                    referral_id: account("app.near"),
                }],
                None,
            )
        };
        assert_eq!(swap(10, 0), swap(1000, 0));
        assert_ne!(swap(1000, 0), swap(1000, 1));
    }

    #[test]
    fn transactions_of_implicit_accounts_are_of_other_shapes() {
        let unwrap = [CallBlueprint::UnwrapNear {
            amount: NearToken::from_near(1),
            trader_account_id: None,
        }];
        let shape_as =
            |trader: Option<&str>| shape_of_transaction(&unwrap, trader.map(account).as_ref());
        assert_eq!(shape_as(Some("alice.near")), shape_as(None));
        assert_ne!(
            shape_as(Some("alice.near")),
            shape_as(Some(&"a".repeat(64)))
        );
        assert_eq!(
            shape_as(Some(&"a".repeat(64))),
            r#"wrap.near near_withdraw {"amount":"<amount>"} by NearImplicitAccount"#
        );
    }

    #[test]
    fn a_shape_has_the_size_of_base64_and_of_args_that_are_not_json() {
        let args = json!({"operations": BASE64_STANDARD.encode([0; 30])});
        assert_eq!(
            shape(
                &account("dex.intear.near"),
                "execute_operations",
                &serde_json::to_vec(&args).unwrap(),
                None,
            ),
            r#"dex.intear.near execute_operations {"operations":"<30 bytes>"}"#
        );
        assert_eq!(
            shape(
                &account("dex.intear.near"),
                "borsh_method",
                &[1, 2, 3],
                None
            ),
            "dex.intear.near borsh_method <3 bytes>"
        );
    }

    #[test]
    fn each_measured_call_attaches_what_its_transaction_burnt_and_its_margin() {
        let near = NearToken::from_yoctonear;
        let unwrap = |yocto| CallBlueprint::UnwrapNear {
            amount: near(yocto),
            trader_account_id: None,
        };
        let route = ResolvedRoute {
            dex_id: DexId::Wrap,
            estimated_amount: Amount::AmountOut(3),
            worst_case_amount: Amount::AmountOut(3),
            token_output: TokenId::Near,
            discount: None,
            calls: vec![
                CallBlueprint::Nep141StorageDeposit {
                    token_id: account("wrap.near"),
                    account_id: None,
                    amount: near(1),
                },
                CallBlueprint::WrapNear { amount: near(3) },
                unwrap(1),
                unwrap(2),
            ],
            fee_calls: vec![CallBlueprint::InputNep141FeeTransfer {
                token_id: account("wrap.near"),
                receiver_id: account("app.near"),
                amount: 1,
                swap_referrer_id: account("app.near"),
            }],
        };
        // The storage deposit and the wrapping are of fixed gas, in a transaction of their own
        let shapes = shapes_of(&route, None);
        assert_eq!(shapes.len(), 1);
        let measured_as = |burnt| {
            MEASURED.lock().unwrap().insert(
                shapes[0].clone(),
                Measured {
                    burnt,
                    at: Instant::now(),
                    hops: None,
                },
            );
            measured_gas(&route, &shapes).unwrap()
        };
        let ggas = NearGas::from_ggas;
        assert_eq!(
            measured_as(NearGas::from_tgas(7)),
            (
                RouteGas {
                    calls: vec![ggas(11_573), ggas(11_606), ggas(17_000), ggas(17_000)],
                    fee_calls: vec![ggas(12_180)],
                    estimated_burn: ggas(1_573 + 1_606 + 7_000 + 2_180),
                },
                false
            )
        );
        // Under what the transaction can attach to both
        assert_eq!(
            measured_as(NearGas::from_tgas(600)).0.calls[2..],
            [NearGas::from_tgas(500); 2]
        );
    }

    #[test]
    fn the_hop_gas_is_the_slope_of_what_swaps_burnt_by_their_hops() {
        let tgas = |tgas| NearGas::from_tgas(tgas).as_gas();
        assert_eq!(
            slope(&[(1, tgas(10)), (2, tgas(13)), (3, tgas(16)), (1, tgas(10))]),
            Some(NearGas::from_tgas(3))
        );
        assert_eq!(slope(&[(2, tgas(10)), (2, tgas(13))]), None);
        assert_eq!(slope(&[(1, tgas(13)), (2, tgas(10))]), None);
        assert_eq!(slope(&[]), None);
    }
}
