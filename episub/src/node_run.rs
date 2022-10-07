use crate::utils::{BARRIER_DONE, BARRIER_STARTED_LIBP2P, BARRIER_WARMUP};
use crate::InstanceInfo;
use chrono::Local;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, IdentTopic, IdentityTransform, MessageAuthenticity,
    PeerScoreParams, PeerScoreThresholds, Topic, TopicScoreParams,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{DialError, SwarmBuilder, SwarmEvent};
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::PeerId;
use libp2p::Transport;
use libp2p::{Multiaddr, Swarm};
use std::collections::HashMap;
use std::time::Duration;
use testground::client::Client;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, Interval};
use tracing::debug;

/// The backoff time for pruned peers.
pub(crate) const PRUNE_BACKOFF: u64 = 60;

#[derive(Clone)]
pub(crate) struct TestParams {
    pub(crate) warmup: Duration,
    pub(crate) run: Duration,
}

impl TestParams {
    pub(crate) fn new(
        instance_params: HashMap<String, String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let warmup = instance_params
            .get("warmup")
            .ok_or("warmup is not specified.")?
            .parse::<u64>()?;
        let run = instance_params
            .get("run")
            .ok_or("run is not specified.")?
            .parse::<u64>()?;

        Ok(TestParams {
            warmup: Duration::from_secs(warmup),
            run: Duration::from_secs(run),
        })
    }
}

pub(crate) async fn run(
    client: Client,
    instance_info: InstanceInfo,
    participants: HashMap<usize, InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    let run_id = &client.run_parameters().test_run;

    let test_params = TestParams::new(client.run_parameters().test_instance_params)?;

    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    // let mut registry: Registry<Box<dyn EncodeMetric>> = Registry::default();
    // registry.sub_registry_with_prefix("gossipsub");

    let network_send = spawn_network(
        // &mut registry,
        keypair.clone(),
        instance_info.clone(),
        participants,
        client.clone(),
        &test_params,
    )
    .await?;

    client
        .signal_and_wait(
            BARRIER_STARTED_LIBP2P,
            client.run_parameters().test_instance_count,
        )
        .await?;

    // Subscribe to a topic and wait for `warmup` time to expire TODO: subscriptions?
    tokio::time::sleep(test_params.warmup).await;

    client
        .signal_and_wait(BARRIER_WARMUP, client.run_parameters().test_instance_count)
        .await?;

    // Publish messages
    // TODO

    // Record metrics TODO

    client.record_success().await?;
    Ok(())
}

/// Set up an encrypted TCP transport over the Mplex and Yamux protocols.
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

#[derive(Debug)]
enum HonestMessage {
    Subscribe(IdentTopic),
    StartPublishing,
    StopPublishing,
}

enum PublishState {
    Awaiting,
    Started,
    Stopped,
}

pub(crate) struct HonestNetwork {
    swarm: Swarm<Gossipsub>,
    instance_info: InstanceInfo,
    participants: HashMap<usize, InstanceInfo>,
    client: Client,
    score_interval: Interval,
    publish_state: PublishState,
    recv: UnboundedReceiver<HonestMessage>,
}

impl HonestNetwork {
    #[allow(clippy::too_many_arguments)]
    fn new(
        keypair: Keypair,
        instance_info: InstanceInfo,
        participants: HashMap<usize, InstanceInfo>,
        client: Client,
        test_params: TestParams,
        recv: UnboundedReceiver<HonestMessage>,
    ) -> Self {
        let gossipsub = {
            let gossipsub_config = GossipsubConfigBuilder::default()
                .prune_backoff(Duration::from_secs(PRUNE_BACKOFF))
                .history_length(12)
                .build()
                .expect("Valid configuration");

            let mut gs = Gossipsub::new_with_subscription_filter_and_transform(
                MessageAuthenticity::Signed(keypair.clone()),
                gossipsub_config,
                None,
                AllowAllSubscriptionFilter {},
                IdentityTransform {},
            )
            .expect("Valid configuration");

            // Setup the scoring system.
            let peer_score_params = PeerScoreParams::default();
            gs.with_peer_score(peer_score_params, PeerScoreThresholds::default())
                .expect("Valid score params and thresholds");

            gs
        };

        let swarm = SwarmBuilder::new(
            build_transport(&keypair),
            gossipsub,
            PeerId::from(keypair.public()),
        )
        .executor(Box::new(|future| {
            tokio::spawn(future);
        }))
        .build();

        HonestNetwork {
            swarm,
            instance_info,
            participants,
            client,
            score_interval: interval(Duration::from_secs(1)),
            publish_state: PublishState::Awaiting,
            recv,
        }
    }

    async fn start(&mut self) {
        self.swarm
            .listen_on(self.instance_info.multiaddr.clone())
            .expect("Swarm starts listening");

        match self.swarm.next().await.unwrap() {
            SwarmEvent::NewListenAddr { address, .. } => {
                assert_eq!(address, self.instance_info.multiaddr)
            }
            e => panic!("Unexpected event {:?}", e),
        };
    }

    fn dial(&mut self, addr: Multiaddr) -> Result<(), DialError> {
        self.swarm.dial(addr)
    }

    fn spawn(mut self) {
        let fut = async move {
            loop {
                tokio::select! {
                    // Record peer scores
                    _ = self.score_interval.tick() => {
                        // self.record_peer_scores().await;
                    }
                    // Publish messages TODO: here
                    event = self.swarm.select_next_some() => {
                        debug!("SwarmEvent: {:?}", event);
                    }
                    Some(message) = self.recv.recv() => {
                        match message {
                            HonestMessage::Subscribe(topic) => {
                                if let Err(e) = self.swarm.behaviour_mut().subscribe(&topic) {
                                    self.client.record_message(format!("Failed to subscribe: {}", e));
                                }
                            }
                            HonestMessage::StartPublishing => {
                                self.publish_state = PublishState::Started;
                            }
                            HonestMessage::StopPublishing => {
                                self.publish_state = PublishState::Stopped;
                            }
                        }
                    }
                }
            }
        };

        tokio::runtime::Handle::current().spawn(fut);
    }
}

async fn spawn_network(
    keypair: Keypair,
    instance_info: InstanceInfo,
    participants: HashMap<usize, InstanceInfo>,
    client: Client,
    test_params: &TestParams,
) -> Result<UnboundedSender<HonestMessage>, Box<dyn std::error::Error>> {
    let (send, recv) = tokio::sync::mpsc::unbounded_channel();

    let mut honest_network = HonestNetwork::new(
        keypair,
        instance_info,
        participants,
        client.clone(),
        test_params.clone(),
        recv,
    );
    honest_network.start().await;

    honest_network.spawn();

    Ok(send)
}
