use crate::utils::{record_instance_info, BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY};
use crate::InstanceInfo;
use chrono::{DateTime, Utc};
use futures::stream::FuturesUnordered;
use gen_topology::Params;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::metrics::Config;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, GossipsubMessage, IdentityTransform, MessageAuthenticity,
    MessageId, PeerScoreParams, PeerScoreThresholds, ValidationMode,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::PeerId;
use libp2p::Transport;
use npg::slot_generator::{Subnet, ValId};
use npg::Generator;
use npg::Message;
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use testground::client::Client;
use tokio::time::interval;
use tokio_util::time::DelayQueue;
use tracing::{error, info};

use super::metrics;
use super::Network;
use super::Topic;

pub const ATTESTATION_SUBNETS: u64 = 4;
pub const SYNC_SUBNETS: u64 = 4;
const SLOTS_PER_EPOCH: u64 = 2;
const SLOT_DURATION: u64 = 12;

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
        .parse::<usize>()
        .map_err(|e| format!("Error reading total_validators {}", e))?;
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
        total_nodes_without_vals,
        min_peers_per_node,
        max_peers_per_node_inclusive,
    )?;

    Ok((Duration::from_secs(run), params))
}

// Sets up the gossipsub configuration to be used in the simulation.
pub fn setup_gossipsub(registry: &mut Registry<Box<dyn EncodeMetric>>) -> Gossipsub {
    let gossip_message_id = move |message: &GossipsubMessage| {
        MessageId::from(
            &Sha256::digest([message.topic.as_str().as_bytes(), &message.data].concat())[..20],
        )
    };

    let gossipsub_config = GossipsubConfigBuilder::default()
        .max_transmit_size(10 * 1_048_576) // gossip_max_size(true)
        // .heartbeat_interval(Duration::from_secs(1))
        .prune_backoff(Duration::from_secs(60))
        .mesh_n(8)
        .mesh_n_low(4)
        .mesh_n_high(12)
        .gossip_lazy(6)
        .fanout_ttl(Duration::from_secs(60))
        .history_length(12)
        .max_messages_per_rpc(Some(500)) // Responses to IWANT can be quite large
        .history_gossip(3)
        .validate_messages() // Used for artificial validation delays
        .validation_mode(ValidationMode::Anonymous)
        .duplicate_cache_time(Duration::from_secs(SLOT_DURATION * SLOTS_PER_EPOCH + 1))
        .message_id_fn(gossip_message_id)
        .allow_self_origin(true)
        .build()
        .expect("valid gossipsub configuration");

    let mut gs = Gossipsub::new_with_subscription_filter_and_transform(
        MessageAuthenticity::Anonymous,
        gossipsub_config,
        Some((registry, Config::default())),
        AllowAllSubscriptionFilter {},
        IdentityTransform {},
    )
    .expect("Valid configuration");

    // Setup the scoring system.
    let peer_score_params = PeerScoreParams::default();
    gs.with_peer_score(peer_score_params, PeerScoreThresholds::default())
        .expect("Valid score params and thresholds");

    gs
}

