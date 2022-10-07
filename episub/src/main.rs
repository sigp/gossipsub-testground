mod node_run;
mod utils;

use crate::utils::publish_and_collect;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use testground::client::Client;

use gen_topology::Network;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
const CONFIG_FILE_KEY: &str = "config_file";
const NULL_CONFIG_FILE: &str = "null";

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

    // Read the network configuration file
    let config_file = client
        .run_parameters()
        .test_instance_params
        .get(CONFIG_FILE_KEY)
        .ok_or("missing configuration file")?
        .to_owned();
    if config_file == NULL_CONFIG_FILE {
        return Err("missing configuration file".into());
    }

    let file = File::open(config_file)?;
    let reader = BufReader::new(file);
    let network: Network = serde_json::from_reader(reader)?;

    // Publish information about this test instance to the network and collect the information of
    // all the participants in this test.
    // The network definition starts at 0 and the testground sequences start at 1, so adjust
    // accordingly.
    let node_id = client.global_seq() as usize - 1;
    let instance_info = InstanceInfo { peer_id, multiaddr };

    client.record_message(format!("InstanceInfo: {:?}", instance_info));

    let participants = {
        let infos =
            publish_and_collect("node_info", &client, (node_id, instance_info.clone())).await?;
        infos
            .into_iter()
            .filter(|(other_node_id, _)| *other_node_id != node_id)
            .collect::<HashMap<usize, InstanceInfo>>()
    };

    let peers_to_dial = network
        .outbound_peers()
        .get(&node_id)
        .expect("Current node id should be in the network configuration");
    for outbound_node_id in peers_to_dial {
        let peer_info = participants
            .get(&outbound_node_id)
            .expect("All participants appear in the network configuration");
        let addr = &peer_info.multiaddr;
        tracing::info!("Dialing [{node_id}] -> [{outbound_node_id}] using {addr}")
    }

    node_run::run(client, instance_info, participants, local_key).await?;

    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstanceInfo {
    peer_id: PeerId,
    multiaddr: Multiaddr,
}
