use std::{
    collections::{HashMap, VecDeque},
    env::VarError,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Query, Request, State},
    http::{header::RETRY_AFTER, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension,
};
use ipnet::IpNet;
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::shared_utils::REQWEST_CLIENT;

const CLOUDFLARE_IPS_URL: &str = "https://api.cloudflare.com/client/v4/ips";
const CLOUDFLARE_IPS_REFRESH_INTERVAL: Duration = Duration::from_hours(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitSource {
    /// Clients connect directly, the TCP peer is the client.
    Ip,
    /// nginx on the same host. The peer must be loopback, the client IP is taken
    /// from `X-Forwarded-For`.
    NginxIp,
    /// Cloudflare. The peer must be a Cloudflare IP, the client IP is taken from
    /// `CF-Connecting-IP`.
    CloudflareIp,
    /// Cloudflare, then nginx on the same host. The peer must be loopback,
    /// `X-Forwarded-For` must be a Cloudflare IP, the client IP is taken from
    /// `CF-Connecting-IP`.
    CloudflareNginxIp,
}

impl RateLimitSource {
    fn uses_cloudflare(self) -> bool {
        matches!(self, Self::CloudflareIp | Self::CloudflareNginxIp)
    }
}

impl FromStr for RateLimitSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "IP" => Self::Ip,
            "NGINX_IP" => Self::NginxIp,
            "CLOUDFLARE_IP" => Self::CloudflareIp,
            "CLOUDFLARE_NGINX_IP" => Self::CloudflareNginxIp,
            _ => {
                return Err(format!(
                    "{s:?} is not one of IP, NGINX_IP, CLOUDFLARE_IP, CLOUDFLARE_NGINX_IP"
                ))
            }
        })
    }
}

/// At most `requests` in any `period`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RateLimit {
    requests: usize,
    period: Duration,
}

impl FromStr for RateLimit {
    type Err = String;

    /// Parses `<requests>/<seconds>`, e.g. `5/2` is 5 requests every 2 seconds.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let error = || format!("{s:?} is not <requests>/<seconds> with both above 0, e.g. 5/2");
        let (requests, seconds) = s.split_once('/').ok_or_else(error)?;
        let requests: usize = requests.parse().map_err(|_| error())?;
        let seconds: u32 = seconds.parse().map_err(|_| error())?;
        if requests == 0 || seconds == 0 {
            return Err(error());
        }
        Ok(Self {
            requests,
            period: Duration::from_secs(seconds.into()),
        })
    }
}

impl RateLimit {
    /// Records a request made at `now` if the client made fewer than `requests`
    /// in the last `period`, otherwise returns how long until it can retry.
    fn check(&self, history: &mut VecDeque<Instant>, now: Instant) -> Result<(), Duration> {
        while history
            .front()
            .is_some_and(|&time| now.duration_since(time) >= self.period)
        {
            history.pop_front();
        }
        if history.len() < self.requests {
            history.push_back(now);
            Ok(())
        } else {
            Err(self.period - now.duration_since(history[0]))
        }
    }
}

/// Comma-separated keys. They're passed as `&key=` as-is, so only URL-safe
/// characters are allowed.
#[derive(Debug, Default, PartialEq, Eq)]
struct ApiKeys(Vec<String>);

