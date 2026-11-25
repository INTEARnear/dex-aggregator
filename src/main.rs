#![deny(clippy::float_arithmetic)]

use axum::{
    extract::{Json, Query},
    http::{HeaderMap, StatusCode, Uri},
    middleware,
    routing::get,
    Extension, Router,
};
use bigdecimal::BigDecimal;
use futures_util::FutureExt;
use near_min_api::{
    types::{BlockHeight, Finality, NearToken},
    QueryFinality, RpcClient,
};
use std::{
    cmp::Reverse,
    collections::HashMap,
    env,
    future::Future,
    net::SocketAddr,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tokio::task::{AbortHandle, JoinHandle};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use swap_router_types::{
    fee_discount::FeeDiscounter,
    hook::{IntearDexRouteCosts, IntearDexTrade, RequestDetails},
    last_look::LastLook,
    types,
};

use crate::{
    hooks::{
        inteardex::IntearDexSwap,
        pool_routes::{self, PoolRoutes, PoolRoutesRequest},
    },
    rate_limit::{ClientIp, RateLimiter},
    route::{resolve, ResolvedRoute, RouteBlueprint, RouteGas},
    shared_utils::{
        base_token, convert_to, final_block_hash, price_raw, BaseTokenId, Mainnet, NetworkView,
        RPC_CLIENT,
    },
    types::{
        Amount, CostPrices, DexId, OutcomeRequest, Quote, Route, SigningContext, Slippage,
        SwapOutcome, SwapRequest, TokenId,
    },
};

static ACCESS_KEY_RPC_CLIENT: LazyLock<RpcClient> =
    LazyLock::new(|| RPC_CLIENT.clone().with_max_retries(3));

mod calls;
mod hooks;
mod outcome;
mod providers;
mod rate_limit;
mod route;
mod shared_utils;
mod simulated_gas;
mod simulated_holdings;
mod subscription;
mod tear_stakes;
mod token_infos;
mod trader_fee_discount;

pub trait Provider: Sync {
    fn route(
        &self,
        request: SwapRequest,
    ) -> Pin<Box<dyn Future<Output = Option<RouteBlueprint>> + Send>>;

    fn dex_id(&self) -> DexId;
}

async fn route_handler(
    Extension(ClientIp(ip)): Extension<ClientIp>,
    uri: Uri,
    headers: HeaderMap,
    Query(request): Query<SwapRequest>,
) -> Result<Json<Vec<Quote>>, (StatusCode, String)> {
    info!("Received route request: {:?}", request);

    validate_request(&request)?;
    let (quotes, _) = find_routes(&request, &hooks::request_details(ip, &uri, &headers)).await;

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

fn validate_request(request: &SwapRequest) -> Result<(), (StatusCode, String)> {
    const MAX_SLIPPAGE_DECIMALS: i64 = 18;

    let slippages = match &request.slippage {
        Slippage::Auto {
            max_slippage,
            min_slippage,
        } => vec![max_slippage, min_slippage],
        Slippage::Fixed { slippage } => vec![slippage],
    };
    if slippages
        .iter()
        .any(|slippage| slippage.fractional_digit_count() > MAX_SLIPPAGE_DECIMALS)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Slippage must have at most {MAX_SLIPPAGE_DECIMALS} decimals"),
        ));
    }

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

    let unknown_hook = request.dexes.iter().flatten().find(|dex_id| {
        matches!(dex_id, DexId::Hook(_))
            && !hooks::loaded().iter().any(|hook| hook.dex_id() == **dex_id)
    });
    if let Some(dex_id) = unknown_hook {
        return Err((StatusCode::BAD_REQUEST, format!("Invalid dex id: {dex_id}")));
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

/// Quotes of every requested DEX, best first, and how long routing on each DEX that quotes took
async fn find_routes(
    request: &SwapRequest,
    request_details: &RequestDetails,
) -> (Vec<Quote>, HashMap<DexId, Duration>) {
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

    let intear_dex_requested = dexes.contains(&DexId::IntearDex);
    let asked_hooks = hooks::loaded()
        .iter()
        .filter(|hook| intear_dex_requested || dexes.contains(&hook.dex_id()))
        .collect::<Vec<_>>();
    let hook_trade_and_block_height = if asked_hooks.is_empty() {
        None
    } else {
        let block_height = Mainnet
            .current_block_height()
            .inspect_err(|err| warn!("No block height for hooks: {err}"))
            .ok();
        let trade = hooks::inteardex::trade_of_request(&Mainnet, request);
        trade.zip(block_height)
    };

    let mut routes = Vec::new();
    for provider in providers {
        if !dexes.contains(&provider.dex_id()) {
            continue;
        }

        let request_cloned = request.clone();
        routes.push(tokio::spawn(async move {
            let started_at = Instant::now();
            let route = tokio::select! {
                route = AssertUnwindSafe(provider.route(request_cloned)).catch_unwind() => route.ok().flatten(),
                _ = tokio::time::sleep(Duration::from_millis(60_000)) => {
                    warn!("{} took over 60s to route, leaving it out", provider.dex_id());
                    None
                }
            };
            ((provider.dex_id(), started_at.elapsed()), route)
        }));
    }

    let _abort_routes_on_drop = AbortOnDrop(routes.iter().map(JoinHandle::abort_handle).collect());
    // The nonce and the costs of hooks' routes are found out while the routes are found, so they
    // don't add to the wait
    let (routes, signing, hook_route_costs) = tokio::join!(
        futures_util::future::join_all(routes),
        signing_context(request),
        async {
            let (trade, _) = hook_trade_and_block_height.as_ref()?;
            intear_dex_route_costs(request, trade).await
        }
    );
    // Hooks' quotes have no route without the route their costs are of
    let hook_trade = match (hook_trade_and_block_height, hook_route_costs) {
        (Some((trade, block_height)), Some(route_costs)) => {
            Some((trade, block_height, route_costs))
        }
        (Some((trade, _)), None) => {
            warn!("No costs of Intear DEX's routes, leaving out the quotes of hooks for {trade:?}");
            None
        }
        (None, _) => None,
    };
    let (mut dex_durations, routes): (HashMap<_, _>, Vec<_>) = routes
        .into_iter()
        .map(|result| result.expect("Route tasks catch panics and are only aborted after this"))
        .unzip();
    let resolved_routes = futures_util::future::join_all(routes.into_iter().map(|route| async {
        match route {
            Some(route) => Some(resolve_route(request, route).await),
            None => None,
        }
    }))
    .await;
    let mut quotes = futures_util::future::join_all(
        resolved_routes
            .into_iter()
            .flatten()
            .flatten()
            .map(|route| quote_of_route(request, route, signing.clone())),
    )
    .await
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    sort_quotes(request, &mut quotes);

    // Pools of hooks are routed through together, and through one hook's alone when it's asked
    // for. Hooks that quote do it for both.
    let routing_pools = tokio::task::spawn_blocking(hooks::routing_pools)
        .await
        .expect("Getting the pools of hooks doesn't panic");
    let intear_dex_started_at = Instant::now();
    let mut intear_dex_hook_quotes = Vec::new();
    for hook in asked_hooks {
        let started_at = Instant::now();
        let asked_alone = dexes.contains(&hook.dex_id());
        let hook_pools = routing_pools.clone().and_then(|pools| {
            let snapshot = pools.snapshot_of(hook)?;
            Some((pools, snapshot))
        });
        if hook_pools.is_some() && !asked_alone {
            continue;
        }
        let Some((trade, block_height, route_costs)) = &hook_trade else {
            continue;
        };
        let hook_quotes = hook
            .quotes(
                request,
                request_details,
                trade,
                *block_height,
                &quotes,
                route_costs,
            )
            .await;
        intear_dex_hook_quotes.extend(hook_quotes.iter().cloned());
        if asked_alone {
            let mut swaps = hook_quotes
                .into_iter()
                .map(|quote| IntearDexSwap::of_quote(trade, quote))
                .collect::<Vec<_>>();
            if let Some((pools, snapshot)) = hook_pools {
                let pool_routes = pool_routes(
                    pools,
                    Some(snapshot),
                    request,
                    trade,
                    *block_height,
                    route_costs,
                    None,
                )
                .await;
                swaps.extend(
                    pool_routes
                        .quotes
                        .into_iter()
                        .map(|quote| IntearDexSwap::of_quote(trade, quote))
                        .chain(pool_routes.chains),
                );
            }
            if !swaps.is_empty() {
                let hook_quote =
                    best_intear_dex_quote(hook.dex_id(), request, trade, swaps, &signing).await;
                quotes.extend(hook_quote.map(|hook_quote| hook_quote.quote));
                sort_quotes(request, &mut quotes);
            }
            dex_durations.insert(hook.dex_id(), started_at.elapsed());
        }
    }
    if intear_dex_requested {
        let intear_dex_quote = match &hook_trade {
            Some((trade, block_height, route_costs)) => {
                let other_quotes = &quotes;
                let signing = &signing;
                let quote_with_costs = |route_costs: IntearDexRouteCosts| {
                    let routing_pools = routing_pools.clone();
                    let mut hook_quotes = intear_dex_hook_quotes.clone();
                    async move {
                        let mut swaps = Vec::new();
                        if let Some(routing_pools) = routing_pools {
                            let last_look = LastLook::of(other_quotes, &request.fees, &route_costs);
                            let found = pool_routes(
                                routing_pools,
                                None,
                                request,
                                trade,
                                *block_height,
                                &route_costs,
                                last_look,
                            )
                            .await;
                            hook_quotes.extend(found.quotes);
                            swaps.extend(found.chains);
                        }
                        swaps.extend(IntearDexSwap::fallback_queue(trade, hook_quotes));
                        if swaps.is_empty() {
                            None
                        } else {
                            best_intear_dex_quote(DexId::IntearDex, request, trade, swaps, signing)
                                .await
                        }
                    }
                };
                let first_look = quote_with_costs(route_costs.clone()).await;
                let underestimate = first_look.as_ref().and_then(|intear_dex_quote| {
                    let last_look = LastLook::of(other_quotes, &request.fees, route_costs)?;
                    last_look_underestimate(intear_dex_quote, &last_look)
                });
                match underestimate {
                    Some(underestimate) => {
                        info!(
                            "Intear DEX's route burns {} yoctoNEAR more than the last look \
                             weighed it with, looking again",
                            underestimate.as_yoctonear()
                        );
                        let second_look = quote_with_costs(IntearDexRouteCosts {
                            without_oracle_request: route_costs
                                .without_oracle_request
                                .saturating_add(underestimate),
                            with_oracle_request: route_costs
                                .with_oracle_request
                                .saturating_add(underestimate),
                            ..route_costs.clone()
                        })
                        .await;
                        // A second look that can't match gives no discount, less than the first
                        let prices = cost_prices(request);
                        [first_look, second_look].into_iter().flatten().min_by_key(
                            |intear_dex_quote| quote_rank(&intear_dex_quote.quote, prices.as_ref()),
                        )
                    }
                    None => first_look,
                }
            }
            None => None,
        };
        dex_durations.insert(DexId::IntearDex, intear_dex_started_at.elapsed());
        quotes.extend(intear_dex_quote.map(|intear_dex_quote| intear_dex_quote.quote));
        sort_quotes(request, &mut quotes);
    }

    tracing::info!("Found {} routes: {:?}", quotes.len(), quotes);

    (quotes, dex_durations)
}

/// Routes through the pools of the hook of `snapshot` in `pools`, or of every hook. Without prices
/// to weigh what hops cost, there are none.
async fn pool_routes(
    pools: Arc<hooks::RoutingPools>,
    snapshot: Option<usize>,
    request: &SwapRequest,
    trade: &IntearDexTrade,
    block_height: BlockHeight,
    route_costs: &IntearDexRouteCosts,
    last_look: Option<LastLook>,
) -> PoolRoutes {
    let Some(prices) = route_costs.prices.clone() else {
        info!("No prices to weigh what hops cost, leaving routes through pools out of {trade:?}");
        return PoolRoutes::default();
    };
    let tear_fee_discount = trade
        .trader_account_id
        .as_ref()
        .and_then(tear_stakes::fee_discount_of);
    pool_routes::find_in_background(
        pools,
        snapshot,
        PoolRoutesRequest {
            request: request.clone(),
            trade: trade.clone(),
            block_height,
            prices,
            asset_in_prices: cost_prices_of(&request.token_in),
            last_look,
            tear_fee_discount,
        },
    )
    .await
}

/// A quote of a route on Intear DEX, with the gas fee that its route was planned to burn, which
/// the last look weighs routes with
struct IntearDexQuote {
    quote: Quote,
    planned_gas_usage: NearToken,
}

/// The one quote of the DEX: of the best of the routes of the swaps on Intear DEX
async fn best_intear_dex_quote(
    dex_id: DexId,
    request: &SwapRequest,
    trade: &IntearDexTrade,
    swaps: Vec<IntearDexSwap>,
    signing: &Option<SigningContext>,
) -> Option<IntearDexQuote> {
    let routes = hooks::routes(dex_id, request, trade, swaps);
    let resolved_routes = futures_util::future::join_all(
        routes
            .into_iter()
            .map(|route| resolve_route(request, route)),
    )
    .await;
    let quotes =
        futures_util::future::join_all(resolved_routes.into_iter().flatten().map(|route| async {
            let planned_gas_usage = pathfinder::MIN_GAS_PRICE.saturating_mul(
                RouteGas::planned(&route)
                    .total()
                    .estimated_burn
                    .as_gas()
                    .into(),
            );
            let quote = quote_of_route(request, route, signing.clone()).await?;
            Some(IntearDexQuote {
                quote,
                planned_gas_usage,
            })
        }))
        .await;
    let prices = cost_prices(request);
    quotes
        .into_iter()
        .flatten()
        .min_by_key(|intear_dex_quote| quote_rank(&intear_dex_quote.quote, prices.as_ref()))
}

/// What the route of Intear DEX's quote burns over what the last look weighed it with, when the
/// quote matches the best other quote by its planned gas but not by what its simulation measured.
/// The last look matches with the smallest discount that it needs by the planned gas, so the
/// route falls just under the best other quote when it burns more. None when the quote matches
/// as measured, or doesn't even as planned.
fn last_look_underestimate(
    intear_dex_quote: &IntearDexQuote,
    last_look: &LastLook,
) -> Option<NearToken> {
    let IntearDexQuote {
        quote,
        planned_gas_usage,
    } = intear_dex_quote;
    let prices = last_look.route_costs.prices.as_ref();
    let best_other_net_amount = types::net_amount(
        last_look.best_other_amount,
        last_look.best_other_cost,
        prices,
    );
    let matches_with_gas_usage = |gas_usage: NearToken| {
        types::net_amount(
            quote.route.estimated_amount,
            gas_usage.saturating_add(quote.storage_deposits),
            prices,
        ) >= best_other_net_amount
    };
    (matches_with_gas_usage(*planned_gas_usage)
        && !matches_with_gas_usage(quote.estimated_gas_usage))
    .then(|| quote.estimated_gas_usage.saturating_sub(*planned_gas_usage))
}

/// The route with its output converted to the requested token location, and what its steps need
/// found out. None if that can't be found out.
/// What the route of a hook's quote costs besides the gas its swap burns, as its quote has it.
/// None if there's no such route.
async fn intear_dex_route_costs(
    request: &SwapRequest,
    trade: &IntearDexTrade,
) -> Option<IntearDexRouteCosts> {
    let cost_of_route = |asks_oracle| async move {
        let swap = IntearDexSwap::burning_no_gas(trade, asks_oracle);
        let route = hooks::inteardex::route(DexId::IntearDex, request, trade, swap)?;
        let route = resolve_route(request, route).await?;
        let gas = RouteGas::planned(&route);
        let quote = quote_with_gas(route, gas, None);
        Some(
            quote
                .estimated_gas_usage
                .saturating_add(quote.storage_deposits),
        )
    };
    let (without_oracle_request, with_oracle_request) =
        tokio::join!(cost_of_route(false), cost_of_route(true));
    Some(IntearDexRouteCosts {
        without_oracle_request: without_oracle_request?,
        with_oracle_request: with_oracle_request?,
        gas_price: pathfinder::MIN_GAS_PRICE,
        prices: cost_prices(request),
    })
}

async fn resolve_route(request: &SwapRequest, mut route: RouteBlueprint) -> Option<ResolvedRoute> {
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
        route.steps.extend(convert_to(
            &route.token_output,
            &request.token_out,
            amount_out,
            request.trader_account_id.as_ref(),
        ));
        route.token_output = request.token_out.clone();
    }
    let dex_id = route.dex_id.clone();
    let resolved_route = resolve(&Mainnet, route).await;
    if resolved_route.is_none() {
        info!("What the steps of the {dex_id} route need couldn't be found out, leaving it out");
    }
    resolved_route
}

