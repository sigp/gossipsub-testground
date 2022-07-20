use crate::utils::{barrier, build_swarm, BARRIER_DIALED, BARRIER_DONE, BARRIER_STARTED_LIBP2P};
use crate::InstanceInfo;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::{IdentTopic, Topic};
use libp2p::identity::Keypair;
use libp2p::swarm::SwarmEvent;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use std::time::Duration;
use testground::client::Client;

pub(crate) async fn run(
    client: Client,
    instance_info: InstanceInfo,
    participants: Vec<InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    let mut swarm = build_swarm(keypair);

    swarm
        .listen_on(instance_info.multiaddr.clone())
        .expect("Swarm starts listening");

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { .. } => break,
            event => {
                client.record_message(format!("{:?}", event));
            }
        }
    }

    barrier(&client, &mut swarm, BARRIER_STARTED_LIBP2P).await;

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Setup discovery
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let peers_to_connect = {
        let mut honest = participants
            .iter()
            .filter(|&info| info.role.is_honest())
            .collect::<Vec<_>>();

        // TODO: Parameterize
        let n_peers: usize = 5;

        // Select peers to connect from the honest.
        let mut rnd = rand::rngs::StdRng::seed_from_u64(client.global_seq());
        honest.shuffle(&mut rnd);
        honest[..n_peers]
            .iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>()
    };
    client.record_message(format!("Peers to connect: {:?}", peers_to_connect));

    for peer in peers_to_connect {
        swarm.dial(peer.multiaddr)?;
    }

    barrier(&client, &mut swarm, BARRIER_DIALED).await;

    // ////////////////////////////////////////////////////////////////////////
    // Subscribe to a topic and wait for `warmup` time to expire
    // ////////////////////////////////////////////////////////////////////////
    let topic: IdentTopic = Topic::new("emulate");
    swarm.behaviour_mut().subscribe(&topic)?;

    // TODO: Parameterize
    let warmup = Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(warmup) => {
                break;
            }
            event = swarm.select_next_some() => {
                client.record_message(format!("{:?}", event));
            }
        }
    }

    barrier(&client, &mut swarm, BARRIER_DONE).await;
    client.record_success().await?;
    Ok(())
}
