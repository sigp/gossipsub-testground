use crate::utils::{record_instance_info, BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY};
use crate::InstanceInfo;
use chrono::{DateTime, Utc};
use futures::stream::FuturesUnordered;
use gen_topology::Params;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::futures::StreamExt;
use libp2p::gossipsub::metrics::Config;
use libp2p::gossipsub::{
    Gossipsub, GossipsubBuilder, GossipsubConfigBuilder, GossipsubMessage, MessageAuthenticity,
    MessageId, PeerScoreParams, PeerScoreThresholds, ValidationMode,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::tcp::tokio::Transport as TokioTransport;
use libp2p::tcp::Config as GenTcpConfig;
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
use tracing::{error, info, warn};
use rand::SeedableRng;

use super::metrics;
use super::Network;
use super::Topic;

pub const ATTESTATION_SUBNETS: u64 = 4;
pub const SYNC_SUBNETS: u64 = 4;
const TARGET_AGGREGATORS: u64 = 14;
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
    let episub = instance_params
        .get("episub")
        .ok_or("episub is not specified.")?
        .parse::<bool>()?;

    let params = Params::new(
        seed,
        total_validators,
        total_nodes_with_vals,
        total_nodes_without_vals,
        min_peers_per_node,
        max_peers_per_node_inclusive,
        episub,
    )?;

    Ok((Duration::from_secs(run), params))
}

// Sets up the gossipsub configuration to be used in the simulation.
pub fn setup_gossipsub(registry: &mut Registry<Box<dyn EncodeMetric>>, use_episub: bool) -> Gossipsub {
    let gossip_message_id = move |message: &GossipsubMessage| {
        MessageId::from(
            &Sha256::digest([message.topic.as_str().as_bytes(), &message.data].concat())[..20],
        )
    };

    let mut gossipsub_config = GossipsubConfigBuilder::default()
        .max_transmit_size(10 * 1_048_576) // gossip_max_size(true)
        .heartbeat_interval(Duration::from_secs(1))
        .prune_backoff(Duration::from_secs(60))
        //.mesh_n(8)
        .mesh_n(2)
        .mesh_outbound_min(1)
        //.mesh_n_low(4)
        .mesh_n_low(1)
        //.mesh_n_high(12)
        .mesh_n_high(5)
        .gossip_lazy(6)
        .fanout_ttl(Duration::from_secs(60))
        .history_length(12)
        .max_messages_per_rpc(Some(500)) // Responses to IWANT can be quite large
        .history_gossip(3)
        .validate_messages() // Used for artificial validation delays
        .duplicate_cache_time(Duration::from_secs(SLOT_DURATION * SLOTS_PER_EPOCH + 1))
        .message_id_fn(gossip_message_id)
        .allow_self_origin(true)
        .episub_heartbeat_ticks(24); // small amount for testing

    if !use_episub {
        gossipsub_config = gossipsub_config
            .disable_episub();
    } else {
        // construct a choking strategy
        let default_strat = libp2p::gossipsub::DefaultStratBuilder::new().min_choke_message_count(2).build();
        gossipsub_config = gossipsub_config.choking_strategy(default_strat);
    }

    let gossipsub_config = gossipsub_config
        .build()
        .expect("valid gossipsub configuration");

    //let mut gs = GossipsubBuilder::new(MessageAuthenticity::Anonymous)
    GossipsubBuilder::new(MessageAuthenticity::Anonymous)
        .config(gossipsub_config)
        .validation_mode(ValidationMode::Anonymous)
        .metrics(registry, Config::default())
        .build()
        .expect("Correct gossipsub configuration")

    /* Ignore Scoring for now
    // Setup the scoring system.
    let peer_score_params = PeerScoreParams::default();
    gs.with_peer_score(peer_score_params, PeerScoreThresholds::default())
        .expect("Valid score params and thresholds");

    gs
    */ 

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
    let transport = libp2p::dns::TokioDnsConfig::system(TokioTransport::new(
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
    #[allow(clippy::too_many_arguments)]
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
        let gossipsub = setup_gossipsub(&mut registry, params.episub());

        let swarm = SwarmBuilder::with_tokio_executor(
            build_transport(&keypair),
            gossipsub,
            PeerId::from(keypair.public()),
        )
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

        let messages_gen = Generator::builder()
            .slot_clock(genesis_slot, genesis_duration, slot_duration)
            .slots_per_epoch(slots_per_epoch)
            .sync_subnet_size(SYNC_SUBNETS)
            .sync_committee_subnets(SYNC_SUBNETS)
            .total_validators(params.total_validators() as u64)
            .target_aggregators(TARGET_AGGREGATORS)
            .attestation_subnets(ATTESTATION_SUBNETS)
            .build(validator_set)
            .expect("need to adjust these params");

        let start_time: DateTime<Utc> =
            DateTime::parse_from_rfc3339(&client.run_parameters().test_start_time)
                .expect("Correct time date format from testground")
                .into();
        let local_start_time = Instant::now();

        let rng = rand::rngs::SmallRng::seed_from_u64(params.seed());

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
            message_arrive_duration: HashMap::with_capacity(64),
            artificial_validation_delay: HashMap::with_capacity(64),
            rng,
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
        let start_instant = Instant::now();
        futures::pin_mut!(deadline);

        // Initialise some metrics
        // This just adds the 0 value to some counters.
        let metric_info = self.record_metrics_info();

        self.influx_db_handles
            .push(tokio::spawn(metrics::initialise_metrics(metric_info)));

        loop {
            tokio::select! {
                _ = deadline.as_mut() => {
                    // Sim complete
                    break;
                }
                Some(m) = self.messages_gen.next() => {
                    let payload = m.payload(&mut self.rng);
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

                    // Hack to just send blocks
              //      if let Topic::Blocks = topic {
                        if let Err(e) = self.publish(topic.clone(), val, payload) {
                            error!("Failed to publish message {e} to topic {topic:?}");
                        }
               //     }

                }
                // Record peer scores
                _ = self.metrics_interval.tick() => {
                    let metrics_info = self.record_metrics_info();
                    // Spawn into its own task
                    self.influx_db_handles.push(tokio::spawn(metrics::record_metrics(metrics_info)));
                },
                _ = self.influx_db_handles.next() => {} // Remove excess db handles
                event = self.swarm.select_next_some() => self.handle_swarm_event(event),

                Some(x) = self.messages_to_validate.next(), if !self.messages_to_validate.is_empty() => { // Message needs validation
                    let (message_id, peer_id) = x.into_inner();
                    if let Err(e)  = self.swarm.behaviour_mut().report_message_validation_result(&message_id, &peer_id, libp2p::gossipsub::MessageAcceptance::Accept) {
                        warn!("Could not publish message: {} {}", message_id, e);
                    }
                }
            }

            // Sometimes the deadline doesn't fire, so we need to check if we've exceeded the run
            // duration
            if start_instant.elapsed() > run_duration {
                break;
            }
        }

        // Waiting for influx db handles to end
        info!(
            "Waiting for influx db writes: {}",
            self.influx_db_handles.len()
        );
        while (self.influx_db_handles.next().await).is_some() {}
        info!("Completed influx db write");
    }
}
