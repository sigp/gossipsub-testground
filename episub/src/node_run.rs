use crate::utils::{BARRIER_DONE, BARRIER_TOPOLOGY_READY, BARRIER_WARMUP};
use crate::InstanceInfo;
use chrono::Local;
use gen_topology::Network as Topology;
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
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use testground::client::Client;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, Interval};
use tracing::{debug, info};

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
    node_id: usize,
    instance_info: InstanceInfo,
    participants: HashMap<usize, InstanceInfo>,
    topology: Topology,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    let run_id = &client.run_parameters().test_run;

    let test_params = TestParams::new(client.run_parameters().test_instance_params)?;

    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    // let mut registry: Registry<Box<dyn EncodeMetric>> = Registry::default();
    // registry.sub_registry_with_prefix("gossipsub");

    let keypair = keypair.clone();
    let node_id = node_id;
    let instance_info = instance_info.clone();
    let client = client.clone();
    let test_params = &test_params;

    let (params, outbound_peers, validator_assignments): (
        gen_topology::Params,
        std::collections::BTreeMap<usize, Vec<usize>>,
        std::collections::BTreeMap<usize, std::collections::BTreeSet<usize>>,
    ) = topology.destructure();
    let validator_set = validator_assignments
        .get(&node_id)
        .cloned()
        .unwrap_or_default();
    let validator_set: HashSet<u64> = validator_set.into_iter().map(|v| v as u64).collect();
    let mut network = Network::new(
        keypair,
        node_id,
        instance_info,
        participants.clone(),
        client.clone(),
        validator_set,
        test_params.clone(),
    );

    // Set up the listening address
    network.start_libp2p().await;
    // Dial the designated outbound peers
    network.dial_peers(outbound_peers).await;

    client
        .signal_and_wait(
            BARRIER_TOPOLOGY_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

    network.run_sim().await;

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

pub(crate) struct Network {
    swarm: Swarm<Gossipsub>,
    node_id: usize,
    instance_info: InstanceInfo,
    test_params: TestParams,
    participants: HashMap<usize, InstanceInfo>,
    client: Client,
    score_interval: Interval,
    validator_set: HashSet<u64>,
}

impl Network {
    #[allow(clippy::too_many_arguments)]
    fn new(
        keypair: Keypair,
        node_id: usize,
        instance_info: InstanceInfo,
        participants: HashMap<usize, InstanceInfo>,
        client: Client,
        validator_set: HashSet<u64>,
        test_params: TestParams,
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

        info!(
            "[{}] running with {} validators",
            node_id,
            validator_set.len()
        );

        Network {
            swarm,
            node_id,
            instance_info,
            participants,
            client,
            score_interval: interval(Duration::from_secs(1)),
            test_params,
            validator_set,
        }
    }

    async fn start_libp2p(&mut self) {
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

    pub async fn dial_peers(
        &mut self,
        outbound_peers: std::collections::BTreeMap<usize, Vec<usize>>,
    ) {
        let mut dialed_peers = 0;
        if let Some(outbound_peers) = outbound_peers.get(&self.node_id) {
            for peer_node_id in outbound_peers {
                let InstanceInfo { peer_id, multiaddr } = self
                    .participants
                    .get(peer_node_id)
                    .expect("All outbound peers are participants in the network")
                    .clone();
                info!(
                    "[{}] dialing {} on {}",
                    self.node_id, peer_node_id, multiaddr
                );
                if let Err(e) = self.swarm.dial(
                    libp2p::swarm::dial_opts::DialOpts::peer_id(peer_id)
                        .addresses(vec![multiaddr])
                        .build(),
                ) {
                    panic!(
                        "[{}] Dialing -> {} failed {}",
                        self.node_id, peer_node_id, e
                    );
                }
                dialed_peers += 1;
            }
        }
        info!("[{}] dialed {} peers", self.node_id, dialed_peers);
    }

    async fn run_sim(&mut self) {
        let deadline = tokio::time::sleep(self.test_params.run);
        futures::pin_mut!(deadline);
        loop {
            tokio::select! {
                _ = deadline.as_mut() => {
                    // Sim complete
                    break;
                }
                // Record peer scores
                _ = self.score_interval.tick() => {
                    // self.record_peer_scores().await;
                }
                // Publish messages TODO: here
                event = self.swarm.select_next_some() => {
                    debug!("SwarmEvent: {:?}", event);
                }
            }
        }
    }
}
