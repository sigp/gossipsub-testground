extern crate core;

use gossipsub::AllowAllSubscriptionFilter;
use gossipsub::{
    Behaviour, ConfigBuilder, Event, IdentTopic as Topic, IdentityTransform, MessageAuthenticity,
};
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::ConnectedPoint;
use libp2p::futures::FutureExt;
use libp2p::futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::SwarmEvent;
use libp2p::{noise, yamux, Multiaddr, PeerId, Swarm, SwarmBuilder, Transport};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Duration;
use testground::client::Client;
use tracing::{debug, info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let client = Client::new_and_init().await?;

    let local_key = Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    client.record_message(format!("PeerId: {}", local_peer_id));

    // Variables to keep track of the received events.
    let mut event_subscribed = 0;
    let mut event_message = HashSet::new();

    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    let mut swarm = {
        // Build a Gossipsub network behaviour.
        let gossipsub_config = ConfigBuilder::default()
            .history_length(
                client
                    .run_parameters()
                    .test_instance_params
                    .get("gossipsub_history_length")
                    .ok_or("gossipsub_history_length is not specified")?
                    .parse::<usize>()?,
            )
            .build()
            .expect("Valid configuration");
        let gossipsub = Behaviour::new_with_subscription_filter_and_transform(
            MessageAuthenticity::Signed(local_key.clone()),
            gossipsub_config,
            None,
            AllowAllSubscriptionFilter {},
            IdentityTransform {},
        )
        .expect("Valid configuration");
        let transport = build_transport(&local_key);
        SwarmBuilder::with_existing_identity(local_key)
            .with_tokio()
            .with_other_transport(|_| transport)
            .expect("infallible")
            .with_behaviour(|_| gossipsub)
            .expect("infallible")
            .build()
    };

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

    swarm
        .listen_on(multiaddr.clone())
        .expect("Swarm starts listening");

    barrier_and_drive_swarm(
        &client,
        &mut swarm,
        "Started listening",
        &mut event_subscribed,
        &mut event_message,
    )
    .await?;

    // ////////////////////////////////////////////////////////////////////////
    // Connect to a peer randomly selected
    // ////////////////////////////////////////////////////////////////////////
    let peer_addr = {
        // Collect addresses of test case participants.
        let mut others = publish_and_collect(&client, multiaddr.to_string())
            .await?
            .iter()
            .filter(|&addr| addr != &multiaddr.to_string())
            .map(|addr| Multiaddr::try_from(addr.as_str()).expect("Valid as multiaddr"))
            .collect::<Vec<_>>();

        // Select a peer to connect.
        let mut rnd =
            rand::rngs::StdRng::seed_from_u64(client.run_parameters().test_instance_count);
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

    barrier_and_drive_swarm(
        &client,
        &mut swarm,
        "Connected to a peer",
        &mut event_subscribed,
        &mut event_message,
    )
    .await?;

    // ////////////////////////////////////////////////////////////////////////
    // Subscribe a topic
    // ////////////////////////////////////////////////////////////////////////
    // Subscribe each node to the same topic.
    let topic = Topic::new("smoke");
    client.record_message(format!("Subscribing to topic: {}", topic));
    swarm.behaviour_mut().subscribe(&topic)?;

    // Wait for all connected peers to be subscribed.
    let all_peers = swarm.behaviour().all_peers().count();
    if event_subscribed < all_peers {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::Behaviour(gossipsub_event) => match gossipsub_event {
                    Event::Subscribed { peer_id, topic } => {
                        client.record_message(format!(
                            "Peer {} subscribed to a topic: {}",
                            peer_id, topic
                        ));
                        event_subscribed += 1;
                        if event_subscribed == all_peers {
                            break;
                        }
                    }
                    _ => unreachable!(),
                },
                event => client.record_message(format!("{:?}", event)),
            }
        }
    }

    barrier_and_drive_swarm(
        &client,
        &mut swarm,
        "Subscribed a topic",
        &mut event_subscribed,
        &mut event_message,
    )
    .await?;

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
    if event_message.len() < (client.run_parameters().test_instance_count - 1) as usize {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::Behaviour(gossipsub_event) => match gossipsub_event {
                    Event::Message {
                        propagation_source,
                        message_id: _,
                        message,
                    } => {
                        client.record_message(format!(
                            "Message: propagation_source: {}, source: {:?}",
                            propagation_source, message.source
                        ));

                        if !event_message.insert(message.source.expect("Source peer id")) {
                            client
                                .record_failure(format!(
                                    "Received duplicated message: {:?}",
                                    message
                                ))
                                .await?;
                            return Ok(());
                        }

                        if event_message.len()
                            == (client.run_parameters().test_instance_count - 1) as usize
                        {
                            break;
                        }
                    }
                    event => client.record_message(format!("{:?}", event)),
                },
                event => client.record_message(format!("{:?}", event)),
            }
        }
    }

    client.record_message("Received all the published messages");

    barrier_and_drive_swarm(
        &client,
        &mut swarm,
        "Published a message",
        &mut event_subscribed,
        &mut event_message,
    )
    .await?;

    client.record_success().await?;

    Ok(())
}

