extern crate core;

mod beacon_node;
mod utils;
// mod attacker;
mod params;

use crate::utils::{BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY, publish_and_collect};
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use testground::client::Client;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let client = Client::new_and_init().await?;

    match client.run_parameters().test_group_id.as_str() {
        "beacon_node" => {
            beacon_node::run(client).await?;
        }
        "attacker" => {
            println!("attacker");
            if let Err(e) = client
                .signal_and_wait(
                    BARRIER_LIBP2P_READY,
                    client.run_parameters().test_instance_count,
                )
                .await
            {
                panic!("error : BARRIER_LIBP2P_READY : {:?}", e);
            }

            if let Err(e) = client
                .signal_and_wait(
                    BARRIER_TOPOLOGY_READY,
                    client.run_parameters().test_instance_count,
                )
                .await
            {
                panic!("error : BARRIER_TOPOLOGY_READY : {:?}", e);
            }

            client.record_success().await?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

// #[derive(Clone, Debug, Serialize, Deserialize)]
// struct InstanceInfo {
//     peer_id: PeerId,
//     multiaddr: Multiaddr,
// }
