use crate::utils::{barrier, build_swarm, BARRIER_DIALED, BARRIER_DONE, BARRIER_STARTED_LIBP2P};
use crate::InstanceInfo;
use libp2p::identity::Keypair;
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
    let victim = {
        participants
            .iter()
            .find(|&info| info.role.is_publisher() && info.group_seq == 1)
            .expect("publisher should exists")
            .clone()
    };
    client.record_message(format!("Victim: {:?}", victim));

    swarm.dial(victim.multiaddr)?;

    barrier(&client, &mut swarm, BARRIER_DIALED).await;

    barrier(&client, &mut swarm, BARRIER_DONE).await;
    client.record_success().await?;
    Ok(())
}