/// With what its calls burn, simulated as the trader. None when that can't be found out in time.
async fn quote_of_route(
    request: &SwapRequest,
    route: ResolvedRoute,
    signing: Option<SigningContext>,
) -> Option<Quote> {
    let gas = simulated_gas::route_gas(
        &route,
        request.trader_account_id.as_ref(),
        &request.token_in,
    )
    .await?;
    Some(quote_with_gas(route, gas, signing))
}

fn quote_with_gas(
    route: ResolvedRoute,
    route_gas: RouteGas,
    signing: Option<SigningContext>,
) -> Quote {
    let gas = route_gas.total();
    Quote {
        estimated_max_gas_cost: pathfinder::MIN_GAS_PRICE
            .saturating_mul(gas.needs_attached.as_gas().into()),
        estimated_gas_usage: pathfinder::MIN_GAS_PRICE
            .saturating_mul(gas.estimated_burn.as_gas().into()),
        storage_deposits: route.storage_deposits(),
        fee_amount: route.fee_amount(),
        signing,
        quote_id: None,
        route: Route {
            estimated_amount: route.estimated_amount,
            worst_case_amount: route.worst_case_amount,
            dex_id: route.dex_id.clone(),
            token_output: route.token_output.clone(),
            discount: route.discount,
            deprecated_needs_unwrap_always_false: false,
            execution_instructions: route.into_execution_instructions(route_gas),
        },
    }
}

