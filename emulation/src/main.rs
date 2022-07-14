use libp2p::futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use serde::de::DeserializeOwned;
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
        role: client.run_parameters().test_group_id.as_str().into(),
    };

    client.record_message(format!("InstanceInfo: {:?}", instance_info));

    let participants = {
        let mut infos = publish_and_collect(&client, instance_info.clone()).await?;
        // Remove the info about myself.
        let pos = infos
            .iter()
            .position(|i| i.peer_id == peer_id)
            .expect("Should have info about myself");
        infos.remove(pos);
        infos
    };
    println!("Participants: {:?}", participants); // Debug

    client.record_success().await?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Role {
    Publisher,
    Lurker,
    Attacker,
}

impl From<&str> for Role {
    fn from(test_group_id: &str) -> Self {
        match test_group_id {
            "publishers" => Role::Publisher,
            "lurkers" => Role::Lurker,
            "attackers" => Role::Attacker,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstanceInfo {
    /// A global sequence number assigned to this test instance by the sync service.
    global_seq: u64,
    /// A group-scoped sequence number assigned to this test instance by the sync service.
    group_seq: u64,
    peer_id: PeerId,
    multiaddr: Multiaddr,
    role: Role,
}

// Publish info and collect it from the participants. The return value includes one published by
// myself.
async fn publish_and_collect<T: Serialize + DeserializeOwned>(
    client: &Client,
    info: T,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    const TOPIC: &str = "publish_and_collect";

    client.publish(TOPIC, serde_json::to_string(&info)?).await?;

    let mut stream = client.subscribe(TOPIC).await;

    let mut vec: Vec<T> = vec![];

    for _ in 0..client.run_parameters().test_instance_count {
        match stream.next().await {
            Some(Ok(other)) => {
                let info: T = serde_json::from_str(&other)?;
                vec.push(info);
            }
            Some(Err(e)) => return Err(Box::new(e)),
            None => unreachable!(),
        }
    }

    Ok(vec)
}
