use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::core::ConnectedPoint;
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::multiaddr::Protocol;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::tcp::TokioTcpConfig;
use libp2p::yamux::YamuxConfig;
use libp2p::{Multiaddr, PeerId, Transport};
use libp2p_gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p_gossipsub::{
    Gossipsub, GossipsubConfigBuilder, GossipsubEvent, IdentTopic as Topic, IdentityTransform,
    MessageAuthenticity,
};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::time::Duration;
use testground::client::Client;
use testground::RunParameters;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (client, run_parameters) = Client::new().await?;
    client.wait_network_initialized().await?;

    let local_key = Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    client.record_message(format!("PeerId: {}", local_peer_id));

    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    let mut swarm = {
        // Build a Gossipsub network behaviour.
        let gossipsub_config = GossipsubConfigBuilder::default()
            .build()
            .expect("Valid configuration");
        let gossipsub = Gossipsub::new_with_subscription_filter_and_transform(
            MessageAuthenticity::Signed(local_key.clone()),
            gossipsub_config,
            None,
            AllowAllSubscriptionFilter {},
            IdentityTransform {},
        )
        .expect("Valid configuration");

        SwarmBuilder::new(build_transport(&local_key), gossipsub, local_peer_id)
            .executor(Box::new(|future| {
                tokio::spawn(future);
            }))
            .build()
    };

    let multiaddr = {
        let mut multiaddr = Multiaddr::from(
            run_parameters
                .data_network_ip()?
                .expect("Should have an IP address for the data network"),
        );
        multiaddr.push(Protocol::Tcp(9000));
        multiaddr
    };

    swarm
        .listen_on(multiaddr.clone())
        .expect("Swarm starts listening");

    // Sets a barrier on the supplied state that fires when it reaches all participants.
    macro_rules! barrier {
        ($state: expr) => {
            loop {
                tokio::select! {
                    _ = client.signal_and_wait($state, run_parameters.test_instance_count) => {
                        break;
                    }
                    event = swarm.select_next_some() => {
                        client.record_message(format!("{:?}", event))
                    }
                }
            }
            // The sleep time is set to prevent a race condition caused by variations in the timing
            // of receiving messages from the synchronization service of testground.
            tokio::time::sleep(Duration::from_millis(1000)).await;
        };
    }

    barrier!("Started listening");

    // ////////////////////////////////////////////////////////////////////////
    // Connect to a peer randomly selected
    // ////////////////////////////////////////////////////////////////////////
    let peer_addr = {
        // Collect addresses of test case participants.
        let mut others = publish_and_collect(&client, &run_parameters, multiaddr.to_string())
            .await?
            .iter()
            .filter(|&addr| addr != &multiaddr.to_string())
            .map(|addr| Multiaddr::try_from(addr.as_str()).expect("Valid as multiaddr"))
            .collect::<Vec<_>>();

        // Select a peer to connect.
        let mut rnd = rand::rngs::StdRng::seed_from_u64(run_parameters.test_instance_count);
        others.shuffle(&mut rnd);
        others.pop().unwrap()
    };

    client.record_message(format!("Dialing {}", peer_addr));
    swarm.dial(peer_addr)?;

    // Ensure that a connection has been established with the peer selected.
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::ConnectionEstablished {
                peer_id: _,
                endpoint,
                ..
            } => match endpoint {
                ConnectedPoint::Dialer { address, .. } => {
                    client.record_message(format!("Connection established: {}", address));
                    break;
                }
                event => client.record_message(format!("{:?}", event)),
            },
            event => client.record_message(format!("{:?}", event)),
        }
    }

    barrier!("Connected to a peer");

    // ////////////////////////////////////////////////////////////////////////
    // Subscribe a topic
    // ////////////////////////////////////////////////////////////////////////
    // Subscribe each node to the same topic.
    let topic = Topic::new("smoke");
    client.record_message(format!("Subscribing to topic: {}", topic));
    swarm.behaviour_mut().subscribe(&topic)?;

    // Wait for all connected peers to be subscribed.
    let connected_peers = swarm.connected_peers().collect::<Vec<_>>().len();
    let mut subscribed = 0;
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::Behaviour(gossipsub_event) => match gossipsub_event {
                GossipsubEvent::Subscribed { peer_id, topic } => {
                    client.record_message(format!(
                        "Peer {} subscribed to a topic: {}",
                        peer_id, topic
                    ));
                    subscribed += 1;
                    if subscribed == connected_peers {
                        break;
                    }
                }
                _ => unreachable!(),
            },
            event => client.record_message(format!("{:?}", event)),
        }
    }

    barrier!("Subscribed a topic");

    // ////////////////////////////////////////////////////////////////////////
    // Publish a single message
    // ////////////////////////////////////////////////////////////////////////
    {
        let gossipsub = swarm.behaviour();
        client.record_message(format!(
            "all_peers: {:?}, all_mesh_peers: {:?}",
            gossipsub.all_peers().collect::<Vec<_>>(),
            gossipsub.all_mesh_peers().collect::<Vec<_>>()
        ));
    }

    client.record_message("Publishing message");
    swarm.behaviour_mut().publish(topic, "message".as_bytes())?;

    // Wait until all messages published by participants have been received.
    let mut messages_received = 0;
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::Behaviour(gossipsub_event) => match gossipsub_event {
                GossipsubEvent::Message {
                    propagation_source,
                    message_id: _,
                    message,
                } => {
                    client.record_message(format!(
                        "Message: propagation_source: {}, source: {:?}",
                        propagation_source, message.source
                    ));

                    messages_received += 1;
                    if messages_received == run_parameters.test_instance_count - 1 {
                        break;
                    }
                }
                event => client.record_message(format!("{:?}", event)),
            },
            event => client.record_message(format!("{:?}", event)),
        }
    }
    client.record_message("Received all the published messages");

    barrier!("Published a message");

    client.record_success().await?;
    Ok(())
}

// Set up an encrypted TCP Transport over the Mplex and Yamux protocols.
fn build_transport(keypair: &Keypair) -> libp2p::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
    let transport =
        TokioDnsConfig::system(TokioTcpConfig::new().nodelay(true)).expect("DNS config");

    let noise_keys = libp2p::noise::Keypair::<libp2p::noise::X25519Spec>::new()
        .into_authentic(keypair)
        .expect("Signing libp2p-noise static DH keypair failed.");

    transport
        .upgrade(Version::V1)
        .authenticate(NoiseConfig::xx(noise_keys).into_authenticated())
        .multiplex(SelectUpgrade::new(
            YamuxConfig::default(),
            MplexConfig::default(),
        ))
        .timeout(std::time::Duration::from_secs(20))
        .boxed()
}

async fn publish_and_collect<T: Serialize + DeserializeOwned>(
    client: &Client,
    run_parameters: &RunParameters,
    info: T,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    const TOPIC: &str = "publish_and_collect";

    client.publish(TOPIC, serde_json::to_string(&info)?).await?;

    let mut stream = client.subscribe(TOPIC).await;

    let mut vec: Vec<T> = vec![];

    for _ in 0..run_parameters.test_instance_count {
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
