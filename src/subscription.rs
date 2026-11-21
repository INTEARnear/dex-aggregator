use std::{collections::HashMap, fmt::Display, net::IpAddr, str::FromStr, time::Duration};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        WebSocketUpgrade,
    },
    http::{HeaderMap, Uri},
    response::Response,
    Extension,
};
use chrono::{DateTime, Utc};
use near_min_api::types::{AccountId, CryptoHash};
use serde::Deserialize;
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use tracing::info;

use swap_router_types::hook::RequestDetails;

use crate::{
    find_routes, hooks,
    outcome::{self, StoredQuote},
    rate_limit::{ClientIp, RequestLimit},
    shared_utils::Mainnet,
    types::{Amount, DexId, OutcomeWaitUntil, Quote, SwapOutcome, SwapRequest},
    validate_request,
};

/// If routing took 30ms, wait at least REROUTE_COOLDOWN_FACTOR * 30ms (rounded up to nearest block) before rerouting
const REROUTE_COOLDOWN_FACTOR: u32 = 10;
const MAX_REROUTE_COOLDOWN: Duration = Duration::from_secs(3);

/// A message from the client. With `outcome`, it asks for the outcome of a quote's transactions,
/// and routing goes on. Otherwise it replaces the previous route request, or only stops it if
/// there's no `query`.
#[derive(Deserialize)]
struct ClientMessage {
    /// Sent back with every update of this request, so updates of the
    /// previous requests can be told apart
    id: u64,
    /// Same query parameter string as `/route` takes
    query: Option<String>,
    /// Leaves the fee transactions out of the quotes, and gives each quote a `quote_id`. The
    /// `outcome` of its transactions has them in its next steps once the swap went through, with
    /// the quote improvement fee of routes that Intear DEX doesn't take it within. Needs
    /// `trader_account_id`. The quotes' gas and storage costs still count the fee transactions.
    #[serde(default)]
    fees_in_next_steps: bool,
    outcome: Option<QuoteOutcomeRequest>,
}

/// Of a quote of a route request with `fees_in_next_steps`, on this connection or any other
#[derive(Deserialize)]
struct QuoteOutcomeRequest {
    quote_id: i64,
    /// The quote's executed transactions
    tx_hashes: Vec<CryptoHash>,
    #[serde(default)]
    wait_until: OutcomeWaitUntil,
}

pub async fn subscribe_handler(
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(limit): Extension<RequestLimit>,
    Extension(database): Extension<SqlitePool>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let request_details = hooks::request_details(ip, &uri, &headers);
    upgrade.on_upgrade(move |socket| subscribe(socket, database, request_details, limit))
}

async fn subscribe(
    mut socket: WebSocket,
    database: SqlitePool,
    request_details: RequestDetails,
    limit: RequestLimit,
) {
    let (updates_sender, mut updates) = mpsc::unbounded_channel::<String>();
    let mut routing: Option<JoinHandle<()>> = None;
    // Aborted with the connection
    let mut outcomes = JoinSet::new();
    loop {
        tokio::select! {
            message = socket.recv() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                };
                let Ok(ClientMessage { id, query, fees_in_next_steps, outcome }) =
                    serde_json::from_str(&text)
                else {
                    break;
                };
                if let Some(outcome) = outcome {
                    if let Err(error) = limit.check() {
                        let update = json!({ "id": id, "error": error }).to_string();
                        if socket.send(Message::text(update)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    let database = database.clone();
                    let updates_sender = updates_sender.clone();
                    outcomes.spawn(async move {
                        let update = match quote_outcome(&database, outcome).await {
                            Ok(outcome) => json!({ "id": id, "outcome": outcome }),
                            Err(error) => json!({ "id": id, "error": error }),
                        };
                        // The connection is gone if nothing receives it
                        updates_sender.send(update.to_string()).ok();
                    });
                    continue;
                }
                if let Some(previous) = routing.take() {
                    previous.abort();
                }
                let Some(query) = query else {
                    continue;
                };
                let request = parse_route_request(&query, &limit).and_then(|request| {
                    let fees_in_next_steps = match (fees_in_next_steps, &request.trader_account_id) {
                        (false, _) => None,
                        (true, Some(trader_account_id)) => {
                            Some((database.clone(), trader_account_id.clone()))
                        }
                        (true, None) => {
                            return Err("fees_in_next_steps needs trader_account_id".to_string())
                        }
                    };
                    Ok((request, fees_in_next_steps))
                });
                match request {
                    Ok((request, fees_in_next_steps)) => {
                        info!("Received route subscription request: {:?}", request);
                        routing = Some(tokio::spawn(route_updates(
                            id,
                            request,
                            fees_in_next_steps,
                            request_details.clone(),
                            updates_sender.clone(),
                        )));
                    }
                    Err(error) => {
                        let update = json!({ "id": id, "error": error }).to_string();
                        if socket.send(Message::text(update)).await.is_err() {
                            break;
                        }
                    }
                }
            }
            Some(update) = updates.recv() => {
                if socket.send(Message::text(update)).await.is_err() {
                    break;
                }
            }
            Some(_) = outcomes.join_next(), if !outcomes.is_empty() => {}
        }
    }
    if let Some(routing) = routing {
        routing.abort();
    }
}

