use crate::utils::{barrier, BARRIER_DIALED, BARRIER_DONE, BARRIER_STARTED_LIBP2P};
use crate::{InstanceInfo, Role};
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::error::{PublishError, SubscriptionError};
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, GossipsubEvent, IdentTopic, IdentityTransform,
    MessageAuthenticity, MessageId, Topic,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{
    NetworkBehaviour, NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters,
    SwarmBuilder, SwarmEvent,
};
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::Swarm;
use libp2p::Transport;
use libp2p::{NetworkBehaviour, PeerId};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use std::task::{Context, Poll};
use std::time::Duration;
use testground::client::Client;
use tokio::time::{interval, Interval};

// The backoff time for pruned peers.
pub(crate) const PRUNE_BACKOFF: u64 = 60;

pub(crate) async fn run(
    client: Client,
    instance_info: InstanceInfo,
    participants: Vec<InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    let mut swarm = {
        let gossipsub_config = GossipsubConfigBuilder::default()
            .prune_backoff(Duration::from_secs(PRUNE_BACKOFF))
            .history_length(12)
            .build()
            .expect("Valid configuration");
        let gossipsub = Gossipsub::new_with_subscription_filter_and_transform(
            MessageAuthenticity::Signed(keypair.clone()),
            gossipsub_config,
            None,
            AllowAllSubscriptionFilter {},
            IdentityTransform {},
        )
        .expect("Valid configuration");

        SwarmBuilder::new(
            build_transport(&keypair),
            HonestBehaviour::new(gossipsub),
            PeerId::from(keypair.public()),
        )
        .executor(Box::new(|future| {
            tokio::spawn(future);
        }))
        .build()
    };

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

    if matches!(instance_info.role, Role::Publisher) {
        // ////////////////////////////////////////////////////////////////////////
        // Publish messages
        // ////////////////////////////////////////////////////////////////////////
        // TODO: Parameterize
        let runtime = Duration::from_secs(10);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(runtime) => {
                    break;
                }
                _ = publish_message_periodically(&client, &mut swarm, topic.clone()) => {}
            }
        }
    }

    barrier(&client, &mut swarm, BARRIER_DONE).await;
    client.record_success().await?;
    Ok(())
}

// Set up an encrypted TCP transport over the Mplex and Yamux protocols.
fn build_transport(keypair: &Keypair) -> libp2p::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
    let transport = TokioDnsConfig::system(TokioTcpTransport::new(
        GenTcpConfig::default().nodelay(true),
    ))
    .expect("DNS config");

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
        .timeout(Duration::from_secs(20))
        .boxed()
}

async fn publish_message_periodically(
    client: &Client,
    swarm: &mut Swarm<HonestBehaviour>,
    topic: IdentTopic,
) {
    // TODO: Parameterize
    let mut interval = interval(Duration::from_millis(500));
    let mut message_counter = 0;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = swarm.behaviour_mut().publish(topic.clone(), format!("message {}", message_counter).as_bytes()) {
                    client.record_message(format!("Failed to publish message: {}", e))
                }
                message_counter += 1;
            }
            event = swarm.select_next_some() => {
                client.record_message(format!("{:?}", event));
            }
        }
    }
}

#[derive(NetworkBehaviour)]
#[behaviour(event_process = true, poll_method = "poll")]
pub(crate) struct HonestBehaviour {
    gossipsub: Gossipsub,
    #[behaviour(ignore)]
    score_interval: Interval,
}

impl HonestBehaviour {
    pub(crate) fn new(gossipsub: Gossipsub) -> Self {
        HonestBehaviour {
            gossipsub,
            score_interval: interval(Duration::from_secs(1)),
        }
    }

    fn publish(
        &mut self,
        topic: IdentTopic,
        message: impl Into<Vec<u8>>,
    ) -> Result<MessageId, PublishError> {
        self.gossipsub.publish(topic, message)
    }

    fn subscribe(&mut self, topic: &IdentTopic) -> Result<bool, SubscriptionError> {
        self.gossipsub.subscribe(topic)
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<(), <HonestBehaviour as NetworkBehaviour>::ConnectionHandler>>
    {
        while self.score_interval.poll_tick(cx).is_ready() {
            for (peer, _) in self.gossipsub.all_peers() {
                // TODO: Store scores to InfluxDB.
                println!("score: {}, {:?}", peer, self.gossipsub.peer_score(peer));
            }
        }

        Poll::Pending
    }
}

impl NetworkBehaviourEventProcess<GossipsubEvent> for HonestBehaviour {
    fn inject_event(&mut self, _: GossipsubEvent) {}
}
