use crate::utils::{BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY};
use crate::InstanceInfo;
use gen_topology::Params;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, IdentityTransform, MessageAuthenticity, PeerScoreParams,
    PeerScoreThresholds,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::PeerId;
use libp2p::Swarm;
use libp2p::Transport;
use npg::slot_generator::{Subnet, ValId};
use npg::{Generator, Message};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use testground::client::Client;
use tokio::time::{interval, Interval};
use tracing::{debug, info};

pub(crate) fn parse_params(
    instance_count: usize,
    instance_params: HashMap<String, String>,
) -> Result<(Duration, Params), Box<dyn std::error::Error>> {
    let seed = instance_params
        .get("seed")
        .ok_or("seed is not specified.")?
        .parse::<u64>()?;
    let no_val_percentage = instance_params
        .get("no_val_percentage")
        .ok_or("`no_val_percentage` is not specified")?
        .parse::<usize>()?
        .min(100);
    let total_validators = instance_params
        .get("total_validators")
        .ok_or("`total_validators` not specified")?
        .parse::<usize>()?;
    let min_peers_per_node = instance_params
        .get("min_peers_per_node")
        .ok_or("`min_peers_per_node` not specified")?
        .parse::<usize>()?;
    let max_peers_per_node_inclusive = instance_params
        .get("max_peers_per_node_inclusive")
        .ok_or("`max_peers_per_node_inclusive` not specified")?
        .parse::<usize>()?;
    let total_nodes_without_vals = instance_count * no_val_percentage / 100;
    let total_nodes_with_vals = instance_count - total_nodes_without_vals;
    let run = instance_params
        .get("run")
        .ok_or("run is not specified.")?
        .parse::<u64>()?;

    let params = Params::new(
        seed,
        total_validators,
        total_nodes_with_vals,
        instance_count,
        min_peers_per_node,
        max_peers_per_node_inclusive,
    )?;

    Ok((Duration::from_secs(run), params))
}

pub(crate) async fn run(
    client: Client,
    node_id: usize,
    instance_info: InstanceInfo,
    participants: HashMap<usize, InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    let run_id = &client.run_parameters().test_run;

    let test_instance_count = client.run_parameters().test_instance_count;
    let (run_duration, params) = parse_params(
        test_instance_count as usize,
        client.run_parameters().test_instance_params,
    )?;

    let (params, outbound_peers, validator_assignments) =
        gen_topology::Network::generate(params)?.destructure();

    let validator_set = validator_assignments
        .get(&node_id)
        .cloned()
        .unwrap_or_default();
    let validator_set: HashSet<ValId> =
        validator_set.into_iter().map(|v| ValId(v as u64)).collect();
    let mut network = Network::new(
        keypair,
        node_id,
        instance_info,
        participants.clone(),
        client.clone(),
        validator_set,
        params,
    );

    client
        .signal_and_wait(BARRIER_TOPOLOGY_READY, test_instance_count)
        .await?;

    // Set up the listening address
    network.start_libp2p().await;

    client
        .signal_and_wait(BARRIER_LIBP2P_READY, test_instance_count)
        .await?;
    // Dial the designated outbound peers
    network.dial_peers(outbound_peers).await;

    client
        .signal_and_wait(
            BARRIER_TOPOLOGY_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

    network.run_sim(run_duration).await;

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
    participants: HashMap<usize, InstanceInfo>,
    client: Client,
    score_interval: Interval,
    messages_gen: Generator,
}

impl Network {
    fn new(
        keypair: Keypair,
        node_id: usize,
        instance_info: InstanceInfo,
        participants: HashMap<usize, InstanceInfo>,
        client: Client,
        validator_set: HashSet<ValId>,
        params: Params,
    ) -> Self {
        let gossipsub = {
            let gossipsub_config = GossipsubConfigBuilder::default()
                .prune_backoff(Duration::from_secs(60))
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

        let genesis_slot = 0;
        let genesis_duration = Duration::ZERO;
        let slot_duration = Duration::from_secs(6);
        let slots_per_epoch = 2;
        let sync_subnet_size = 2;
        let sync_committee_subnets = 2;
        let target_aggregators = 14;
        let attestation_subnets = 4;

        let messages_gen = Generator::builder()
            .slot_clock(genesis_slot, genesis_duration, slot_duration)
            .slots_per_epoch(slots_per_epoch)
            .sync_subnet_size(sync_subnet_size)
            .sync_committee_subnets(sync_committee_subnets)
            .total_validators(params.total_validators() as u64)
            .target_aggregators(target_aggregators)
            .attestation_subnets(attestation_subnets)
            .build(validator_set)
            .expect("need to adjust these params");

        Network {
            swarm,
            node_id,
            instance_info,
            participants,
            client,
            score_interval: interval(Duration::from_secs(1)),
            messages_gen,
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

    async fn run_sim(&mut self, run_duration: Duration) {
        let deadline = tokio::time::sleep(run_duration);
        futures::pin_mut!(deadline);
        loop {
            tokio::select! {
                _ = deadline.as_mut() => {
                    // Sim complete
                    break;
                }
                Some(m) = self.messages_gen.next() => {
                    let payload = m.payload();
                    match m {
                        Message::BeaconBlock { proposer: ValId(v) } => {
                            // publish
                        },
                        Message::AggregateAndProofAttestation { aggregator: ValId(v), subnet: Subnet(s) } => {
                            // publish
                        },
                        Message::Attestation { attester: ValId(v), subnet: Subnet(s) } => {
                            // publish
                        },
                        Message::SignedContributionAndProof { validator: ValId(v), subnet: Subnet(s) } => {
                            // publish
                        },
                        Message::SyncCommitteeMessage { validator: ValId(v), subnet: Subnet(s) } => {
                            // publish
                        },
                    }

                }
                // Record peer scores
                _ = self.score_interval.tick() => {
                    // self.record_peer_scores().await;
                }
                event = self.swarm.select_next_some() => {
                    debug!("SwarmEvent: {:?}", event);
                }
            }
        }
    }
}