fn parse_route_request(query: &str, limit: &RequestLimit) -> Result<SwapRequest, String> {
    limit.check()?;
    let request: SwapRequest =
        serde_urlencoded::from_str(query).map_err(|error| error.to_string())?;
    validate_request(&request).map_err(|(_, error)| error)?;
    Ok(request)
}

/// The outcome of the stored quote's transactions, with its fees in the next steps, recorded
/// with the quote
async fn quote_outcome(
    database: &SqlitePool,
    request: QuoteOutcomeRequest,
) -> Result<SwapOutcome, String> {
    let quote = stored_quote(database, request.quote_id)
        .await?
        .ok_or_else(|| format!("Quote {} doesn't exist", request.quote_id))?;
    let outcome = outcome::quote_outcome(
        &Mainnet,
        &Mainnet,
        quote,
        request.tx_hashes.clone(),
        request.wait_until,
    )
    .await
    .map_err(|(_, error)| error)?;
    record_outcome(database, request.quote_id, &request.tx_hashes, &outcome).await?;
    Ok(outcome)
}

/// How the quotes stored with it were found
struct Routing {
    created_at: DateTime<Utc>,
    ip: IpAddr,
    duration: Duration,
    dex_durations: HashMap<DexId, Duration>,
}

/// The quotes without their fee transactions, each with the ID that the outcome of its
/// transactions is asked for by, once it's stored
async fn with_fees_in_next_steps(
    database: &SqlitePool,
    request: &SwapRequest,
    routing: &Routing,
    trader_account_id: &AccountId,
    found_quotes: Vec<Quote>,
) -> Result<Vec<Quote>, String> {
    let (quotes, stored_quotes): (Vec<_>, Vec<_>) = found_quotes
        .into_iter()
        .map(|quote| StoredQuote::split(quote, request, trader_account_id.clone()))
        .unzip();
    let ids = insert_quotes(database, request, routing, &stored_quotes).await?;
    Ok(quotes
        .into_iter()
        .zip(ids)
        .map(|(quote, id)| Quote {
            quote_id: Some(id),
            ..quote
        })
        .collect())
}

/// Sends the routes as soon as they're found, then again whenever newer pools change them
async fn route_updates(
    id: u64,
    request: SwapRequest,
    fees_in_next_steps: Option<(SqlitePool, AccountId)>,
    request_details: RequestDetails,
    updates: mpsc::UnboundedSender<String>,
) {
    let mut pools_updated = pathfinder::pools_updated();
    let mut sent_quotes = None;
    loop {
        // Routes found below use pools at least this new, so only newer ones reroute
        pools_updated.borrow_and_update();
        let started_at = Instant::now();
        let created_at = Utc::now();
        let (quotes, dex_durations) = find_routes(&request, &request_details).await;
        let duration = started_at.elapsed();

        if sent_quotes.as_ref() != Some(&quotes) {
            let routes = match &fees_in_next_steps {
                Some((database, trader_account_id)) => {
                    let routing = Routing {
                        created_at,
                        ip: request_details.ip,
                        duration,
                        dex_durations,
                    };
                    match with_fees_in_next_steps(
                        database,
                        &request,
                        &routing,
                        trader_account_id,
                        quotes.clone(),
                    )
                    .await
                    {
                        Ok(routes) => routes,
                        Err(error) => {
                            updates
                                .send(json!({ "id": id, "error": error }).to_string())
                                .ok();
                            return;
                        }
                    }
                }
                None => quotes.clone(),
            };
            let update = json!({ "id": id, "routes": &routes }).to_string();
            if updates.send(update).is_err() {
                return;
            }
            sent_quotes = Some(quotes);
        }

        let cooldown = (duration * REROUTE_COOLDOWN_FACTOR).min(MAX_REROUTE_COOLDOWN);
        tokio::time::sleep_until(started_at + cooldown).await;
        pools_updated
            .changed()
            .await
            .expect("Pools sender is static");
    }
}