impl FromStr for ApiKeys {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(Self::default());
        }
        s.split(',')
            .enumerate()
            .map(|(i, key)| {
                let invalid_char = key
                    .chars()
                    .find(|&c| !c.is_ascii_alphanumeric() && !"-._~".contains(c));
                if key.is_empty() {
                    Err(format!("key #{} is empty", i + 1))
                } else if let Some(c) = invalid_char {
                    Err(format!(
                        "key #{} contains {c:?}, only A-Z a-z 0-9 - . _ ~ are allowed",
                        i + 1
                    ))
                } else {
                    Ok(key.to_string())
                }
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

/// Compares against every key in constant time, so response timing doesn't
/// reveal how close a guess was.
fn is_valid_api_key(api_keys: &[String], key: &str) -> bool {
    api_keys.iter().fold(false, |valid, api_key| {
        let diff = api_key
            .bytes()
            .zip(key.bytes())
            .fold(0, |diff, (a, b)| diff | (a ^ b));
        valid | (api_key.len() == key.len() && diff == 0)
    })
}

fn resolve_client_ip(
    source: RateLimitSource,
    peer: IpAddr,
    headers: &HeaderMap,
    cloudflare_ranges: &[IpNet],
) -> Result<IpAddr, String> {
    let peer = peer.to_canonical();
    match source {
        RateLimitSource::Ip => Ok(peer),
        RateLimitSource::NginxIp => {
            require_loopback(peer)?;
            header_ip(headers, "x-forwarded-for")
        }
        RateLimitSource::CloudflareIp => {
            require_cloudflare(peer, cloudflare_ranges)?;
            header_ip(headers, "cf-connecting-ip")
        }
        RateLimitSource::CloudflareNginxIp => {
            require_loopback(peer)?;
            require_cloudflare(header_ip(headers, "x-forwarded-for")?, cloudflare_ranges)?;
            header_ip(headers, "cf-connecting-ip")
        }
    }
}

fn require_loopback(peer: IpAddr) -> Result<(), String> {
    if peer.is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "{peer} connected directly instead of through nginx"
        ))
    }
}

fn require_cloudflare(ip: IpAddr, cloudflare_ranges: &[IpNet]) -> Result<(), String> {
    if cloudflare_ranges.iter().any(|range| range.contains(&ip)) {
        Ok(())
    } else {
        Err(format!("{ip} is not a Cloudflare IP"))
    }
}

/// Reads an IP from a header that must be present exactly once.
fn header_ip(headers: &HeaderMap, name: &str) -> Result<IpAddr, String> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<IpAddr>().ok())
            .map(|ip| ip.to_canonical())
            .ok_or_else(|| format!("{name} header is not a valid IP")),
        _ => Err(format!("expected exactly one {name} header")),
    }
}

/// IPv6 clients usually get a whole /64, so it's limited as one client.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(ip) => IpAddr::V6(Ipv6Addr::from_bits(ip.to_bits() & (u128::MAX << 64))),
    }
}

/// Parses an env variable, panicking if it's set but invalid.
fn parse_env<T: FromStr<Err = String>>(name: &str) -> Option<T> {
    match std::env::var(name) {
        Ok(value) => Some(
            value
                .parse()
                .unwrap_or_else(|err| panic!("Invalid {name}: {err}")),
        ),
        Err(VarError::NotPresent) => None,
        Err(err) => panic!("Invalid {name}: {err}"),
    }
}

#[derive(Deserialize)]
struct CloudflareIpsResponse {
    result: CloudflareIps,
}

#[derive(Deserialize)]
struct CloudflareIps {
    ipv4_cidrs: Vec<IpNet>,
    ipv6_cidrs: Vec<IpNet>,
}

async fn fetch_cloudflare_ranges() -> Result<Vec<IpNet>, String> {
    let response = REQWEST_CLIENT
        .get(CLOUDFLARE_IPS_URL)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|err| format!("Failed to fetch Cloudflare IP ranges: {err}"))?;
    let CloudflareIpsResponse {
        result: CloudflareIps {
            ipv4_cidrs,
            ipv6_cidrs,
        },
    } = response
        .json()
        .await
        .map_err(|err| format!("Failed to parse Cloudflare IP ranges: {err}"))?;
    if ipv4_cidrs.is_empty() || ipv6_cidrs.is_empty() {
        return Err("Cloudflare returned no IP ranges".to_string());
    }
    Ok([ipv4_cidrs, ipv6_cidrs].concat())
}

pub struct RateLimiter {
    source: RateLimitSource,
    limit: RateLimit,
    api_keys: Vec<String>,
    /// Times of recent requests without an API key, per [`client_key`]
    history: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    /// Empty unless the source uses Cloudflare
    cloudflare_ranges: RwLock<Vec<IpNet>>,
}

