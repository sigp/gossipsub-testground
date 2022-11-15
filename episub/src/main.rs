mod node_run;
mod utils;

use crate::utils::publish_and_collect;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use testground::client::Client;
use tracing::info;

use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let client = Client::new_and_init().await?;

    let local_key = Keypair::generate_ed25519();
    let peer_id = PeerId::from(local_key.public());
    let multiaddr = {
        let mut multiaddr = Multiaddr::from(
            client
                .run_parameters()
                .data_network_ip()?
                .expect("Should have an IP address for the data network"),
        );
        multiaddr.push(Protocol::Tcp(9000));
        multiaddr
    };

    // The network definition starts at 0 and the testground sequences start at 1, so adjust
    // accordingly.
    let node_id = client.global_seq() as usize - 1;
    info!("THIS IS MY NUMBER {node_id}");
    let instance_info = InstanceInfo { peer_id, multiaddr };

    let participants = {
        let infos =
            publish_and_collect("node_info", &client, (node_id, instance_info.clone())).await?;
        info!("Found {}", infos.len());
        infos
            .into_iter()
            .filter(|(other_node_id, _)| *other_node_id != node_id)
            .collect::<HashMap<usize, InstanceInfo>>()
    };

    node_run::run(client, node_id, instance_info, participants, local_key).await?;

    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstanceInfo {
    peer_id: PeerId,
    multiaddr: Multiaddr,
}