/// The database of the quotes, at `STATS_DATABASE_URL`, migrated
pub async fn open_database() -> SqlitePool {
    let url = std::env::var("STATS_DATABASE_URL").unwrap_or_else(|_| "stats.sqlite".to_string());
    let options = SqliteConnectOptions::from_str(&url)
        .unwrap_or_else(|err| panic!("Invalid STATS_DATABASE_URL: {err}"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let database = SqlitePool::connect_with(options)
        .await
        .unwrap_or_else(|err| panic!("Failed to open the quotes database {url}: {err}"));
    sqlx::migrate!()
        .run(&database)
        .await
        .unwrap_or_else(|err| panic!("Failed to migrate the quotes database: {err}"));
    info!("Writing quotes to {url}");
    database
}

/// Stores the quotes found by one routing, and gives their IDs
async fn insert_quotes(
    database: &SqlitePool,
    request: &SwapRequest,
    routing: &Routing,
    quotes: &[StoredQuote],
) -> Result<Vec<i64>, String> {
    let store_error = |err: sqlx::Error| format!("Failed to store the quotes: {err}");
    let (swap_type, amount) = match request.amount {
        Amount::AmountIn(amount) => ("exact_in", amount),
        Amount::AmountOut(amount) => ("exact_out", amount),
    };
    let mut transaction = database.begin().await.map_err(store_error)?;
    let mut ids = Vec::with_capacity(quotes.len());
    for quote in quotes {
        let (Amount::AmountIn(estimated_amount) | Amount::AmountOut(estimated_amount)) =
            quote.estimated_amount;
        let dex_duration = routing
            .dex_durations
            .get(&quote.dex_id)
            .expect("Routing is timed on every DEX that quotes");
        // The routing's first quote is given its own ID once it has one
        let routing_id = ids.first().copied().unwrap_or(0);
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO quotes (routing_id, created_at, ip, token_in, token_out, swap_type, amount, referrer_id, trader_account_id, routing_duration_ms, dex_id, dex_duration_ms, estimated_amount, fee_amount, fee_instructions, quote_improvement_receiver)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id",
        )
        .bind(routing_id)
        .bind(routing.created_at)
        .bind(routing.ip.to_string())
        .bind(quote.token_in.to_string())
        .bind(quote.token_out.to_string())
        .bind(swap_type)
        .bind(amount.to_string())
        .bind(request.referrer_id.as_ref().map(|id| id.to_string()))
        .bind(quote.trader_account_id.to_string())
        .bind(routing.duration.as_millis() as i64)
        .bind(quote.dex_id.to_string())
        .bind(dex_duration.as_millis() as i64)
        .bind(estimated_amount.to_string())
        .bind(quote.fee_amount.to_string())
        .bind(
            serde_json::to_string(&quote.fee_instructions)
                .expect("Execution instructions serialize"),
        )
        .bind(
            quote
                .quote_improvement_receiver
                .as_ref()
                .map(|receiver_id| receiver_id.to_string()),
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(store_error)?;
        ids.push(id);
    }
    if let Some(first_id) = ids.first() {
        sqlx::query("UPDATE quotes SET routing_id = id WHERE id = ?")
            .bind(first_id)
            .execute(&mut *transaction)
            .await
            .map_err(store_error)?;
    }
    transaction.commit().await.map_err(store_error)?;
    Ok(ids)
}

async fn stored_quote(database: &SqlitePool, id: i64) -> Result<Option<StoredQuote>, String> {
    let row: Option<QuoteRow> = sqlx::query_as(
        "SELECT token_in, token_out, trader_account_id, dex_id, swap_type, estimated_amount, fee_amount, fee_instructions, quote_improvement_receiver
        FROM quotes WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(database)
    .await
    .map_err(|err| format!("Failed to read quote {id}: {err}"))?;
    row.map(|row| {
        row.into_stored_quote()
            .map_err(|err| format!("Quote {id} is invalid: {err}"))
    })
    .transpose()
}

/// Replaces the quote's outcome, unless it's settled and this one isn't
async fn record_outcome(
    database: &SqlitePool,
    quote_id: i64,
    tx_hashes: &[CryptoHash],
    outcome: &SwapOutcome,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE quotes SET outcome_at = ?1, tx_hashes = ?2, charge_fee = ?3, outcome_amount_in = ?4, outcome_amount_out = ?5, quote_improvement_fee = ?6, settled = ?7
        WHERE id = ?8 AND (?7 OR settled IS NOT TRUE)",
    )
    .bind(Utc::now())
    .bind(serde_json::to_string(tx_hashes).expect("Hashes serialize"))
    .bind(outcome.charge_fee)
    .bind(outcome.amount_in.to_string())
    .bind(outcome.amount_out.to_string())
    .bind(outcome.quote_improvement_fee.map(|fee| fee.to_string()))
    .bind(outcome.settled)
    .bind(quote_id)
    .execute(database)
    .await
    .map_err(|err| format!("Failed to record the outcome of quote {quote_id}: {err}"))?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct QuoteRow {
    token_in: String,
    token_out: String,
    trader_account_id: String,
    dex_id: String,
    swap_type: String,
    estimated_amount: String,
    fee_amount: String,
    fee_instructions: String,
    quote_improvement_receiver: Option<String>,
}

impl QuoteRow {
    fn into_stored_quote(self) -> Result<StoredQuote, String> {
        fn parse<T: FromStr>(name: &str, value: &str) -> Result<T, String>
        where
            T::Err: Display,
        {
            value
                .parse()
                .map_err(|err| format!("{name} {value:?}: {err}"))
        }
        let estimated_amount = parse("estimated_amount", &self.estimated_amount)?;
        Ok(StoredQuote {
            token_in: parse("token_in", &self.token_in)?,
            token_out: parse("token_out", &self.token_out)?,
            trader_account_id: parse("trader_account_id", &self.trader_account_id)?,
            dex_id: parse("dex_id", &self.dex_id)?,
            estimated_amount: match self.swap_type.as_str() {
                "exact_in" => Amount::AmountOut(estimated_amount),
                "exact_out" => Amount::AmountIn(estimated_amount),
                swap_type => return Err(format!("swap_type {swap_type:?}")),
            },
            fee_amount: parse("fee_amount", &self.fee_amount)?,
            fee_instructions: serde_json::from_str(&self.fee_instructions)
                .map_err(|err| format!("fee_instructions: {err}"))?,
            quote_improvement_receiver: self
                .quote_improvement_receiver
                .map(|receiver_id| parse("quote_improvement_receiver", &receiver_id))
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::types::TokenId;

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn stored_quote_on(dex_id: DexId, estimated_out: u128) -> StoredQuote {
        StoredQuote {
            token_in: TokenId::Near,
            token_out: TokenId::Nep141(account("usdc.near")),
            trader_account_id: account("trader.near"),
            dex_id,
            estimated_amount: Amount::AmountOut(estimated_out),
            fee_amount: 50,
            fee_instructions: Vec::new(),
            quote_improvement_receiver: Some(account("app.near")),
        }
    }

    fn swapped(amount_out: u128, settled: bool) -> SwapOutcome {
        SwapOutcome {
            charge_fee: true,
            amount_in: 950,
            amount_out,
            balance_changes: Vec::new(),
            next_steps: Vec::new(),
            error: None,
            settled,
            quote_improvement_fee: Some(5),
        }
    }

    #[tokio::test]
    async fn quotes_are_stored_with_their_routing_and_outcome() {
        // One connection, as every connection to memory is a database of its own
        let database = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&database).await.unwrap();
        let request: SwapRequest = serde_urlencoded::from_str(
            "token_in=near&token_out=usdc.near&amount_in=1000&slippage_type=Fixed&slippage=0.01&referrer_id=ref.near&trader_account_id=trader.near&quote_improvement_receiver=app.near",
        )
        .unwrap();
        let plach = DexId::Hook("Plach".into());
        let routing = Routing {
            created_at: Utc::now(),
            ip: "10.0.0.1".parse().unwrap(),
            duration: Duration::from_millis(80),
            dex_durations: HashMap::from([
                (DexId::Rhea, Duration::from_millis(30)),
                (plach.clone(), Duration::from_millis(70)),
            ]),
        };
        let quotes = [
            stored_quote_on(DexId::Rhea, 900),
            stored_quote_on(plach, 890),
        ];
        let ids = insert_quotes(&database, &request, &routing, &quotes)
            .await
            .unwrap();
        for (id, quote) in ids.iter().zip(&quotes) {
            assert_eq!(
                stored_quote(&database, *id).await.unwrap().as_ref(),
                Some(quote)
            );
        }
        assert_eq!(stored_quote(&database, 1_000).await.unwrap(), None);
        let rows: Vec<(i64, String, String, Option<String>, i64, i64)> = sqlx::query_as(
            "SELECT routing_id, ip, amount, referrer_id, routing_duration_ms, dex_duration_ms FROM quotes ORDER BY id",
        )
        .fetch_all(&database)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [30, 70].map(|dex_duration_ms| (
                ids[0],
                "10.0.0.1".to_string(),
                "1000".to_string(),
                Some("ref.near".to_string()),
                80,
                dex_duration_ms
            ))
        );

        let tx_hash: CryptoHash = "7xEDD8Bdu9arQn7dDghTDMzhvuwmiKsRFJovq8LyfVQw"
            .parse()
            .unwrap();
        let outcome_of = |id| {
            sqlx::query_as::<_, (String, bool, String, bool)>(
                "SELECT tx_hashes, charge_fee, outcome_amount_out, settled FROM quotes WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&database)
        };
        record_outcome(&database, ids[1], &[tx_hash], &swapped(895, false))
            .await
            .unwrap();
        record_outcome(&database, ids[1], &[tx_hash], &swapped(896, true))
            .await
            .unwrap();
        // An unsettled outcome asked for later doesn't replace the settled one
        record_outcome(&database, ids[1], &[tx_hash], &swapped(895, false))
            .await
            .unwrap();
        assert_eq!(
            outcome_of(ids[1]).await.unwrap(),
            (format!("[\"{tx_hash}\"]"), true, "896".to_string(), true)
        );
        let not_executed: Option<String> =
            sqlx::query_scalar("SELECT outcome_at FROM quotes WHERE id = ?")
                .bind(ids[0])
                .fetch_one(&database)
                .await
                .unwrap();
        assert_eq!(not_executed, None);
    }

    #[test]
    fn outcome_requests_are_told_apart_from_route_requests() {
        let message: ClientMessage = serde_json::from_str(
            r#"{"id":2,"outcome":{"quote_id":7,"tx_hashes":["7xEDD8Bdu9arQn7dDghTDMzhvuwmiKsRFJovq8LyfVQw"]}}"#,
        )
        .unwrap();
        let outcome = message.outcome.unwrap();
        assert_eq!(outcome.quote_id, 7);
        assert_eq!(
            outcome.tx_hashes,
            vec!["7xEDD8Bdu9arQn7dDghTDMzhvuwmiKsRFJovq8LyfVQw"
                .parse::<CryptoHash>()
                .unwrap()]
        );
        assert_eq!(outcome.wait_until, OutcomeWaitUntil::Executed);

        let message: ClientMessage =
            serde_json::from_str(r#"{"id":1,"query":"token_in=near","fees_in_next_steps":true}"#)
                .unwrap();
        assert!(message.outcome.is_none());
        assert!(message.fees_in_next_steps);

        // A malformed outcome request isn't taken for a route request that stops routing
        assert!(serde_json::from_str::<ClientMessage>(
            r#"{"id":2,"outcome":{"quote_id":"7","tx_hashes":[]}}"#
        )
        .is_err());
    }
}
