#![deny(clippy::float_arithmetic)]

use axum::{
    extract::{Json, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware,
    routing::get,
    Extension, Router,
};
use bigdecimal::{BigDecimal, Zero};
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use near_min_api::{
    types::{Finality, NearGas},
    QueryFinality, RpcClient,
};
use std::{
    env,
    future::Future,
    net::{IpAddr, SocketAddr},
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tokio::task::{AbortHandle, JoinHandle};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use swap_router_types::{
    hook::{IntearDexTrade, RequestDetails},
    types,
};

use crate::{
    hooks::inteardex::IntearDexSwap,
    rate_limit::{ClientIp, RateLimiter},
    shared_utils::{
        base_token, convert_to, estimated_gas_paid, fee_transactions_amount, final_block_hash,
        max_gas_cost, optimize_execution_instructions, price_raw, storage_deposits, BaseTokenId,
        Mainnet, NetworkView, RPC_CLIENT,
    },
    stats::{QueryStats, RouteOutcome, RouteStats, Stats},
    types::{
        Amount, DexId, ExecutionInstruction, OutcomeRequest, Quote, Route, SigningContext,
        Slippage, SwapOutcome, SwapRequest,
    },
};

static ACCESS_KEY_RPC_CLIENT: LazyLock<RpcClient> =
    LazyLock::new(|| RPC_CLIENT.clone().with_max_retries(3));

mod hooks;
mod outcome;
mod providers;
mod rate_limit;
mod shared_utils;
mod stats;
mod subscription;

pub trait Provider: Sync {
    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>>;

    fn dex_id(&self) -> DexId;
}

async fn route_handler(
    State(stats): State<Stats>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    uri: Uri,
    headers: HeaderMap,
    Query(request): Query<SwapRequest>,
) -> Result<Json<Vec<Quote>>, (StatusCode, String)> {
    let started_at = Instant::now();
    let timestamp = Utc::now();
    info!("Received route request: {:?}", request);

    validate_request(&request)?;
    let (quotes, route_stats) =
        find_routes(&request, &hooks::request_details(ip, &uri, &headers)).await;
    record_stats(
        &stats,
        ip,
        timestamp,
        &request,
        started_at.elapsed(),
        route_stats,
    );

    Ok(Json(quotes))
}

async fn outcome_handler(
    Query(request): Query<OutcomeRequest>,
) -> Result<Json<SwapOutcome>, (StatusCode, String)> {
    info!("Received outcome request: {:?}", request);
    outcome::swap_outcome(&Mainnet, &Mainnet, request)
        .await
        .map(Json)
}

fn record_stats(
    stats: &Stats,
    ip: IpAddr,
    timestamp: DateTime<Utc>,
    request: &SwapRequest,
    duration: Duration,
    routes: Vec<RouteStats>,
) {
    stats.record(QueryStats {
        timestamp,
        ip,
        token_in: request.token_in.clone(),
        token_out: request.token_out.clone(),
        amount: request.amount,
        referrer_id: request.referrer_id.clone(),
        trader_account_id: request.trader_account_id.clone(),
        duration,
        routes,
    });
}

fn validate_request(request: &SwapRequest) -> Result<(), (StatusCode, String)> {
    match &request.slippage {
        Slippage::Auto {
            max_slippage,
            min_slippage,
        } => {
            if *max_slippage < 0 || *max_slippage > 1 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Max slippage must be between 0.00 and 1.00".to_string(),
                ));
            }
            if *min_slippage < 0 || *min_slippage > 1 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Min slippage must be between 0.00 and 1.00".to_string(),
                ));
            }
            if max_slippage < min_slippage {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Max slippage must be greater than min slippage".to_string(),
                ));
            }
        }
        Slippage::Fixed { slippage } => {
            if *slippage < 0 || *slippage > 1 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Slippage must be between 0.00 and 1.00".to_string(),
                ));
            }
        }
    }

    if request.token_in == request.token_out {
        return Err((
            StatusCode::BAD_REQUEST,
            "Token in and token out must be different".to_string(),
        ));
    }

    if let Amount::AmountIn(0) | Amount::AmountOut(0) = request.amount {
        return Err((
            StatusCode::BAD_REQUEST,
            "Amount must be greater than 0".to_string(),
        ));
    }

    if request.dexes.as_ref().is_some_and(|dexes| dexes.is_empty()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Dexes must not be an empty array".to_string(),
        ));
    }

    Ok(())
}

