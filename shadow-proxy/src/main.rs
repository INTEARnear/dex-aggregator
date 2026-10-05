use std::{env, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use tokio::{
    fs::OpenOptions,
    io::AsyncWriteExt,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tracing::{Level, error, info};
use tracing_subscriber::FmtSubscriber;

/// Router gives each DEX up to 60 seconds
const REQUEST_TIMEOUT: Duration = Duration::from_secs(70);

const HEADERS_TO_REMOVE: [HeaderName; 8] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

struct Proxy {
    client: reqwest::Client,
    main_url: String,
    testing_url: String,
    results: UnboundedSender<String>,
}

#[derive(Clone)]
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

async fn send(
    client: reqwest::Client,
    url: String,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Answer, String> {
    // The URL isn't in the errors, its query can have an API key
    let response = client
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|e| e.without_url().to_string())?;
    Ok(Answer {
        status,
        headers,
        body,
    })
}

fn filter_headers(mut headers: HeaderMap) -> HeaderMap {
    for name in HEADERS_TO_REMOVE {
        headers.remove(name);
    }
    headers
}

async fn proxy(
    State(proxy): State<Arc<Proxy>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_and_query = uri.path_and_query().map_or("/", |path| path.as_str());

    // Passing through x-forwarded-for because I run nginx, but if you don't I
    // guess you can set connection IP to x-forwarded-for here and run as nginx_ip
    let mut headers = filter_headers(headers);
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);

    let testing = tokio::spawn(send(
        proxy.client.clone(),
        format!("{}{path_and_query}", proxy.testing_url),
        method.clone(),
        headers.clone(),
        body.clone(),
    ));
    let main = send(
        proxy.client.clone(),
        format!("{}{path_and_query}", proxy.main_url),
        method,
        headers,
        body,
    )
    .await;

    let main_answer = main.clone();
    let results = proxy.results.clone();
    let path_and_query = path_and_query.to_string();
    tokio::spawn(async move {
        let testing = testing
            .await
            .unwrap_or_else(|e| Err(format!("Request task failed: {e}")));
        let line = match compare(&main_answer, &testing) {
            Ok(()) => "ok".to_string(),
            Err(mut data) => {
                data["request"] = path_and_query.into();
                format!("error: {data}")
            }
        };
        results
            .send(line)
            .expect("The results are written as long as the server runs");
    });

    match main {
        Ok(answer) => {
            let mut response = Response::new(Body::from(answer.body));
            *response.status_mut() = answer.status;
            *response.headers_mut() = filter_headers(answer.headers);
            response.headers_mut().remove(header::CONTENT_LENGTH);
            response
        }
        Err(e) => {
            error!("Main build failed: {e}");
            (StatusCode::BAD_GATEWAY, "The router failed to respond").into_response()
        }
    }
}

/// `Ok` if the testing build answered the same as the main build, or for routes, with a best quote
/// that is the same or better. Otherwise both answers for debugging.
fn compare(main: &Result<Answer, String>, testing: &Result<Answer, String>) -> Result<(), Value> {
    let (main, testing) = match (main, testing) {
        (Ok(main), Ok(testing)) => (main, testing),
        (main, testing) => {
            return Err(json!({
                "reason": "a build didn't respond",
                "main": main.as_ref().map(summary).unwrap_or_else(|e| e.clone().into()),
                "testing": testing.as_ref().map(summary).unwrap_or_else(|e| e.clone().into()),
            }));
        }
    };
    let mismatch = |reason: &str| {
        json!({
            "reason": reason,
            "main": summary(main),
            "testing": summary(testing),
        })
    };
    if main.status != testing.status {
        return Err(mismatch("different status"));
    }
    if main.status != StatusCode::OK {
        return if main.body == testing.body {
            Ok(())
        } else {
            Err(mismatch("different error"))
        };
    }
    let (Some(main_quotes), Some(testing_quotes)) = (quotes(&main.body), quotes(&testing.body))
    else {
        return Err(mismatch("not a list of routes"));
    };
    match (best(&main_quotes), best(&testing_quotes)) {
        (None, _) => Ok(()),
        (Some(_), None) => Err(mismatch("no route")),
        (Some((main_kind, main_amount)), Some((testing_kind, testing_amount))) => {
            let same_or_better = match (main_kind, testing_kind) {
                ("amount_out", "amount_out") => testing_amount >= main_amount,
                ("amount_in", "amount_in") => testing_amount <= main_amount,
                _ => return Err(mismatch("different kinds of amounts")),
            };
            if same_or_better {
                Ok(())
            } else {
                Err(mismatch("worse quote"))
            }
        }
    }
}

/// (dex, kind, estimated amount, worst case amount) of each route, kind is `amount_out` for
/// exact-in requests and `amount_in` for exact-out
type Quote = (String, String, u128, Value);

fn quotes(body: &[u8]) -> Option<Vec<Quote>> {
    let routes = serde_json::from_slice::<Value>(body).ok()?;
    routes
        .as_array()?
        .iter()
        .map(|route| {
            let (kind, amount) = route["estimated_amount"].as_object()?.iter().next()?;
            Some((
                route["dex_id"].as_str()?.to_string(),
                kind.clone(),
                amount.as_str()?.parse().ok()?,
                route["worst_case_amount"].clone(),
            ))
        })
        .collect()
}

/// The most output of exact-in routes, or the least input of exact-out routes
fn best(quotes: &[Quote]) -> Option<(&str, u128)> {
    let amounts = quotes
        .iter()
        .map(|(_, kind, amount, _)| (kind.as_str(), *amount));
    match quotes.first()?.1.as_str() {
        "amount_in" => amounts.min_by_key(|(_, amount)| *amount),
        _ => amounts.max_by_key(|(_, amount)| *amount),
    }
}

fn summary(answer: &Answer) -> Value {
    match quotes(&answer.body) {
        Some(quotes) if answer.status == StatusCode::OK => quotes
            .into_iter()
            .map(|(dex, kind, amount, worst_case)| {
                json!({
                    "dex": dex,
                    "estimated": { kind: amount.to_string() },
                    "worst_case": worst_case,
                })
            })
            .collect(),
        _ => json!({
            "status": answer.status.as_u16(),
            "body": String::from_utf8_lossy(&answer.body),
        }),
    }
}

async fn write_results(path: String, mut lines: UnboundedReceiver<String>) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .unwrap_or_else(|e| panic!("Failed to open {path}: {e}"));
    while let Some(line) = lines.recv().await {
        file.write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap_or_else(|e| panic!("Failed to write to {path}: {e}"));
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    FmtSubscriber::builder().with_max_level(Level::INFO).init();

    let required = |name: &str| {
        env::var(name).unwrap_or_else(|_| panic!("{name} is required, e.g. http://127.0.0.1:3001"))
    };
    let main_url = required("MAIN_URL");
    let testing_url = required("TESTING_URL");
    let results_path = env::var("SHADOWTEST_FILE").unwrap_or_else(|_| "shadowtest.txt".to_string());
    let bind_address = env::var("BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:3000".to_string());

    let (results, lines) = mpsc::unbounded_channel();
    let proxy = Arc::new(Proxy {
        client: reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap(),
        main_url,
        testing_url,
        results,
    });
    let app = Router::new().fallback(self::proxy).with_state(proxy);

    let listener = tokio::net::TcpListener::bind(&bind_address).await.unwrap();
    info!("Shadow testing proxy running on http://{bind_address}, results in {results_path}");

    tokio::select! {
        result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        ) => result.unwrap(),
        () = write_results(results_path, lines) => unreachable!("The proxy keeps a sender"),
    }
}
