#![deny(clippy::float_arithmetic)]

mod dcl;
mod plach;
mod rhea;

use std::sync::Arc;

use near_min_api::RpcClient;
use pool_indexer::PoolIndexer;
use tracing::{Level, error};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .pretty()
        .with_file(true)
        .with_line_number(true)
        .with_max_level(Level::INFO)
        .init();
    let error = serve().await;
    error!("Stopped: {error:#}");
    Ok(())
}

async fn serve() -> anyhow::Error {
    let (indexer, indexer_task) = PoolIndexer::start(
        vec![rhea::watch(), plach::watch(), dcl::watch()],
        Arc::new(RpcClient::new(
            std::env::var("RPC_URLS")
                .unwrap_or_else(|_| {
                    "https://rpc.intea.rs,https://rpc.shitzuapes.xyz,https://free.rpc.fastnear.com"
                        .to_string()
                })
                .split(',')
                .map(|url| url.to_string())
                .collect::<Vec<_>>(),
        )),
    );

    tokio::select! {
        error = rhea::run(indexer.clone()) => error.context("Rhea"),
        error = plach::run(indexer.clone()) => error.context("Plach"),
        error = dcl::run(indexer) => error.context("Rhea DCL"),
        result = indexer_task => match result {
            Ok(error) => error.context("Pool indexer"),
            Err(e) => anyhow::anyhow!("Pool indexer panicked: {e}"),
        },
    }
}
