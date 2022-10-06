mod honest;
mod utils;

use crate::utils::publish_and_collect;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use testground::client::Client;

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

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Publish information about this test instance to the network and collect the information of
    // all the participants in this test.
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let instance_info = InstanceInfo {
        global_seq: client.global_seq(),
        group_seq: client.group_seq(),
        peer_id,
        multiaddr,
    };

    client.record_message(format!("InstanceInfo: {:?}", instance_info));

    let participants = {
        let mut infos = publish_and_collect("node_info", &client, instance_info.clone()).await?;
        // Remove the info about myself.
        let pos = infos
            .iter()
            .position(|i| i.peer_id == peer_id)
            .expect("Should have info about myself");
        infos.remove(pos);
        infos
    };

    honest::run(client, instance_info, participants, local_key).await?;

    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstanceInfo {
    /// A global sequence number assigned to this test instance by the sync service.
    global_seq: u64,
    /// A group-scoped sequence number assigned to this test instance by the sync service.
    group_seq: u64,
    peer_id: PeerId,
    multiaddr: Multiaddr,
}

impl InstanceInfo {
    fn name(&self) -> String {
        format!("{}", self.group_seq)
    }
}
