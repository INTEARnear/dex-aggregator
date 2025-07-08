use axum::{
    extract::{Json, Query},
    http::StatusCode,
    routing::get,
    Router,
};
use std::{env, future::Future, pin::Pin, time::Duration};
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::{
    shared_utils::{convert_to, optimize_execution_instructions},
    types::{Amount, DexId, Route, Slippage, SwapRequest},
};

mod providers;
mod shared_utils;
mod types;

pub trait Provider: Sync {
    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>>;

    fn dex_id(&self) -> DexId;
}

async fn route_handler(
    Query(request): Query<SwapRequest>,
) -> Result<Json<Vec<Route>>, (StatusCode, String)> {
    info!("Received route request: {:?}", request);

    match request.slippage {
        Slippage::Auto {
            max_slippage,
            min_slippage,
        } => {
            if !(0.00..=1.0).contains(&max_slippage) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Max slippage must be between 0.00 and 1.00".to_string(),
                ));
            }
            if !(0.00..=1.0).contains(&min_slippage) {
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
            if !(0.00..=1.0).contains(&slippage) {
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

    let providers: &[&dyn Provider] = &[
        &providers::rhea::RheaProvider,
        &providers::aidols::AidolsProvider,
        &providers::grafun::GraFunProvider,
        &providers::near_intents::NearIntentsProvider,
        &providers::wrap::WrapProvider,
        &providers::rhea_dcl::RheaDclProvider,
        &providers::veax::VeaxProvider,
        &providers::metapool::MetapoolProvider,
        &providers::linear::LinearProvider,
    ];

    let mut routes = Vec::new();
    for provider in providers {
        if let Some(dexes) = request.dexes.as_ref() {
            if !dexes.contains(&provider.dex_id()) {
                continue;
            }
        }

        let request_cloned = request.clone();
        routes.push(tokio::spawn(async move {
            tokio::select! {
                route = provider.route(request_cloned) => route,
                _ = tokio::time::sleep(Duration::from_millis(request.max_wait_ms)) => None,
            }
        }));
    }

    let routes = futures_util::future::join_all(routes).await;
    let mut routes = routes.into_iter().flatten().flatten().collect::<Vec<_>>();

    routes.sort_by_key(|route| {
        match route.estimated_amount {
            // If 2 or more dexes return amount more than i128::MAX, don't care about these
            // stupidly large token amounts, usually normal tokens don't go so close to
            // limits of u128.
            Amount::AmountIn(amount) => amount.try_into().unwrap_or(i128::MAX),
            Amount::AmountOut(amount) => -(amount.try_into().unwrap_or(i128::MAX)),
        }
    });

    for route in routes.iter_mut() {
        if !route.has_leftover_after_slippage_that_needs_unwrapping {
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
                        &route.token_output,
                        request.token_out.location(),
                        amount_out,
                        request.trader_account_id.clone(),
                    )
                    .await,
                );
            }
        }
        route.execution_instructions =
            optimize_execution_instructions(route.execution_instructions.clone());
    }

    tracing::info!("Found {} routes: {:?}", routes.len(), routes);

    Ok(Json(routes))
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .pretty()
        .init();

    info!("Starting swap-router HTTP server...");

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/route", get(route_handler))
        .layer(cors);

    let bind_address = env::var("BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:3000".to_string());

    let listener = tokio::net::TcpListener::bind(&bind_address).await.unwrap();

    info!("Server running on http://{}", bind_address);

    axum::serve(listener, app).await.unwrap();
}