/// The main entry point of the sim.
pub(crate) async fn run(
    client: Client,
    node_id: usize,
    instance_info: InstanceInfo,
    participants: HashMap<usize, InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    let test_instance_count = client.run_parameters().test_instance_count;
    let (run_duration, params) = parse_params(
        test_instance_count as usize,
        client.run_parameters().test_instance_params,
    )?;

    let (params, outbound_peers, validator_assignments) =
        gen_topology::Network::generate(params)?.destructure();

    info!(
        "Running with params {params:?} and {} participants",
        participants.len()
    );
    let validator_set = validator_assignments
        .get(&node_id)
        .cloned()
        .unwrap_or_default();
    info!("[{}] Validators on this node: {:?}", node_id, validator_set);
    let validator_set: HashSet<ValId> =
        validator_set.into_iter().map(|v| ValId(v as u64)).collect();

    record_instance_info(
        &client,
        node_id,
        &instance_info.peer_id,
        &client.run_parameters().test_run,
    )
    .await?;

    let registry: Registry<Box<dyn EncodeMetric>> = Registry::default();
    let mut network = Network::new(
        registry,
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

    if let Err(e) = network.subscribe_topics() {
        error!("[{}] Failed to subscribe to topics {e}", network.node_id);
    };
    network.run_sim(run_duration).await;

    client.record_success().await?;
    Ok(())
}

/// Set up an encrypted TCP transport over the Mplex and Yamux protocols.
pub fn build_transport(
    keypair: &Keypair,
) -> libp2p::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
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

impl Network {
    // Sets up initial conditions and configuration
    pub(crate) fn new(
        mut registry: Registry<Box<dyn EncodeMetric>>,
        keypair: Keypair,
        node_id: usize,
        instance_info: InstanceInfo,
        participants: HashMap<usize, InstanceInfo>,
        client: Client,
        validator_set: HashSet<ValId>,
        params: Params,
    ) -> Self {
        let gossipsub = setup_gossipsub(&mut registry);

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
        let slot_duration = Duration::from_secs(SLOT_DURATION);
        let slots_per_epoch = SLOTS_PER_EPOCH;
        let sync_subnet_size = 2;
        let target_aggregators = 14;

        let messages_gen = Generator::builder()
            .slot_clock(genesis_slot, genesis_duration, slot_duration)
            .slots_per_epoch(slots_per_epoch)
            .sync_subnet_size(sync_subnet_size)
            .sync_committee_subnets(SYNC_SUBNETS)
            .total_validators(params.total_validators() as u64)
            .target_aggregators(target_aggregators)
            .attestation_subnets(ATTESTATION_SUBNETS)
            .build(validator_set)
            .expect("need to adjust these params");

        let start_time: DateTime<Utc> =
            DateTime::parse_from_rfc3339(&client.run_parameters().test_start_time)
                .expect("Correct time date format from testground")
                .into();
        let local_start_time = Instant::now();

        Network {
            swarm,
            node_id,
            instance_info,
            participants,
            client: Arc::new(client),
            metrics_interval: interval(slot_duration / 3),
            messages_gen,
            start_time,
            local_start_time,
            registry,
            influx_db_handles: FuturesUnordered::new(),
            messages_to_validate: DelayQueue::with_capacity(500),
        }
    }

    // Starts libp2p
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

    // Main execution loop of the sim.
    async fn run_sim(&mut self, run_duration: Duration) {
        let deadline = tokio::time::sleep(run_duration);
        futures::pin_mut!(deadline);

        // Initialise some metrics
        // This just adds the 0 value to some counters.

        self.influx_db_handles
            .push(tokio::spawn(metrics::initialise_metrics(
                self.record_metrics_info(),
            )));

        loop {
            tokio::select! {
            _ = deadline.as_mut() => {
                // Sim complete
                break;
            }
            Some(m) = self.messages_gen.next() => {
                let payload = m.payload();
                let (topic, val) = match m {
                    Message::BeaconBlock { proposer: ValId(v), slot: _ } => {
                        (Topic::Blocks, v)

                    },
                    Message::AggregateAndProofAttestation { aggregator: ValId(v), subnet: Subnet(_s), slot: _ } => {
                        (Topic::Aggregates, v)
                    },
                    Message::Attestation { attester: ValId(v), subnet: Subnet(s), slot: _ } => {
                        (Topic::Attestations(s), v)
                    },
                    Message::SignedContributionAndProof { validator: ValId(v), subnet: Subnet(s), slot: _ } => {
                        (Topic::SignedContributionAndProof(s), v)
                    },
                    Message::SyncCommitteeMessage { validator: ValId(v), subnet: Subnet(s), slot: _ } => {
                        (Topic::SyncMessages(s), v)
                    },
                };
                if let Err(e) = self.publish(topic.clone(), val, payload) {
                    error!("Failed to publish message {e} to topic {topic:?}");
                }

            }
            // Record peer scores
            _ = self.metrics_interval.tick() => {
                let metrics_info = self.record_metrics_info();
                // Spawn into its own task
                self.influx_db_handles.push(tokio::spawn(metrics::record_metrics(metrics_info)));
            },
            _ = self.influx_db_handles.next() => {} // Remove excess db handles
            event = self.swarm.select_next_some() => self.handle_swarm_event(event),
            }
        }

        // Waiting for influx db handles to end
        while let Some(_) = self.influx_db_handles.next().await {}
    }
}
