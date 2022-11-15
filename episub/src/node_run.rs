use crate::utils::{
    queries_for_counter, queries_for_gauge, queries_for_histogram, record_instance_info,
    BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY,
};
use crate::InstanceInfo;
use chrono::{DateTime, Utc};
use gen_topology::Params;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::metrics::Config;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, IdentTopic, IdentityTransform, MessageAuthenticity,
    PeerScoreParams, PeerScoreThresholds, Topic as GossipTopic,
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
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use testground::client::Client;
use tokio::time::{interval, Interval};
use tracing::{debug, error, info};

const ATTESTATION_SUBNETS: u64 = 4;
const SYNC_SUBNETS: u64 = 4;

#[derive(Serialize, Deserialize, Clone, Debug)]
enum Topic {
    Blocks,
    Attestations(u64),
    Aggregates(u64),
    SyncMessages(u64),
    SignedContributionAndProof(u64),
}

impl From<Topic> for IdentTopic {
    fn from(t: Topic) -> Self {
        let rep = serde_json::to_string(&t).expect("json serialization of topics never fails");
        GossipTopic::new(rep)
    }
}

impl From<IdentTopic> for Topic {
    fn from(t: IdentTopic) -> Self {
        let repr = t.hash().into_string();
        serde_json::from_str(&repr).expect("json deserialization of topics never fails")
    }
}

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
        &instance_info,
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
    /// Libp2p2 swarm.
    swarm: Swarm<Gossipsub>,
    /// Node id for this node, local to the test run.
    node_id: usize,
    /// This nodes contact info.
    instance_info: InstanceInfo,
    /// Metrics registry.
    registry: Registry<Box<dyn EncodeMetric>>,
    /// Information of every other participant in the network, indexed by their (local to the test
    /// run) node_id.
    participants: HashMap<usize, InstanceInfo>,
    /// Testground client.
    client: Client,
    /// Chronos time reported by testground as the start of the test run.
    start_time: DateTime<Utc>,
    /// Instant in which the simmulation starts running, according to the local time.
    local_start_time: Instant,
    /// How often metrics are recorded.
    metrics_interval: Interval,
    /// Generator of messages per slot.
    messages_gen: Generator,
}