/// Best first, by what each quote gets net of its costs
fn sort_quotes(request: &SwapRequest, quotes: &mut [Quote]) {
    let prices = cost_prices(request);
    if prices.is_none() {
        info!(
            "No price to weigh the costs of routes against their amounts, comparing amounts only"
        );
    }
    quotes.sort_by_cached_key(|quote| quote_rank(quote, prices.as_ref()));
}

/// Lower is better: what the quote gets net of its costs, and of quotes that get the same, Intear
/// DEX's first, as its last look matches the best other quote
fn quote_rank(quote: &Quote, prices: Option<&CostPrices>) -> (Reverse<BigDecimal>, bool) {
    let net_amount = types::net_amount(
        quote.route.estimated_amount,
        quote
            .estimated_gas_usage
            .saturating_add(quote.storage_deposits),
        prices,
    );
    let of_another_dex = !matches!(quote.route.dex_id, DexId::IntearDex | DexId::Hook(_));
    (Reverse(net_amount), of_another_dex)
}

/// Costs are in NEAR, the amounts in the token whose amount is quoted
fn cost_prices(request: &SwapRequest) -> Option<CostPrices> {
    cost_prices_of(match request.amount {
        Amount::AmountIn(_) => &request.token_out,
        Amount::AmountOut(_) => &request.token_in,
    })
}

