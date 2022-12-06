extern crate core;

mod attacker;
mod beacon_node;
mod param;
mod topic;
mod utils;

use crate::param::NetworkParams;
use crate::utils::{
    publish_and_collect, BARRIER_LIBP2P_READY, BARRIER_NETWORK_READY, BARRIER_TOPOLOGY_READY,
};
use testground::client::Client;
use testground::network_conf::{
    FilterAction, LinkShape, NetworkConfiguration, RoutingPolicyType, DEFAULT_DATA_NETWORK,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let client = Client::new_and_init().await?;
    let network_params = NetworkParams::new(&client.run_parameters().test_instance_params)?;

    client
        .configure_network(NetworkConfiguration {
            network: DEFAULT_DATA_NETWORK.to_owned(),
            ipv4: None,
            ipv6: None,
            enable: true,
            default: LinkShape {
                latency: network_params.latency * 1_000_000, // Convert from millisecond to nanosecond
                jitter: 0,
                bandwidth: network_params.bandwidth * 1024 * 1024,
                filter: FilterAction::Accept,
                loss: 0.0,
                corrupt: 0.0,
                corrupt_corr: 0.0,
                reorder: 0.0,
                reorder_corr: 0.0,
                duplicate: 0.0,
                duplicate_corr: 0.0,
            },
            rules: None,
            callback_state: BARRIER_NETWORK_READY.to_owned(),
            callback_target: None,
            routing_policy: RoutingPolicyType::DenyAll,
        })
        .await?;

    client
        .barrier(
            BARRIER_NETWORK_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

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