impl RateLimiter {
    /// Reads the configuration and, for Cloudflare sources, fetches Cloudflare
    /// IP ranges. Panics if anything is missing or invalid.
    pub async fn from_env() -> Arc<Self> {
        let source: RateLimitSource =
            parse_env("RATE_LIMIT_SOURCE").expect("RATE_LIMIT_SOURCE must be set");
        let limit: RateLimit =
            parse_env("UNAUTHORIZED_RATE_LIMIT").expect("UNAUTHORIZED_RATE_LIMIT must be set");
        let ApiKeys(api_keys) = parse_env("API_KEYS").unwrap_or_default();
        let cloudflare_ranges = if source.uses_cloudflare() {
            fetch_cloudflare_ranges()
                .await
                .unwrap_or_else(|err| panic!("{err}"))
        } else {
            Vec::new()
        };
        info!(
            "Rate limiting requests without an API key to {} per {:?} by {source:?}, {} API keys",
            limit.requests,
            limit.period,
            api_keys.len()
        );

        let limiter = Arc::new(Self {
            source,
            limit,
            api_keys,
            history: Mutex::default(),
            cloudflare_ranges: RwLock::new(cloudflare_ranges),
        });
        tokio::spawn(limiter.clone().forget_idle_clients_loop());
        if source.uses_cloudflare() {
            tokio::spawn(limiter.clone().refresh_cloudflare_ranges_loop());
        }
        limiter
    }

    /// Drops clients without requests in the last period, so memory doesn't grow
    /// with every IP ever seen.
    async fn forget_idle_clients_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.limit.period);
        loop {
            interval.tick().await;
            let now = Instant::now();
            self.history.lock().unwrap().retain(|_, history| {
                history
                    .back()
                    .is_some_and(|&time| now.duration_since(time) < self.limit.period)
            });
        }
    }

    async fn refresh_cloudflare_ranges_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(CLOUDFLARE_IPS_REFRESH_INTERVAL).await;
            match fetch_cloudflare_ranges().await {
                Ok(ranges) => *self.cloudflare_ranges.write().unwrap() = ranges,
                Err(err) => error!("{err}, keeping the previous ranges"),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

/// Rejects requests that didn't arrive through `RATE_LIMIT_SOURCE`, and passes
/// the client IP to [`limit_unauthorized`].
pub async fn validate_source(
    State(limiter): State<Arc<RateLimiter>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let client_ip = resolve_client_ip(
        limiter.source,
        peer.ip(),
        request.headers(),
        &limiter.cloudflare_ranges.read().unwrap(),
    );
    match client_ip {
        Ok(ip) => {
            request.extensions_mut().insert(ClientIp(ip));
            next.run(request).await
        }
        Err(err) => {
            warn!("Rejected request from {peer}: {err}");
            (StatusCode::FORBIDDEN, err).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct KeyQuery {
    key: Option<String>,
}

/// Lets requests with a valid `key` through, and limits the rest per client IP.
pub async fn limit_unauthorized(
    State(limiter): State<Arc<RateLimiter>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Query(KeyQuery { key }): Query<KeyQuery>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(key) = key {
        if !is_valid_api_key(&limiter.api_keys, &key) {
            warn!("Rejected request from {ip}: invalid API key");
            return (StatusCode::UNAUTHORIZED, "Invalid API key").into_response();
        }
        return next.run(request).await;
    }

    let result = {
        let mut history = limiter.history.lock().unwrap();
        let client_history = history.entry(client_key(ip)).or_default();
        limiter.limit.check(client_history, Instant::now())
    };
    match result {
        Ok(()) => next.run(request).await,
        Err(retry_after) => {
            let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(RETRY_AFTER, seconds.to_string())],
                format!("Rate limit exceeded, retry in {seconds}s or use an API key"),
            )
                .into_response()
        }
    }
}