/// Costs are in NEAR, the amounts in `quoted_token`
fn cost_prices_of(quoted_token: &TokenId) -> Option<CostPrices> {
    let (near_price_raw, quoted_token_price_raw) = price_raw(&Mainnet, &BaseTokenId::Near)
        .zip(price_raw(&Mainnet, &base_token(quoted_token)))?;
    Some(CostPrices {
        near_price_raw,
        quoted_token_price_raw,
    })
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
#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .pretty()
        .init();

    info!("Starting swap-router HTTP server...");

    // Dependencies enable both of rustls's crypto providers, so it can't pick one by itself
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("No crypto provider is installed yet");

    hooks::load();
    tear_stakes::start_fetching();
    FeeDiscounter::from_env()
        .unwrap_or_else(|err| panic!("No fee discount signer: {err}"))
        .start()
        .unwrap_or_else(|_| panic!("The fee discounter is started once"));
    let rate_limiter = RateLimiter::from_env().await;
    let database = subscription::open_database().await;

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
        .route(
            "/fee-discount",
            get(trader_fee_discount::fee_discount_handler),
        )
        .layer(Extension(database))
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
    let serve = async {
        // A quote before then would leave out of its routes the pools that aren't in yet
        info!("Waiting for the pools of every DEX and hook, and for every hook to take prices");
        let mut pools_updated = pathfinder::pools_updated();
        tokio::join!(
            async {
                pools_updated
                    .wait_for(|height| *height > 0)
                    .await
                    .expect("The sender is static");
            },
            hooks::prices_taken(),
        );
        let listener = tokio::net::TcpListener::bind(&bind_address).await.unwrap();
        info!("Server running on http://{}", bind_address);
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    };

    // dexes need the indexer to run, and indexer without http does nothing, so stop when first one crashes
    tokio::select! {
        result = serve => result.unwrap(),
        error = pathfinder::run(Arc::new(shared_utils::RPC_CLIENT.clone()), hooks::indexed()) => {
            error!("Pool indexing stopped: {error:#}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(dex_id: DexId, amount_out: u128, gas_usage: u128) -> Quote {
        Quote {
            route: Route {
                estimated_amount: Amount::AmountOut(amount_out),
                worst_case_amount: Amount::AmountOut(amount_out),
                dex_id,
                execution_instructions: vec![],
                deprecated_needs_unwrap_always_false: false,
                token_output: TokenId::Near,
                discount: None,
            },
            estimated_max_gas_cost: NearToken::from_yoctonear(gas_usage),
            estimated_gas_usage: NearToken::from_yoctonear(gas_usage),
            storage_deposits: NearToken::from_yoctonear(0),
            fee_amount: 0,
            signing: None,
            quote_id: None,
        }
    }

    /// A yoctoNEAR of costs is a unit of the amounts
    fn prices() -> CostPrices {
        CostPrices {
            near_price_raw: 1.into(),
            quoted_token_price_raw: 1.into(),
        }
    }

    #[test]
    fn intear_dex_ranks_first_of_quotes_that_get_the_same() {
        let ranked = |quotes: [Quote; 2]| {
            let mut quotes = quotes;
            quotes.sort_by_cached_key(|quote| quote_rank(quote, Some(&prices())));
            quotes.map(|quote| quote.route.dex_id)
        };
        assert_eq!(
            ranked([
                quote(DexId::Rhea, 1_000, 10),
                quote(DexId::IntearDex, 1_005, 15)
            ]),
            [DexId::IntearDex, DexId::Rhea]
        );
        assert_eq!(
            ranked([
                quote(DexId::IntearDex, 1_004, 15),
                quote(DexId::Rhea, 1_000, 10)
            ]),
            [DexId::Rhea, DexId::IntearDex]
        );
    }

    #[test]
    fn the_last_look_looks_again_for_what_the_route_burns_over_its_plan() {
        let last_look = LastLook {
            best_other_amount: Amount::AmountOut(1_000),
            best_other_cost: NearToken::from_yoctonear(10),
            referral_fees: Default::default(),
            route_costs: IntearDexRouteCosts {
                without_oracle_request: NearToken::from_yoctonear(5),
                with_oracle_request: NearToken::from_yoctonear(8),
                gas_price: NearToken::from_yoctonear(1),
                prices: Some(prices()),
            },
        };
        let underestimate = |amount_out, gas_usage, planned_gas_usage| {
            let intear_dex_quote = IntearDexQuote {
                quote: quote(DexId::IntearDex, amount_out, gas_usage),
                planned_gas_usage: NearToken::from_yoctonear(planned_gas_usage),
            };
            last_look_underestimate(&intear_dex_quote, &last_look).map(|cost| cost.as_yoctonear())
        };
        // Matched by its plan of 10, it gets 989 net of the 13 it burns, against 990
        assert_eq!(underestimate(1_002, 13, 10), Some(3));
        assert_eq!(underestimate(1_001, 12, 10), Some(2));
        assert_eq!(underestimate(1_003, 13, 10), None);
        // Under the best other quote even by its plan, which looking again wouldn't change
        assert_eq!(underestimate(995, 13, 10), None);
        // Over its plan, but still at least as good
        assert_eq!(underestimate(1_010, 13, 10), None);
    }
}