impl Network {
    fn new(
        mut registry: Registry<Box<dyn EncodeMetric>>,
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
                .max_transmit_size(10 * 1_048_576) // gossip_max_size(true)
                .prune_backoff(Duration::from_secs(60))
                .history_length(12)
                .build()
                .expect("Valid configuration");

            let mut gs = Gossipsub::new_with_subscription_filter_and_transform(
                MessageAuthenticity::Signed(keypair.clone()),
                gossipsub_config,
                Some((&mut registry, Config::default())),
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
        let slot_duration = Duration::from_secs(12);
        let slots_per_epoch = 2;
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
            client,
            metrics_interval: interval(Duration::from_secs(5)),
            messages_gen,
            start_time,
            local_start_time,
            registry,
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
                    .unwrap_or_else(|| {
                        panic!("[{}] All outbound peers are participants of the network {peer_node_id} {:?}", self.node_id,self.participants.keys().collect::<Vec<_>>())
                    })
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
                    let raw_payload = m.payload();
                    let payload = String::from_utf8_lossy(&raw_payload);
                    let (topic, val) = match m {
                        Message::BeaconBlock { proposer: ValId(v) } => {
                            (Topic::Blocks, v)

                        },
                        Message::AggregateAndProofAttestation { aggregator: ValId(v), subnet: Subnet(s) } => {
                            (Topic::Aggregates(s), v)
                        },
                        Message::Attestation { attester: ValId(v), subnet: Subnet(s) } => {
                            (Topic::Attestations(s), v)
                        },
                        Message::SignedContributionAndProof { validator: ValId(v), subnet: Subnet(s) } => {
                            (Topic::SignedContributionAndProof(s), v)
                        },
                        Message::SyncCommitteeMessage { validator: ValId(v), subnet: Subnet(s) } => {
                            (Topic::SyncMessages(s), v)
                        },
                    };
                    if let Err(e) = self.publish(topic.clone(), val, &payload) {
                        error!("Failed to publish message {e} to topic {topic:?}");
                    }

                }
                // Record peer scores
                _ = self.metrics_interval.tick() => {
                    self.record_metrics().await;
                }
                event = self.swarm.select_next_some() => {
                    debug!("SwarmEvent: {:?}", event);
                }
            }
        }
    }

    async fn record_metrics(&self) {
        let run_id = &self.client.run_parameters().test_run;

        // Encode the metrics to an instance of the OpenMetrics protobuf format.
        // https://github.com/OpenObservability/OpenMetrics/blob/main/proto/openmetrics_data_model.proto
        let metric_set = prometheus_client::encoding::proto::encode(&self.registry);

        let mut queries = vec![];

        let elapsed = chrono::Duration::from_std(self.local_start_time.elapsed())
            .expect("Durations are small");
        let current = self.start_time + elapsed;

        for family in metric_set.metric_families.iter() {
            let q = match family.name.as_str() {
                // ///////////////////////////////////
                // Metrics per known topic
                // ///////////////////////////////////
                "topic_subscription_status" => queries_for_gauge(
                    &current,
                    family,
                    self.node_id,
                    &self.instance_info,
                    run_id,
                    "status",
                ),
                "topic_peers_counts" => queries_for_gauge(
                    &current,
                    family,
                    self.node_id,
                    &self.instance_info,
                    run_id,
                    "count",
                ),
                "invalid_messages_per_topic"
                | "accepted_messages_per_topic"
                | "ignored_messages_per_topic"
                | "rejected_messages_per_topic" => {
                    queries_for_counter(&current, family, self.node_id, &self.instance_info, run_id)
                }
                // ///////////////////////////////////
                // Metrics regarding mesh state
                // ///////////////////////////////////
                "mesh_peer_counts" => queries_for_gauge(
                    &current,
                    family,
                    self.node_id,
                    &self.instance_info,
                    run_id,
                    "count",
                ),
                "mesh_peer_inclusion_events" => {
                    queries_for_counter(&current, family, self.node_id, &self.instance_info, run_id)
                }
                "mesh_peer_churn_events" => {
                    queries_for_counter(&current, family, self.node_id, &self.instance_info, run_id)
                }
                // ///////////////////////////////////
                // Metrics regarding messages sent/received
                // ///////////////////////////////////
                "topic_msg_sent_counts"
                | "topic_msg_published"
                | "topic_msg_sent_bytes"
                | "topic_msg_recv_counts_unfiltered"
                | "topic_msg_recv_counts"
                | "topic_msg_recv_bytes" => {
                    queries_for_counter(&current, family, self.node_id, &self.instance_info, run_id)
                }
                // ///////////////////////////////////
                // Metrics related to scoring
                // ///////////////////////////////////
                "score_per_mesh" => queries_for_histogram(
                    &current,
                    family,
                    self.node_id,
                    &self.instance_info,
                    run_id,
                ),
                "scoring_penalties" => {
                    queries_for_counter(&current, family, self.node_id, &self.instance_info, run_id)
                }
                // ///////////////////////////////////
                // General Metrics
                // ///////////////////////////////////
                "peers_per_protocol" => queries_for_gauge(
                    &current,
                    family,
                    self.node_id,
                    &self.instance_info,
                    run_id,
                    "peers",
                ),
                "heartbeat_duration" => queries_for_histogram(
                    &current,
                    family,
                    self.node_id,
                    &self.instance_info,
                    run_id,
                ),
                // ///////////////////////////////////
                // Performance metrics
                // ///////////////////////////////////
                "topic_iwant_msgs" => {
                    queries_for_counter(&current, family, self.node_id, &self.instance_info, run_id)
                }
                "memcache_misses" => {
                    queries_for_counter(&current, family, self.node_id, &self.instance_info, run_id)
                }
                _ => unreachable!(),
            };

            queries.extend(q);
        }

        for query in queries {
            if let Err(e) = self.client.record_metric(query).await {
                error!("Failed to record metrics: {:?}", e);
            }
        }
    }

    fn publish(
        &mut self,
        topic: Topic,
        validator: u64,
        payload: &str,
    ) -> Result<libp2p::gossipsub::MessageId, libp2p::gossipsub::error::PublishError> {
        let ident_topic: IdentTopic = topic.into();
        // simple tuples as messages
        let msg =
            serde_json::to_vec(&(validator, payload)).expect("json serialization never fails");
        self.swarm.behaviour_mut().publish(ident_topic, msg)
    }

    pub fn subscribe_topics(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // blocks, attestations and aggregates, syncc messages and aggregates
        let blocks_topic: IdentTopic =
            GossipTopic::new(serde_json::to_string(&Topic::Blocks).unwrap());
        self.swarm.behaviour_mut().subscribe(&blocks_topic)?;
        for subnet_n in 0..ATTESTATION_SUBNETS {
            let attestation_subnet: IdentTopic = Topic::Attestations(subnet_n).into();
            let aggregate_subnet: IdentTopic = Topic::Aggregates(subnet_n).into();
            self.swarm.behaviour_mut().subscribe(&attestation_subnet)?;
            self.swarm.behaviour_mut().subscribe(&aggregate_subnet)?;
        }

        for subnet_n in 0..SYNC_SUBNETS {
            let sync_subnet: IdentTopic = Topic::SyncMessages(subnet_n).into();
            let sync_aggregates: IdentTopic = Topic::SignedContributionAndProof(subnet_n).into();
            self.swarm.behaviour_mut().subscribe(&sync_subnet)?;
            self.swarm.behaviour_mut().subscribe(&sync_aggregates)?;
        }
        Ok(())
    }
}
