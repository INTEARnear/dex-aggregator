#![deny(clippy::float_arithmetic)]

use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    middleware,
    routing::get,
    Extension, Router,
};
use bigdecimal::{BigDecimal, Zero};
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use std::{
    env,
    future::Future,
    net::{IpAddr, SocketAddr},
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::{
    rate_limit::{ClientIp, RateLimiter},
    shared_utils::{
        convert_to, estimated_gas_usage, max_gas_cost, optimize_execution_instructions, price_raw,
        storage_deposits_max_at_any_point, storage_deposits_net, Mainnet,
    },
    stats::{QueryStats, RouteOutcome, RouteStats, Stats},
    types::{Amount, DexId, Quote, Route, Slippage, SwapRequest, TokenId},
};

mod providers;
mod rate_limit;
mod shared_utils;
mod stats;
mod subscription;
mod types;

pub trait Provider: Sync {
    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>>;

    fn dex_id(&self) -> DexId;
}

async fn route_handler(
    State(stats): State<Stats>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Query(request): Query<SwapRequest>,
) -> Result<Json<Vec<Quote>>, (StatusCode, String)> {
    let started_at = Instant::now();
    let timestamp = Utc::now();
    info!("Received route request: {:?}", request);

    validate_request(&request)?;
    let (quotes, route_stats) = find_routes(&request).await;
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

    if request.max_wait_ms > 60_000 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Max wait must be less than 60 seconds".to_string(),
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

/// Quotes of every requested DEX, best first
async fn find_routes(request: &SwapRequest) -> (Vec<Quote>, Vec<RouteStats>) {
    let providers: &[&dyn Provider] = &[
        &providers::rhea::RheaProvider,
        &providers::aidols::AidolsProvider,
        &providers::wrap::WrapProvider,
        &providers::rhea_dcl::RheaDclProvider,
        &providers::metapool::MetapoolProvider,
        &providers::linear::LinearProvider,
        &providers::xrhea::XRheaProvider,
        &providers::rnear::RNearProvider,
        &providers::intear_plach::IntearPlachProvider,
    ];

    let dexes = request.dexes.clone().unwrap_or(vec![
        DexId::Rhea,
        DexId::Aidols,
        DexId::Wrap,
        DexId::RheaDcl,
        DexId::MetaPool,
        DexId::Linear,
        DexId::XRhea,
        DexId::RNear,
        DexId::Plach,
    ]);

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

    let (route_stats, routes): (Vec<_>, Vec<_>) = futures_util::future::join_all(routes)
        .await
        .into_iter()
        .map(|result| result.expect("Route tasks catch panics and are never aborted"))
        .unzip();
    let mut routes = routes.into_iter().flatten().collect::<Vec<_>>();

    for route in routes.iter_mut() {
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
                (Amount::AmountOut(amount), Amount::AmountIn(_), Amount::AmountIn(_)) => {
                    Some(amount)
                }
                _ => unreachable!(),
            };
            if let Some(amount_out) = amount_out {
                info!(
                    "Converting from {:?} to {:?} with amount {}",
                    route.token_output, request.token_out, amount_out
                );
                route.execution_instructions.extend(
                    convert_to(
                        &Mainnet,
                        &route.token_output,
                        &request.token_out,
                        amount_out,
                        request.trader_account_id.clone(),
                    )
                    .await,
                );
                route.token_output = request.token_out.clone();
            }
        }
        route.execution_instructions =
            optimize_execution_instructions(route.execution_instructions.clone());
    }

    let mut quotes = routes
        .into_iter()
        .map(|route| Quote {
            estimated_max_gas_cost: max_gas_cost(&route.execution_instructions),
            estimated_gas_usage: estimated_gas_usage(&route.execution_instructions),
            storage_deposits: storage_deposits_net(&route.execution_instructions),
            max_storage_deposits: storage_deposits_max_at_any_point(&route.execution_instructions),
            route,
        })
        .collect::<Vec<_>>();
    // Costs are in NEAR, the amounts in the token whose amount is quoted
    let quoted_token = match request.amount {
        Amount::AmountIn(_) => &request.token_out,
        Amount::AmountOut(_) => &request.token_in,
    };
    let prices_raw = price_raw(&Mainnet, &TokenId::Near).zip(price_raw(&Mainnet, quoted_token));
    if prices_raw.is_none() {
        info!(
            "No price to weigh the costs of routes against their amounts, comparing amounts only"
        );
    }
    quotes.sort_by_cached_key(|quote| std::cmp::Reverse(net_amount(quote, prices_raw.as_ref())));

    tracing::info!("Found {} routes: {:?}", quotes.len(), quotes);

    (quotes, route_stats)
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
                    .estimated_max_gas_cost
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

    let rate_limiter = RateLimiter::from_env().await;
    let stats = Stats::from_env().await;

    shared_utils::start_background_refresh().await;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/route", get(route_handler))
        .route("/route/subscribe", get(subscription::subscribe_handler))
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
        error = pathfinder::run(Arc::new(shared_utils::RPC_CLIENT.clone())) => {
            error!("Pool indexing stopped: {error:#}");
            std::process::exit(1);
        }
    }
}
