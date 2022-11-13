extern crate core;

mod attacker;
mod beacon_node;
mod params;
mod utils;

use crate::utils::{publish_and_collect, BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY};
use testground::client::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let client = Client::new_and_init().await?;

    let result = match client.run_parameters().test_group_id.as_str() {
        "beacon_node" => beacon_node::run(client.clone()).await,
        "attacker" => attacker::run(client.clone()).await,
        _ => unreachable!("Invalid group id"),
    };

    if let Err(e) = result {
        client.record_crash(format!("{}", e), "").await?;
    }

    Ok(())
}
