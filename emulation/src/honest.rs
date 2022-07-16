use crate::utils::{barrier, BARRIER_DIALED, BARRIER_STARTED_LIBP2P, build_swarm};
use crate::InstanceInfo;
use libp2p::identity::Keypair;
use rand::seq::SliceRandom;
use rand::SeedableRng;
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

    client.record_success().await?;
    Ok(())
}