struct AbortOnDrop(Vec<AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Quotes of every requested DEX, best first
async fn find_routes(
    request: &SwapRequest,
    request_details: &RequestDetails,
) -> (Vec<Quote>, Vec<RouteStats>) {
    let providers: &[&dyn Provider] = &[
        &providers::rhea::RheaProvider,
        &providers::wrap::WrapProvider,
        &providers::rhea_dcl::RheaDclProvider,
    ];

    // Hooks are in IntearDex, and quoted alone only when they're asked for
    let dexes = request
        .dexes
        .clone()
        .unwrap_or_else(|| vec![DexId::Wrap, DexId::Rhea, DexId::RheaDcl, DexId::IntearDex]);

    let mut routes = Vec::new();
    for provider in providers {
        if !dexes.contains(&provider.dex_id()) {
            continue;
        }

        let request_cloned = request.clone();
        routes.push(tokio::spawn(async move {
            let started_at = Instant::now();
            let (outcome, route) = tokio::select! {
                route = AssertUnwindSafe(provider.route(request_cloned)).catch_unwind() => match route {
                    Ok(Some(route)) => (RouteOutcome::Found, Some(route)),
                    Ok(None) => (RouteOutcome::NotFound, None),
                    Err(_) => (RouteOutcome::Panicked, None),
                },
                _ = tokio::time::sleep(Duration::from_millis(60_000)) => (RouteOutcome::TimedOut, None),
            };
            let route_stats = RouteStats {
                dex_id: provider.dex_id(),
                duration: started_at.elapsed(),
                outcome,
                estimated_amount: route.as_ref().map(|route| match route.estimated_amount {
                    Amount::AmountIn(amount) | Amount::AmountOut(amount) => amount,
                }),
            };
            (route_stats, route)
        }));
    }

    let _abort_routes_on_drop = AbortOnDrop(routes.iter().map(JoinHandle::abort_handle).collect());
    // The nonce is read while the routes are found, so it doesn't add to the wait
    let (routes, signing) = tokio::join!(
        futures_util::future::join_all(routes),
        signing_context(request)
    );
    let (mut route_stats, routes): (Vec<_>, Vec<_>) = routes
        .into_iter()
        .map(|result| result.expect("Route tasks catch panics and are only aborted after this"))
        .unzip();
    let mut quotes = Vec::new();
    for (stats, route) in route_stats.iter_mut().zip(routes) {
        let Some(route) = route else {
            continue;
        };
        match finish_route(request, route).await {
            Some(route) => quotes.push(quote_of_route(route, signing.clone())),
            None => {
                stats.outcome = RouteOutcome::NotFound;
                stats.estimated_amount = None;
            }
        }
    }
    sort_quotes(request, &mut quotes);

    let intear_dex_requested = dexes.contains(&DexId::IntearDex);
    let asked_hooks = hooks::loaded()
        .iter()
        .filter(|hook| intear_dex_requested || dexes.contains(&hook.dex_id()))
        .collect::<Vec<_>>();
    let hook_trade = if asked_hooks.is_empty() {
        None
    } else {
        let block_height = Mainnet
            .current_block_height()
            .inspect_err(|err| tracing::warn!("No block height for hooks: {err}"))
            .ok();
        let trade = hooks::inteardex::trade_of_request(&Mainnet, request).await;
        if let (Some(block_height), Some(trade)) = (block_height, trade) {
            Some((trade, block_height))
        } else {
            None
        }
    };
    let intear_dex_started_at = Instant::now();
    let mut intear_dex_hook_quotes = Vec::new();
    for hook in asked_hooks {
        let started_at = Instant::now();
        let Some((trade, block_height)) = &hook_trade else {
            route_stats.push(RouteStats {
                dex_id: hook.dex_id(),
                duration: started_at.elapsed(),
                outcome: RouteOutcome::NotFound,
                estimated_amount: None,
            });
            continue;
        };
        let (mut outcome, hook_quotes) = hook
            .quotes(request, request_details, trade, *block_height, &quotes)
            .await;
        intear_dex_hook_quotes.extend(hook_quotes.iter().cloned());
        let mut estimated_amount = hook_quotes.first().map(|quote| quote.estimated_amount.0);
        if dexes.contains(&hook.dex_id()) && matches!(outcome, RouteOutcome::Found) {
            let swaps = hook_quotes
                .into_iter()
                .map(IntearDexSwap::of_quote)
                .collect();
            let (routes_outcome, hook_route_quotes) =
                intear_dex_quotes(hook.dex_id(), request, trade, swaps, &signing).await;
            outcome = routes_outcome;
            estimated_amount = hook_route_quotes.first().map(estimated_amount_of);
            quotes.extend(hook_route_quotes);
            sort_quotes(request, &mut quotes);
        }
        route_stats.push(RouteStats {
            dex_id: hook.dex_id(),
            duration: started_at.elapsed(),
            outcome,
            estimated_amount,
        });
    }
    if intear_dex_requested {
        let (outcome, intear_dex_quote) = match &hook_trade {
            Some((trade, _)) => {
                match IntearDexSwap::fallback_queue(trade, intear_dex_hook_quotes) {
                    Some(swap) => {
                        intear_dex_quotes(DexId::IntearDex, request, trade, vec![swap], &signing)
                            .await
                    }
                    None => (RouteOutcome::NotFound, vec![]),
                }
            }
            None => (RouteOutcome::NotFound, vec![]),
        };
        route_stats.push(RouteStats {
            dex_id: DexId::IntearDex,
            duration: intear_dex_started_at.elapsed(),
            outcome,
            estimated_amount: intear_dex_quote.first().map(estimated_amount_of),
        });
        quotes.extend(intear_dex_quote);
        sort_quotes(request, &mut quotes);
    }

    tracing::info!("Found {} routes: {:?}", quotes.len(), quotes);

    (quotes, route_stats)
}

/// The quotes of the routes of the swaps on Intear DEX, best first, and how building them went
async fn intear_dex_quotes(
    dex_id: DexId,
    request: &SwapRequest,
    trade: &IntearDexTrade,
    swaps: Vec<IntearDexSwap>,
    signing: &Option<SigningContext>,
) -> (RouteOutcome, Vec<Quote>) {
    let (mut outcome, routes) = hooks::routes(dex_id, request, trade, swaps).await;
    let mut quotes = Vec::new();
    for route in routes {
        if let Some(route) = finish_route(request, route).await {
            quotes.push(quote_of_route(route, signing.clone()));
        }
    }
    if matches!(outcome, RouteOutcome::Found) && quotes.is_empty() {
        outcome = RouteOutcome::NotFound;
    }
    sort_quotes(request, &mut quotes);
    (outcome, quotes)
}

fn estimated_amount_of(quote: &Quote) -> u128 {
    match quote.route.estimated_amount {
        Amount::AmountIn(amount) | Amount::AmountOut(amount) => amount,
    }
}

/// The route with its output converted to the requested token location, and its fee transactions
/// last. None if there's no conversion for the output.
async fn finish_route(request: &SwapRequest, mut route: Route) -> Option<Route> {
    if !route.deprecated_needs_unwrap_always_false {
        let amount_out = match (
            request.amount,
            route.estimated_amount,
            route.worst_case_amount,
        ) {
            (Amount::AmountIn(_), Amount::AmountOut(amount), Amount::AmountOut(amount2)) => {
                if amount == amount2 {
                    Some(amount)
                } else {
                    None
                }
            }
            (Amount::AmountOut(amount), Amount::AmountIn(_), Amount::AmountIn(_)) => Some(amount),
            _ => unreachable!(),
        };
        if let Some(amount_out) = amount_out {
            info!(
                "Converting from {:?} to {:?} with amount {}",
                route.token_output, request.token_out, amount_out
            );
            let conversion = convert_to(
                &Mainnet,
                &route.token_output,
                &request.token_out,
                amount_out,
                request.trader_account_id.clone(),
            )
            .await;
            let Some(conversion) = conversion else {
                error!(
                    "No conversion of the {} route from {} to {}, leaving it out",
                    route.dex_id, route.token_output, request.token_out
                );
                return None;
            };
            route.estimated_gas_paid = NearGas::from_gas(
                route.estimated_gas_paid.as_gas()
                    + estimated_gas_paid(&conversion, None, request.trader_account_id.as_ref())
                        .as_gas(),
            );
            route.execution_instructions.extend(conversion);
            route.token_output = request.token_out.clone();
        }
    }
    // Fee transactions go last, after the output conversion too, so the swap doesn't wait
    // for fees.
    let (fee_transactions, swap_transactions): (Vec<_>, Vec<_>) = route
        .execution_instructions
        .drain(..)
        .partition(|instruction| {
            matches!(
                instruction,
                ExecutionInstruction::NearTransaction {
                    is_fee_transaction: true,
                    ..
                }
            )
        });
    route.execution_instructions =
        optimize_execution_instructions([swap_transactions, fee_transactions].concat());
    Some(route)
}

fn quote_of_route(route: Route, signing: Option<SigningContext>) -> Quote {
    Quote {
        estimated_max_gas_cost: max_gas_cost(&route.execution_instructions),
        estimated_gas_usage: pathfinder::MIN_GAS_PRICE
            .saturating_mul(route.estimated_gas_paid.as_gas().into()),
        storage_deposits: storage_deposits(&route.execution_instructions),
        fee_amount: fee_transactions_amount(&route.execution_instructions),
        signing,
        route,
    }
}

/// Best first, by what each quote gets net of its costs
fn sort_quotes(request: &SwapRequest, quotes: &mut [Quote]) {
    // Costs are in NEAR, the amounts in the token whose amount is quoted
    let quoted_token = match request.amount {
        Amount::AmountIn(_) => &request.token_out,
        Amount::AmountOut(_) => &request.token_in,
    };
    let prices_raw = price_raw(&Mainnet, &BaseTokenId::Near)
        .zip(price_raw(&Mainnet, &base_token(quoted_token).unwrap()));
    if prices_raw.is_none() {
        info!(
            "No price to weigh the costs of routes against their amounts, comparing amounts only"
        );
    }
    quotes.sort_by_cached_key(|quote| std::cmp::Reverse(net_amount(quote, prices_raw.as_ref())));
}

/// The nonce of the trader's signing key and a recent final block's hash, to sign the routes'
/// transactions with. None without both in the request.
async fn signing_context(request: &SwapRequest) -> Option<SigningContext> {
    let (Some(account_id), Some(public_key)) =
        (&request.trader_account_id, &request.signing_public_key)
    else {
        return None;
    };
    let access_key = ACCESS_KEY_RPC_CLIENT
        .get_access_key(
            account_id.clone(),
            public_key.clone(),
            QueryFinality::Finality(Finality::None),
        )
        .await;
    let access_key = match access_key {
        Ok(access_key) => access_key,
        Err(err) => {
            info!("No nonce of {public_key} for {account_id}: {err:?}");
            return None;
        }
    };
    match final_block_hash() {
        Ok(block_hash) => Some(SigningContext {
            nonce: access_key.nonce,
            block_hash,
        }),
        Err(err) => {
            error!("No block hash to sign with: {err}");
            None
        }
    }
}

/// What a quote gets net of its costs, higher is better: the output minus the costs for exact-in,
/// minus the input and the costs for exact-out. Costs count only with `prices_raw`, the USD prices
/// of a yoctoNEAR and of a raw unit of the quoted token.
fn net_amount(quote: &Quote, prices_raw: Option<&(BigDecimal, BigDecimal)>) -> BigDecimal {
    let cost = prices_raw.map_or_else(
        BigDecimal::zero,
        |(near_price_raw, quoted_token_price_raw)| {
            BigDecimal::from(
                quote
                    .estimated_gas_usage
                    .saturating_add(quote.storage_deposits)
                    .as_yoctonear(),
            ) * near_price_raw
                / quoted_token_price_raw
        },
    );
    match quote.route.estimated_amount {
        Amount::AmountOut(amount_out) => BigDecimal::from(amount_out) - cost,
        Amount::AmountIn(amount_in) => -(BigDecimal::from(amount_in) + cost),
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .pretty()
        .init();

    info!("Starting swap-router HTTP server...");

    hooks::load();
    let rate_limiter = RateLimiter::from_env().await;
    let stats = Stats::from_env().await;

    shared_utils::start_background_refresh().await;
    tokio::spawn(hooks::follow_prices(Mainnet));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/route", get(route_handler))
        .route("/route/subscribe", get(subscription::subscribe_handler))
        .route("/route/outcome", get(outcome_handler))
        .with_state(stats)
        .layer(middleware::from_fn_with_state(
            rate_limiter.clone(),
            rate_limit::limit_unauthorized,
        ))
        .layer(cors)
        .layer(middleware::from_fn_with_state(
            rate_limiter,
            rate_limit::validate_source,
        ));

    let bind_address = env::var("BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:3000".to_string());

    let listener = tokio::net::TcpListener::bind(&bind_address).await.unwrap();

    info!("Server running on http://{}", bind_address);

    // dexes need the indexer to run, and indexer without http does nothing, so stop when first one crashes
    tokio::select! {
        result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        ) => result.unwrap(),
        error = pathfinder::run(Arc::new(shared_utils::RPC_CLIENT.clone()), hooks::indexed()) => {
            error!("Pool indexing stopped: {error:#}");
            std::process::exit(1);
        }
    }
}
