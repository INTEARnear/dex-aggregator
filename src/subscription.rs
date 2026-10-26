use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    http::{HeaderMap, Uri},
    response::Response,
    Extension,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};
use tracing::info;

use swap_router_types::hook::RequestDetails;

use crate::{
    find_routes, hooks,
    rate_limit::{ClientIp, RequestLimit},
    record_stats,
    stats::Stats,
    types::SwapRequest,
    validate_request,
};

/// If routing took 30ms, wait at least REROUTE_COOLDOWN_FACTOR * 30ms (rounded up to nearest block) before rerouting
const REROUTE_COOLDOWN_FACTOR: u32 = 10;
const MAX_REROUTE_COOLDOWN: Duration = Duration::from_secs(3);

/// A message from the client. It replaces the previous route request, or
/// only stops it if there's no `query`.
#[derive(Deserialize)]
struct RouteRequestMessage {
    /// Sent back with every update of this request, so updates of the
    /// previous requests can be told apart
    id: u64,
    /// Same query parameter string as `/route` takes
    query: Option<String>,
}

pub async fn subscribe_handler(
    State(stats): State<Stats>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(limit): Extension<RequestLimit>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let request_details = hooks::request_details(ip, &uri, &headers);
    upgrade.on_upgrade(move |socket| subscribe(socket, stats, request_details, limit))
}

async fn subscribe(
    mut socket: WebSocket,
    stats: Stats,
    request_details: RequestDetails,
    limit: RequestLimit,
) {
    let (updates_sender, mut updates) = mpsc::unbounded_channel::<String>();
    let mut routing: Option<JoinHandle<()>> = None;
    loop {
        tokio::select! {
            message = socket.recv() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                };
                if let Some(previous) = routing.take() {
                    previous.abort();
                }
                let Ok(RouteRequestMessage { id, query }) = serde_json::from_str(&text) else {
                    break;
                };
                let Some(query) = query else {
                    continue;
                };
                match parse_route_request(&query, &limit) {
                    Ok(request) => {
                        info!("Received route subscription request: {:?}", request);
                        routing = Some(tokio::spawn(route_updates(
                            id,
                            request,
                            stats.clone(),
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

/// Sends the routes as soon as they're found, then again whenever newer pools change them
async fn route_updates(
    id: u64,
    request: SwapRequest,
    stats: Stats,
    request_details: RequestDetails,
    updates: mpsc::UnboundedSender<String>,
) {
    let mut pools_updated = pathfinder::pools_updated();
    let mut sent_quotes = None;
    loop {
        // Routes found below use pools at least this new, so only newer ones reroute
        pools_updated.borrow_and_update();
        let started_at = Instant::now();
        let timestamp = Utc::now();
        let (quotes, route_stats) = find_routes(&request, &request_details).await;
        let duration = started_at.elapsed();

        if sent_quotes.is_none() {
            record_stats(
                &stats,
                request_details.ip,
                timestamp,
                &request,
                duration,
                route_stats,
            );
        }
        if sent_quotes.as_ref() != Some(&quotes) {
            let update = json!({ "id": id, "routes": &quotes }).to_string();
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
