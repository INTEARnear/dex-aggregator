use std::{net::IpAddr, time::Duration};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::Response,
    Extension,
};
use chrono::Utc;
use tokio::time::Instant;
use tracing::info;

use crate::{
    find_routes, rate_limit::ClientIp, record_stats, stats::Stats, types::SwapRequest,
    validate_request,
};

/// If routing took 30ms, wait at least REROUTE_COOLDOWN_FACTOR * 30ms (rounded up to nearest block) before rerouting
const REROUTE_COOLDOWN_FACTOR: u32 = 10;
const MAX_REROUTE_COOLDOWN: Duration = Duration::from_secs(3);

pub async fn subscribe_handler(
    State(stats): State<Stats>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Query(request): Query<SwapRequest>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, (StatusCode, String)> {
    info!("Received route subscription: {:?}", request);
    validate_request(&request)?;
    Ok(upgrade.on_upgrade(move |socket| subscribe(socket, request, stats, ip)))
}

async fn subscribe(mut socket: WebSocket, request: SwapRequest, stats: Stats, ip: IpAddr) {
    let mut pools_updated = pathfinder::pools_updated();
    let mut sent_quotes = None;
    loop {
        // Routes found below use pools at least this new, so only newer ones reroute
        pools_updated.borrow_and_update();
        let started_at = Instant::now();
        let timestamp = Utc::now();
        let (quotes, route_stats) = find_routes(&request).await;
        let duration = started_at.elapsed();

        if sent_quotes.is_none() {
            record_stats(&stats, ip, timestamp, &request, duration, route_stats);
        }
        if sent_quotes.as_ref() != Some(&quotes) {
            let message = serde_json::to_string(&quotes).expect("Quotes are serializable");
            if socket.send(Message::text(message)).await.is_err() {
                return;
            }
            sent_quotes = Some(quotes);
        }

        let cooldown = (duration * REROUTE_COOLDOWN_FACTOR).min(MAX_REROUTE_COOLDOWN);
        tokio::select! {
            _ = tokio::time::sleep_until(started_at + cooldown) => {}
            _ = closed(&mut socket) => return,
        }
        tokio::select! {
            result = pools_updated.changed() => result.expect("Pools sender is static"),
            _ = closed(&mut socket) => return,
        }
    }
}

/// Reads messages until the client disconnects, clients aren't expected to send anything
async fn closed(socket: &mut WebSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        if let Message::Close(_) = message {
            return;
        }
    }
}
