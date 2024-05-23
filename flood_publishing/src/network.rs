use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Behaviour, ConfigBuilder, FloodPublish, IdentTopic, IdentityTransform,
    Message as GossipsubMessage, MessageAuthenticity, MessageId, PublishError, ValidationMode,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::tcp::tokio::Transport as TcpTransport;
use libp2p::tcp::Config as TcpConfig;
use libp2p::yamux::YamuxConfig;
use libp2p::{Multiaddr, PeerId, Swarm, Transport};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error};

pub(crate) const SLOT: u64 = 12;

/// The backoff time for pruned peers.
const PRUNE_BACKOFF: u64 = 60;

const TOPIC: &str = "t";

pub(crate) struct Network {
    swarm: Swarm<Behaviour>,
    is_publisher: bool,
    node_info: (PeerId, Multiaddr),
    participants: Vec<(PeerId, Multiaddr)>,
    rng: SmallRng,
}

impl Network {
    pub(crate) fn new(
        keypair: Keypair,
        is_publisher: bool,
        node_info: (PeerId, Multiaddr),
        participants: Vec<(PeerId, Multiaddr)>,
        flood_publish: FloodPublish,
    ) -> Self {
        let gossip_message_id = move |message: &GossipsubMessage| {
            MessageId::from(
                &Sha256::digest([message.topic.as_str().as_bytes(), &message.data].concat())[..20],
            )
        };

        let gossipsub_config = ConfigBuilder::default()
            .flood_publish(flood_publish)
            // Following params are set based on lighthouse.
            .max_transmit_size(10 * 1_048_576) // 10M
            .prune_backoff(Duration::from_secs(PRUNE_BACKOFF))
            .fanout_ttl(Duration::from_secs(60))
            .history_length(12)
            .max_messages_per_rpc(Some(500))
            .validate_messages()
            .validation_mode(ValidationMode::Anonymous)
            .duplicate_cache_time(Duration::from_secs(33 * SLOT + 1))
            .message_id_fn(gossip_message_id)
            .allow_self_origin(true)
            // Following params are set based on `NetworkLoad: 4 Average` which defined at lighthouse.
            .heartbeat_interval(Duration::from_millis(700))
            .mesh_n(8)
            .mesh_n_low(4)
            .mesh_outbound_min(3)
            .mesh_n_high(12)
            .gossip_lazy(3)
            .history_gossip(3)
            .build()
            .expect("Valid gossipsub configuration");

        let gossipsub = Behaviour::new_with_subscription_filter_and_transform(
            MessageAuthenticity::Anonymous,
            gossipsub_config,
            None,
            AllowAllSubscriptionFilter {},
            IdentityTransform {},
        )
        .expect("Valid configuration");

        let swarm = SwarmBuilder::with_tokio_executor(
            build_transport(&keypair),
            gossipsub,
            PeerId::from(keypair.public()),
        )
        .build();

        Network {
            swarm,
            is_publisher,
            node_info,
            participants,
            rng: SmallRng::from_entropy(),
        }
    }

    pub(crate) async fn start_libp2p(&mut self) {
        self.swarm
            .listen_on(self.node_info.1.clone())
            .expect("Swarm starts listening");

        match self.swarm.next().await.unwrap() {
            SwarmEvent::NewListenAddr { address, .. } => {
                assert_eq!(address, self.node_info.1)
            }
            e => panic!("Unexpected event {:?}", e),
        };
    }

    pub(crate) fn dial_peers(&mut self) {
        for (peer_id, multiaddr) in self.participants.iter() {
            debug!("dialing {} on {}", peer_id, multiaddr);

            if let Err(e) = self.swarm.dial(
                libp2p::swarm::dial_opts::DialOpts::peer_id(peer_id.clone())
                    .addresses(vec![multiaddr.clone()])
                    .build(),
            ) {
                panic!("Filed to dial {e}: {} {}", peer_id, multiaddr);
            }
        }
    }

    pub(crate) fn subscribe(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.swarm
            .behaviour_mut()
            .subscribe(&IdentTopic::new(TOPIC))?;
        Ok(())
    }

    pub(crate) fn publish(&mut self, message_size: usize) -> Result<MessageId, PublishError> {
        let mut message = vec![0; message_size];
        // Randomize the first 8 bytes to make sure the message is unique.
        let first_bytes = &mut message[0..8];
        self.rng.fill(first_bytes);
        self.swarm
            .behaviour_mut()
            .publish(IdentTopic::new(TOPIC), message)
    }

    pub(crate) async fn run_sim(
        &mut self,
        warm_up: Duration,
        run: Duration,
        cool_down: Duration,
        publish_interval: Duration,
        message_size: usize,
    ) {
        // Warm-up
        let warm_up = tokio::time::sleep(warm_up);
        futures::pin_mut!(warm_up);
        loop {
            tokio::select! {
                _ = warm_up.as_mut() => {
                    debug!(
                        "Warm-up complete. all mesh peers: {}, all peers: {}",
                        self.swarm.behaviour().all_mesh_peers().count(),
                        self.swarm.behaviour().all_peers().count()
                    );
                    break;
                }
                event = self.swarm.select_next_some() => {
                    debug!("Event: {event:?}");
                }
            }
        }

        // Run simulation
        let deadline = tokio::time::sleep(run);
        futures::pin_mut!(deadline);
        let mut publish_interval = interval(publish_interval);
        loop {
            tokio::select! {
                _ = publish_interval.tick(), if self.is_publisher => {
                    if let Err(e) = self.publish(message_size) {
                        error!("Failed to publish message: {e}");
                    }
                }
                _ = deadline.as_mut() => {
                    debug!("Simulation complete");
                    break;
                }
                event = self.swarm.select_next_some() => {
                    debug!("Event: {event:?}");
                }
            }
        }

        // Cool-down
        let cool_down = tokio::time::sleep(cool_down);
        futures::pin_mut!(cool_down);
        loop {
            tokio::select! {
                _ = cool_down.as_mut() => {
                    debug!("Cool-down complete.",);
                    break;
                }
                event = self.swarm.select_next_some() => {
                    debug!("Event: {event:?}");
                }
            }
        }
    }
}

/// Set up an encrypted TCP transport over the Mplex and Yamux protocols.
fn build_transport(keypair: &Keypair) -> libp2p::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
    let transport = TokioDnsConfig::system(TcpTransport::new(TcpConfig::default().nodelay(true)))
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