// Set up an encrypted TCP transport.
fn build_transport(keypair: &Keypair) -> libp2p::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
    let tcp = libp2p::tcp::tokio::Transport::new(libp2p::tcp::Config::default().nodelay(true));
    let transport = libp2p::dns::tokio::Transport::system(tcp)
        .expect("DNS")
        .boxed();

    transport
        .upgrade(libp2p::core::upgrade::Version::V1)
        .authenticate(
            noise::Config::new(keypair).expect("signing can fail only once during starting a node"),
        )
        .multiplex(yamux::Config::default())
        .timeout(Duration::from_secs(20))
        .boxed()
}

// Publish info and collect it from the participants. The return value includes one published by
// myself.
async fn publish_and_collect<T: Serialize + DeserializeOwned>(
    client: &Client,
    info: T,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    const TOPIC: &str = "publish_and_collect";

    client
        .publish(
            TOPIC,
            Cow::Owned(Value::String(serde_json::to_string(&info)?)),
        )
        .await?;

    let mut stream = client.subscribe(TOPIC, u16::MAX.into()).await;

    let mut vec: Vec<T> = vec![];

    for _ in 0..client.run_parameters().test_instance_count {
        match stream.next().await {
            Some(Ok(other)) => {
                let info: T = match other {
                    Value::String(str) => serde_json::from_str(&str)?,
                    _ => unreachable!(),
                };
                vec.push(info);
            }
            Some(Err(e)) => return Err(Box::new(e)),
            None => unreachable!(),
        }
    }

    Ok(vec)
}

/// Sets a barrier on the supplied state that fires when it reaches all participants.
async fn barrier_and_drive_swarm(
    client: &Client,
    swarm: &mut Swarm<Behaviour>,
    state: impl Into<Cow<'static, str>> + Copy,
    event_subscribed: &mut usize,
    event_message: &mut HashSet<PeerId>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!(
        "Signal and wait for all peers to signal being done with \"{}\".",
        state.into(),
    );

    let events = swarm
        .take_until(
            client
                .signal_and_wait(state, client.run_parameters().test_instance_count)
                .boxed_local(),
        )
        .collect::<Vec<_>>()
        .await;

    for event in &events {
        match event {
            SwarmEvent::Behaviour(gossipsub_event) => match gossipsub_event {
                Event::Subscribed { peer_id, topic } => {
                    client.record_message(format!(
                        "Peer {} subscribed to a topic: {}",
                        peer_id, topic
                    ));
                    *event_subscribed += 1;
                }
                Event::Message {
                    propagation_source,
                    message_id: _,
                    message,
                } => {
                    client.record_message(format!(
                        "Message: propagation_source: {}, source: {:?}",
                        propagation_source, message.source
                    ));

                    if !event_message.insert(message.source.expect("Source peer id")) {
                        warn!("Received duplicated message: {:?}", message);
                    }
                }
                _ => debug!("GossipsubEvent: {:?}", gossipsub_event),
            },
            _ => debug!("SwarmEvent: {:?}", event),
        };
    }

    Ok(())
}
