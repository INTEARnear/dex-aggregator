use std::{
    collections::HashMap,
    convert::Infallible,
    fmt::Display,
    sync::{Arc, RwLock},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use near_min_api::types::BlockHeight;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::error;

use crate::shared_utils::{BaseTokenId, TokenInfo, TOKEN_INFOS_REFRESH_INTERVAL, WRAP_NEAR};

const PROTOCOL_VERSION: u32 = 2;
const TOKENS_SUBSCRIPTION_ID: u64 = 1;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

lazy_static! {
    static ref TOKEN_INFOS: RwLock<Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String>> =
        RwLock::new(Err("Token infos are not loaded yet".to_string()));
}

pub fn latest() -> Result<Arc<HashMap<BaseTokenId, TokenInfo>>, String> {
    TOKEN_INFOS.read().unwrap().clone()
}

/// Loads every token, then keeps following their changes in the background. While disconnected,
/// the token infos stay as they were last, and only what changed since is loaded after
/// reconnecting.
pub async fn start_following() {
    let mut tokens = FollowedTokens::default();
    let mut subscribed = tokens.subscribe().await;
    tokio::spawn(async move {
        loop {
            let err = match subscribed {
                Ok(socket) => {
                    let Err(err) = tokens.follow(socket).await;
                    err
                }
                Err(err) => err,
            };
            error!("{err}, reconnecting to the Intear API");
            tokio::time::sleep(RECONNECT_DELAY).await;
            subscribed = tokens.subscribe().await;
        }
    });
}

#[derive(Default)]
struct FollowedTokens {
    /// As the API sent them, for the fields that changed to be merged into
    fields: HashMap<BaseTokenId, Map<String, Value>>,
    infos: HashMap<BaseTokenId, TokenInfo>,
    /// The latest block the infos account for
    height: Option<BlockHeight>,
}

impl FollowedTokens {
    async fn subscribe(&mut self) -> Result<Socket, String> {
        let (mut socket, _) = connect_async("wss://api.intea.rs/ws")
            .await
            .map_err(|err| format!("Failed to connect to the Intear API: {err}"))?;
        match next_message(&mut socket).await? {
            ServerMessage::Hello {
                hello: Hello {
                    version: PROTOCOL_VERSION,
                },
            } => {}
            other => return Err(format!("Expected the Intear API's hello, got {other:?}")),
        }
        let request = json!({
            "id": TOKENS_SUBSCRIPTION_ID,
            "method": "tokens.subscribe",
            "params": {
                "fields": [
                    "price_usd_raw",
                    "price_usd_raw_24h_ago",
                    "circulating_supply",
                    "liquidity_usd",
                    "volume_usd_24h",
                    "created_at",
                ],
                "min_interval_ms": TOKEN_INFOS_REFRESH_INTERVAL.as_millis(),
                "resume_from": self.height,
            },
        });
        socket
            .send(Message::text(request.to_string()))
            .await
            .map_err(|err| format!("Failed to subscribe to the Intear API's tokens: {err}"))?;
        match next_message(&mut socket).await? {
            ServerMessage::Reply {
                id: TOKENS_SUBSCRIPTION_ID,
                result:
                    TokensSubscribeResult::Snapshot {
                        height,
                        columns,
                        rows,
                    },
            } => self.replace(height, columns, rows)?,
            ServerMessage::Reply {
                id: TOKENS_SUBSCRIPTION_ID,
                result: TokensSubscribeResult::Resumed(changes),
            } => self.apply(changes)?,
            ServerMessage::Error {
                id: TOKENS_SUBSCRIPTION_ID,
                error,
            } => {
                return Err(format!(
                    "Failed to subscribe to the Intear API's tokens: {error}"
                ))
            }
            other => return Err(format!("Unexpected message from the Intear API: {other:?}")),
        }
        Ok(socket)
    }

    async fn follow(&mut self, mut socket: Socket) -> Result<Infallible, String> {
        loop {
            match next_message(&mut socket).await? {
                ServerMessage::Push {
                    sub: TOKENS_SUBSCRIPTION_ID,
                    changes,
                } => self.apply(changes)?,
                ServerMessage::Ended {
                    sub: TOKENS_SUBSCRIPTION_ID,
                    end,
                } => {
                    return Err(format!(
                        "The Intear API ended the token subscription: {end}"
                    ))
                }
                other => return Err(format!("Unexpected message from the Intear API: {other:?}")),
            }
        }
    }

    fn replace(
        &mut self,
        height: BlockHeight,
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    ) -> Result<(), String> {
        self.fields.clear();
        self.infos.clear();
        let set = rows
            .into_iter()
            .map(|row| {
                let mut fields = columns.iter().cloned().zip(row).collect::<Map<_, _>>();
                let token_id = fields
                    .remove("account_id")
                    .ok_or("A token from the Intear API has no account_id")?;
                let token_id = BaseTokenId::deserialize(token_id)
                    .map_err(|err| format!("Failed to parse a token from the Intear API: {err}"))?;
                Ok((token_id, fields))
            })
            .collect::<Result<_, String>>()?;
        self.apply(TokenChanges {
            height,
            set,
            left: vec![],
        })
    }

    fn apply(&mut self, changes: TokenChanges) -> Result<(), String> {
        for token_id in &changes.left {
            self.fields.remove(token_id);
            self.infos.remove(token_id);
        }
        for (token_id, changed_fields) in changes.set {
            let fields = self.fields.entry(token_id.clone()).or_default();
            fields.extend(changed_fields);
            let info = TokenInfo::deserialize(&Value::Object(fields.clone())).map_err(|err| {
                format!("Failed to parse {token_id:?} from the Intear API: {err}")
            })?;
            self.infos.insert(token_id, info);
        }
        self.height = Some(changes.height);

        let mut infos = self.infos.clone();
        if let Some(wnear_info) = infos.get(&BaseTokenId::Nep141(WRAP_NEAR.parse().unwrap())) {
            infos.insert(BaseTokenId::Near, wnear_info.clone());
        }
        *TOKEN_INFOS.write().unwrap() = Ok(Arc::new(infos));
        Ok(())
    }
}

/// The next message other than an `idle`, which the API only sends to show it's still there
async fn next_message(socket: &mut Socket) -> Result<ServerMessage, String> {
    loop {
        let message = socket
            .next()
            .await
            .ok_or("The Intear API closed the connection")?
            .map_err(|err| format!("The Intear API connection failed: {err}"))?;
        let text = match message {
            Message::Text(text) => text,
            Message::Close(frame) => {
                return Err(format!("The Intear API closed the connection: {frame:?}"))
            }
            _ => continue,
        };
        let message = serde_json::from_str::<Value>(&text)
            .map_err(|err| format!("Failed to parse a message from the Intear API: {err}"))?;
        if message.get("idle").is_some() {
            continue;
        }
        return ServerMessage::deserialize(message)
            .map_err(|err| format!("Unknown message from the Intear API: {err}"));
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ServerMessage {
    Hello {
        hello: Hello,
    },
    Reply {
        id: u64,
        result: TokensSubscribeResult,
    },
    Error {
        id: u64,
        error: ApiError,
    },
    /// The subscription was ended by the API, for example when this connection fell too far
    /// behind
    Ended {
        sub: u64,
        end: ApiError,
    },
    Push {
        sub: u64,
        #[serde(flatten)]
        changes: TokenChanges,
    },
}

#[derive(Debug, Deserialize)]
struct Hello {
    version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TokensSubscribeResult {
    /// Every token, as a table with a row per token. Also after reconnecting, when the API can't
    /// tell what changed since `resume_from`.
    Snapshot {
        height: BlockHeight,
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// What changed since `resume_from`
    Resumed(TokenChanges),
}

#[derive(Debug, Deserialize)]
struct TokenChanges {
    height: BlockHeight,
    /// The fields that changed, or every field of a token that's new to the subscription
    #[serde(default)]
    set: HashMap<BaseTokenId, Map<String, Value>>,
    #[serde(default)]
    left: Vec<BaseTokenId>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

impl Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
