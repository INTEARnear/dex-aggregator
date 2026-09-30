use std::{net::IpAddr, str::FromStr, time::Duration};

use chrono::{DateTime, Utc};
use near_min_api::types::AccountId;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool},
    SqliteConnection,
};
use tracing::{error, info};

use crate::types::{Amount, DexId, TokenId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOutcome {
    Found,
    NotFound,
    TimedOut,
    Panicked,
}

impl RouteOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Found => "found",
            Self::NotFound => "not_found",
            Self::TimedOut => "timed_out",
            Self::Panicked => "panicked",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouteStats {
    pub dex_id: DexId,
    pub duration: Duration,
    pub outcome: RouteOutcome,
}

#[derive(Debug, Clone)]
pub struct QueryStats {
    pub timestamp: DateTime<Utc>,
    pub ip: IpAddr,
    pub token_in: TokenId,
    pub token_out: TokenId,
    pub amount: Amount,
    pub referrer_id: Option<AccountId>,
    pub trader_account_id: Option<AccountId>,
    pub duration: Duration,
    pub routes: Vec<RouteStats>,
}

#[derive(Clone)]
pub struct Stats {
    pool: SqlitePool,
}

impl Stats {
    pub async fn from_env() -> Self {
        let url =
            std::env::var("STATS_DATABASE_URL").unwrap_or_else(|_| "stats.sqlite".to_string());
        let options = SqliteConnectOptions::from_str(&url)
            .unwrap_or_else(|err| panic!("Invalid STATS_DATABASE_URL: {err}"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePool::connect_with(options)
            .await
            .unwrap_or_else(|err| panic!("Failed to open stats database {url}: {err}"));
        sqlx::migrate!()
            .run(&pool)
            .await
            .unwrap_or_else(|err| panic!("Failed to migrate stats database: {err}"));
        info!("Writing stats to {url}");
        Self { pool }
    }

    /// Writes the query in the background.
    pub fn record(&self, query: QueryStats) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let result = async {
                let mut transaction = pool.begin().await?;
                insert_query(&mut transaction, &query).await?;
                transaction.commit().await
            }
            .await;
            if let Err(err) = result {
                error!("Failed to record stats for {query:?}: {err}");
            }
        });
    }
}

async fn insert_query(
    connection: &mut SqliteConnection,
    query: &QueryStats,
) -> Result<(), sqlx::Error> {
    let (swap_type, amount) = match query.amount {
        Amount::AmountIn(amount) => ("exact_in", amount),
        Amount::AmountOut(amount) => ("exact_out", amount),
    };
    let query_id: i64 = sqlx::query_scalar(
        "INSERT INTO queries (timestamp, ip, token_in, token_out, swap_type, amount, referrer_id, trader_account_id, duration_ms)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        RETURNING id",
    )
    .bind(query.timestamp)
    .bind(query.ip.to_string())
    .bind(query.token_in.to_string())
    .bind(query.token_out.to_string())
    .bind(swap_type)
    .bind(amount.to_string())
    .bind(query.referrer_id.as_ref().map(|id| id.to_string()))
    .bind(query.trader_account_id.as_ref().map(|id| id.to_string()))
    .bind(query.duration.as_millis() as i64)
    .fetch_one(&mut *connection)
    .await?;

    for route in &query.routes {
        sqlx::query(
            "INSERT INTO query_routes (query_id, dex_id, duration_ms, outcome) VALUES (?, ?, ?, ?)",
        )
        .bind(query_id)
        .bind(route.dex_id.to_string())
        .bind(route.duration.as_millis() as i64)
        .bind(route.outcome.as_str())
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}
