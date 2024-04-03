use crate::param::{build_peer_score_params, parse_peer_score_thresholds, parse_topology_params};
use crate::publish_and_collect;
use crate::topic::Topic;
use crate::utils::{
    queries_for_counter, queries_for_counter_join, record_run_id, record_topology_beacon_node,
    record_topology_edge, BARRIER_LIBP2P_READY, BARRIER_SIMULATION_COMPLETED,
    BARRIER_TOPOLOGY_READY, TAG_PEER_ID, TAG_RUN_ID,
};
use chrono::TimeZone;
use chrono::{DateTime, Local};
use gen_topology::Params;
use gossipsub::{
    AllowAllSubscriptionFilter, Behaviour, ConfigBuilder, Event, IdentTopic, IdentityTransform,
    Message as GossipsubMessage, MessageAuthenticity, MessageId, MetricsConfig, PublishError,
    ValidationMode,
};
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::SwarmEvent;
use libp2p::{noise, yamux, Multiaddr, SwarmBuilder, Transport};
use libp2p::{PeerId, Swarm};
use npg::slot_generator::Subnet;
use npg::slot_generator::ValId;
use npg::{Generator, Message};
use prometheus_client::encoding::protobuf::openmetrics_data_model::MetricSet;
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slot_clock::SlotClock;
use slot_clock::SystemTimeSlotClock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use testground::client::Client;
use testground::{Timestamp, WriteQuery};
use tokio::time::{interval, Interval};
use tracing::{debug, error, info, warn};
use types::{Epoch, Slot};

pub(crate) const ATTESTATION_SUBNETS: u64 = 4;
pub(crate) const SYNC_SUBNETS: u64 = 4;

pub(crate) const SLOT: u64 = 12;
pub(crate) const SLOTS_PER_EPOCH: u64 = 32;

/// The backoff time for pruned peers.
pub(crate) const PRUNE_BACKOFF: u64 = 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct BeaconNodeInfo {
    peer_id: PeerId,
    multiaddr: Multiaddr,
    validators: Vec<u64>,
}

impl BeaconNodeInfo {
    pub(crate) fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub(crate) fn multiaddr(&self) -> &Multiaddr {
        &self.multiaddr
    }

    pub(crate) fn validators(&self) -> &Vec<u64> {
        &self.validators
    }
}

pub(crate) async fn run(client: Client) -> Result<(), Box<dyn std::error::Error>> {
    // The network definition starts at 0 and the testground sequences start at 1, so adjust
    // accordingly.
    let node_id = client.group_seq() as usize - 1;
    let keypair = Keypair::generate_ed25519();
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

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Parse parameters and generate network topology
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let (run_duration, params) = parse_topology_params(
        client.run_parameters().test_group_instance_count as usize, // NOTE: `test_group_instance_count`
        client.run_parameters().test_instance_params,
    )?;

    let (params, outbound_peers, validator_set) = {
        let (params, outbound_peers, validator_assignments) =
            gen_topology::Network::generate(params)?.destructure();

        let validator_set: HashSet<ValId> = validator_assignments
            .get(&node_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|v| ValId(v as u64))
            .collect();

        (
            params,
            outbound_peers
                .get(&node_id)
                .map_or(vec![], |node_ids| node_ids.clone()),
            validator_set,
        )
    };

    let beacon_node_info = BeaconNodeInfo {
        peer_id: PeerId::from(keypair.public()),
        multiaddr,
        validators: validator_set.iter().map(|v| v.0).collect::<Vec<_>>(),
    };

    info!("BeaconNodeInfo: {:?}", beacon_node_info);
    record_topology_beacon_node(&client, &beacon_node_info).await;
    record_run_id(&client).await;

    let participants = {
        let infos = publish_and_collect(
            "beacon_node_info",
            &client,
            (node_id, beacon_node_info.clone()),
            client.run_parameters().test_group_instance_count as usize,
        )
        .await?;
        infos
            .into_iter()
            .filter(|(other_node_id, _)| *other_node_id != node_id)
            .collect::<HashMap<usize, BeaconNodeInfo>>()
    };

    let attackers = collect_attacker_info(&client).await?;

    info!(
        "Running with params {params:?} and {} participants",
        participants.len()
    );

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Start libp2p and dial the designated outbound peers
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let mut registry = Registry::default();
    registry.sub_registry_with_prefix("gossipsub");
    let mut network = Network::new(
        &mut registry,
        keypair,
        node_id,
        beacon_node_info.clone(),
        participants.clone(),
        attackers,
        client.clone(),
        validator_set,
        params,
    );

    // Set up the listening address
    network.start_libp2p().await;

    client
        .signal_and_wait(
            BARRIER_LIBP2P_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

    // Dial the designated outbound peers
    network.dial_peers(&outbound_peers).await;

    client
        .signal_and_wait(
            BARRIER_TOPOLOGY_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

    if let Err(e) = network.subscribe_topics() {
        error!("[{}] Failed to subscribe to topics {e}", network.node_id);
        return Err(e);
    };

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Run simulation
    // /////////////////////////////////////////////////////////////////////////////////////////////
    network.run_sim(run_duration, &registry).await;

    client
        .signal_and_wait(
            BARRIER_SIMULATION_COMPLETED,
            client.run_parameters().test_instance_count,
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

async fn collect_attacker_info(client: &Client) -> Result<Vec<PeerId>, Box<dyn std::error::Error>> {
    let num_attacker = (client.run_parameters().test_instance_count
        - client.run_parameters().test_group_instance_count) as usize;

    let mut stream = client.subscribe("attacker_info", num_attacker * 2).await;

    let mut attackers = vec![];

    for _ in 0..num_attacker {
        match stream.next().await {
            Some(Ok(value)) => {
                let peer_id: PeerId = serde_json::from_value(value)?;
                attackers.push(peer_id);
            }
            Some(Err(e)) => return Err(Box::new(e)),
            None => unreachable!(),
        }
    }

    Ok(attackers)
}

// A context struct for passing information into the `record_peer_scores` function that can be spawned
// into its own task.
struct RecordPeerScoresInfo {
    client: Arc<Client>,
    scores: Vec<(PeerId, f64)>,
    attackers: Arc<Vec<PeerId>>,
    peer_id: PeerId,
    current: DateTime<Local>,
}

// A context struct for passing information into the `record_metrics` function that can be spawned
// into its own task.
struct RecordMetricsInfo {
    client: Arc<Client>,
    metrics: MetricSet,
    peer_id: PeerId,
    current: DateTime<Local>,
}

pub(crate) struct Network {
    swarm: Swarm<Behaviour>,
    node_id: usize,
    beacon_node_info: BeaconNodeInfo,
    participants: HashMap<usize, BeaconNodeInfo>,
    attackers: Arc<Vec<PeerId>>,
    client: Arc<Client>,
    score_interval: Interval,
    messages_gen: Generator,
    received_beacon_blocks: HashMap<Epoch, HashSet<Slot>>,
    received_aggregates: Vec<HashMap<Epoch, Vec<ValId>>>,
    received_attestations: Vec<HashMap<Epoch, HashSet<ValId>>>,
    received_sync_committee_aggregates: Vec<HashMap<Epoch, Vec<ValId>>>,
    received_sync_committee_messages: Vec<HashMap<Epoch, Vec<ValId>>>,
    slot_clock: SystemTimeSlotClock,
}

impl Network {
    #[allow(clippy::too_many_arguments)]
    fn new(
        registry: &mut Registry,
        keypair: Keypair,
        node_id: usize,
        beacon_node_info: BeaconNodeInfo,
        participants: HashMap<usize, BeaconNodeInfo>,
        attackers: Vec<PeerId>,
        client: Client,
        validator_set: HashSet<ValId>,
        params: Params,
    ) -> Self {
        let gossipsub = {
            let gossip_message_id = move |message: &GossipsubMessage| {
                MessageId::from(
                    &Sha256::digest([message.topic.as_str().as_bytes(), &message.data].concat())
                        [..20],
                )
            };

            let gossipsub_config = ConfigBuilder::default()
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

            let mut gs = Behaviour::new_with_subscription_filter_and_transform(
                MessageAuthenticity::Anonymous,
                gossipsub_config,
                Some((registry, MetricsConfig::default())),
                AllowAllSubscriptionFilter {},
                IdentityTransform {},
            )
            .expect("Valid configuration");

            // Setup the scoring system.
            let instance_params = client.run_parameters().test_instance_params;
            gs.with_peer_score(
                build_peer_score_params(&instance_params),
                parse_peer_score_thresholds(&instance_params).expect("Valid peer score thresholds"),
            )
            .expect("Valid score params and thresholds");

            gs
        };

        let transport = build_transport(&keypair);
        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_other_transport(|_| transport)
            .expect("infallible")
            .with_behaviour(|_| gossipsub)
            .expect("infallible")
            .build();

        info!(
            "[{}] running with {} validators",
            node_id,
            validator_set.len()
        );

        let genesis_slot = 0;
        let genesis_duration = Duration::ZERO;
        let slot_duration = Duration::from_secs(SLOT);
        let sync_subnet_size = 2;
        let target_aggregators = 14;

        let messages_gen = Generator::builder()
            .slot_clock(genesis_slot, genesis_duration, slot_duration)
            .slots_per_epoch(SLOTS_PER_EPOCH)
            .sync_subnet_size(sync_subnet_size)
            .sync_committee_subnets(SYNC_SUBNETS)
            .total_validators(params.total_validators() as u64)
            .target_aggregators(target_aggregators)
            .attestation_subnets(ATTESTATION_SUBNETS)
            .build(validator_set)
            .expect("need to adjust these params");

        let mut received_aggregates = Vec::with_capacity(ATTESTATION_SUBNETS as usize);
        let mut received_attestations = Vec::with_capacity(ATTESTATION_SUBNETS as usize);
        for subnet_id in 0..ATTESTATION_SUBNETS {
            received_aggregates.insert(subnet_id as usize, HashMap::new());
            received_attestations.insert(subnet_id as usize, HashMap::new());
        }

        let mut received_sync_committee_aggregates = Vec::with_capacity(SYNC_SUBNETS as usize);
        let mut received_sync_committee_messages = Vec::with_capacity(SYNC_SUBNETS as usize);
        for subnet_id in 0..SYNC_SUBNETS {
            received_sync_committee_aggregates.insert(subnet_id as usize, HashMap::new());
            received_sync_committee_messages.insert(subnet_id as usize, HashMap::new());
        }

        Network {
            swarm,
            node_id,
            beacon_node_info,
            participants,
            attackers: Arc::new(attackers),
            client: Arc::new(client),
            score_interval: interval(Duration::from_secs(3)),
            messages_gen,
            received_beacon_blocks: HashMap::new(),
            received_aggregates,
            received_attestations,
            received_sync_committee_aggregates,
            received_sync_committee_messages,
            slot_clock: SystemTimeSlotClock::new(
                Slot::new(genesis_slot),
                genesis_duration,
                slot_duration,
            ),
        }
    }

    async fn start_libp2p(&mut self) {
        self.swarm
            .listen_on(self.beacon_node_info.multiaddr.clone())
            .expect("Swarm starts listening");

        match self.swarm.next().await.unwrap() {
            SwarmEvent::NewListenAddr { address, .. } => {
                assert_eq!(address, self.beacon_node_info.multiaddr)
            }
            e => panic!("Unexpected event {:?}", e),
        };
    }

    pub async fn dial_peers(&mut self, outbound_peers: &Vec<usize>) {
        let mut dialed_peers = 0;

        for peer_node_id in outbound_peers {
            let BeaconNodeInfo { peer_id, multiaddr, .. } = self
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

            record_topology_edge(
                &self.client,
                self.beacon_node_info.peer_id.to_string(),
                peer_id.to_string(),
            )
            .await;

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

        info!("[{}] dialed {} peers", self.node_id, dialed_peers);
    }

    pub fn subscribe_topics(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // blocks, attestations and aggregates, sync messages and aggregates
        let blocks_topic: IdentTopic = Topic::Blocks.into();
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

    async fn run_sim(&mut self, run_duration: Duration, registry: &Registry) {
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
                    let (topic, msg) = match m {
                        Message::BeaconBlock { proposer: ValId(v), slot } => {
                            let msg = serde_json::to_vec(&(v, slot, payload)).expect("json serialization never fails");
                            (Topic::Blocks, msg)
                        },
                        Message::AggregateAndProofAttestation { aggregator: ValId(v), subnet: Subnet(s), slot } => {
                            let msg = serde_json::to_vec(&(v, slot, payload)).expect("json serialization never fails");
                            (Topic::Aggregates(s), msg)
                        },
                        Message::Attestation { attester: ValId(v), subnet: Subnet(s), slot } => {
                            let msg = serde_json::to_vec(&(v, slot, payload)).expect("json serialization never fails");
                            (Topic::Attestations(s), msg)
                        },
                        Message::SignedContributionAndProof { validator: ValId(v), subnet: Subnet(s), slot } => {
                            let msg = serde_json::to_vec(&(v, slot, payload)).expect("json serialization never fails");
                            (Topic::SignedContributionAndProof(s), msg)
                        },
                        Message::SyncCommitteeMessage { validator: ValId(v), subnet: Subnet(s), slot } => {
                            let msg = serde_json::to_vec(&(v, slot, payload)).expect("json serialization never fails");
                            (Topic::SyncMessages(s), msg)
                        },
                    };

                    if let Err(e) = self.publish(topic.clone(), msg) {
                        error!("Failed to publish message {e} to topic {topic:?}");
                    }
                }
                // Record peer scores and gossipsub metrics
                _ = self.score_interval.tick() => {
                    tokio::spawn(record_peer_scores(self.peer_scores_info()));
                    tokio::spawn(record_metrics(self.metrics_info(registry)));
                }
                event = self.swarm.select_next_some() => {
                    match event {
                        SwarmEvent::Behaviour(gossipsub_event) => self.handle_gossipsub_event(gossipsub_event),
                        _ => debug!("SwarmEvent: {:?}", event),
                    }
                }
            }
        }

        info!("The simulation has completed. Recording the results.");
        self.record_received_beacon_blocks().await;
        self.record_received_attestations().await;
        self.record_received_message_count_per_epoch(
            "aggregates",
            ATTESTATION_SUBNETS,
            &self.received_aggregates,
        )
        .await;
        self.record_received_message_count_per_epoch(
            "sync_committee_aggregates",
            SYNC_SUBNETS,
            &self.received_sync_committee_aggregates,
        )
        .await;
        self.record_received_message_count_per_epoch(
            "sync_committee_messages",
            SYNC_SUBNETS,
            &self.received_sync_committee_messages,
        )
        .await;
    }

    fn publish(&mut self, topic: Topic, msg: Vec<u8>) -> Result<MessageId, PublishError> {
        info!("Publish {:?}", topic);
        let ident_topic: IdentTopic = topic.into();
        self.swarm.behaviour_mut().publish(ident_topic, msg)
    }

    fn handle_gossipsub_event(&mut self, event: Event) {
        match event {
            Event::Message {
                propagation_source: _,
                message_id: _,
                message,
            } => {
                let topic: Topic = message.topic.as_str().into();

                match topic {
                    Topic::Blocks => {
                        let (_proposer, slot, _payload): (u64, Slot, String) =
                            serde_json::from_slice(&message.data).unwrap();
                        let epoch = slot.epoch(SLOTS_PER_EPOCH);

                        if !self
                            .received_beacon_blocks
                            .entry(epoch)
                            .or_default()
                            .insert(slot)
                        {
                            warn!("BeaconBlock message on slot {slot} is already received.")
                        }
                    }
                    Topic::Aggregates(subnet_id) => {
                        let (validator, slot, _payload): (u64, Slot, String) =
                            serde_json::from_slice(&message.data).unwrap();
                        let epoch = slot.epoch(SLOTS_PER_EPOCH);

                        self.received_aggregates
                            .get_mut(subnet_id as usize)
                            .expect("subnet_id")
                            .entry(epoch)
                            .or_default()
                            .push(ValId(validator));
                    }
                    Topic::Attestations(subnet_id) => {
                        let (validator, slot, _payload): (u64, Slot, String) =
                            serde_json::from_slice(&message.data).unwrap();
                        let epoch = slot.epoch(SLOTS_PER_EPOCH);

                        if !self
                            .received_attestations
                            .get_mut(subnet_id as usize)
                            .expect("subnet_id")
                            .entry(epoch)
                            .or_default()
                            .insert(ValId(validator))
                        {
                            warn!("Attestation message from {validator} is already received.")
                        }
                    }
                    Topic::SignedContributionAndProof(subnet_id) => {
                        let (validator, slot, _payload): (u64, Slot, String) =
                            serde_json::from_slice(&message.data).unwrap();
                        let epoch = slot.epoch(SLOTS_PER_EPOCH);

                        self.received_sync_committee_aggregates
                            .get_mut(subnet_id as usize)
                            .expect("subnet_id")
                            .entry(epoch)
                            .or_default()
                            .push(ValId(validator));
                    }
                    Topic::SyncMessages(subnet_id) => {
                        let (validator, slot, _payload): (u64, Slot, String) =
                            serde_json::from_slice(&message.data).unwrap();
                        let epoch = slot.epoch(SLOTS_PER_EPOCH);

                        self.received_sync_committee_messages
                            .get_mut(subnet_id as usize)
                            .expect("subnet_id")
                            .entry(epoch)
                            .or_default()
                            .push(ValId(validator));
                    }
                }
            }
            other => info!("GossipsubEvent: {:?}", other),
        }
    }

    /// Record the number of BeaconBlock messages received per epoch.
    async fn record_received_beacon_blocks(&self) {
        let mut queries = vec![];
        let run_id = self.client.run_parameters().test_run;
        let measurement = format!("{}_beacon_block", env!("CARGO_PKG_NAME"));

        for (epoch, slots) in self.received_beacon_blocks.iter() {
            let timestamp: Timestamp = {
                let duration = self
                    .slot_clock
                    .start_of(epoch.start_slot(SLOTS_PER_EPOCH))
                    .unwrap();

                Local
                    .timestamp_opt(duration.as_secs() as i64, 0)
                    .single()
                    .expect("datetime")
                    .into()
            };

            let query = WriteQuery::new(timestamp, &measurement)
                .add_tag(TAG_PEER_ID, self.beacon_node_info.peer_id.to_string())
                .add_tag(TAG_RUN_ID, run_id.to_owned())
                .add_tag("epoch", epoch.as_u64())
                .add_field("count", slots.len() as u64);
            queries.push(query);
        }

        for query in queries {
            if let Err(e) = self.client.record_metric(query).await {
                error!("Failed to record received_beacon_blocks: {e:?}");
            }
        }
    }

    /// Record the number of Attestations received per epoch.
    async fn record_received_attestations(&self) {
        let mut queries = vec![];
        let run_id = self.client.run_parameters().test_run;

        for subnet_id in 0..ATTESTATION_SUBNETS as usize {
            let measurement = format!("{}_attestations_{subnet_id}", env!("CARGO_PKG_NAME"));

            for (epoch, vals) in self
                .received_attestations
                .get(subnet_id)
                .expect("subnet_id")
                .iter()
            {
                let timestamp: Timestamp = {
                    let duration = self
                        .slot_clock
                        .start_of(epoch.start_slot(SLOTS_PER_EPOCH))
                        .unwrap();

                    Local
                        .timestamp_opt(duration.as_secs() as i64, 0)
                        .single()
                        .expect("datetime")
                        .into()
                };

                let query = WriteQuery::new(timestamp, measurement.as_str())
                    .add_tag(TAG_PEER_ID, self.beacon_node_info.peer_id.to_string())
                    .add_tag(TAG_RUN_ID, run_id.to_owned())
                    .add_tag("epoch", epoch.as_u64())
                    .add_field("count", vals.len() as u64);
                queries.push(query);
            }
        }

        for query in queries {
            if let Err(e) = self.client.record_metric(query).await {
                error!("Failed to record received_attestations: {e:?}");
            }
        }
    }

    /// Record the number of messages received per epoch.
    async fn record_received_message_count_per_epoch(
        &self,
        measurement: &str,
        subnets: u64,
        messages: &[HashMap<Epoch, Vec<ValId>>],
    ) {
        let mut queries = vec![];
        let run_id = self.client.run_parameters().test_run;

        for subnet_id in 0..subnets as usize {
            let measurement = format!("{}_{measurement}_{subnet_id}", env!("CARGO_PKG_NAME"));

            for (epoch, vals) in messages.get(subnet_id).expect("subnet_id").iter() {
                let timestamp: Timestamp = {
                    let duration = self
                        .slot_clock
                        .start_of(epoch.start_slot(SLOTS_PER_EPOCH))
                        .unwrap();

                    Local
                        .timestamp_opt(duration.as_secs() as i64, 0)
                        .single()
                        .expect("datetime")
                        .into()
                };

                let query = WriteQuery::new(timestamp, measurement.as_str())
                    .add_tag(TAG_PEER_ID, self.beacon_node_info.peer_id.to_string())
                    .add_tag(TAG_RUN_ID, run_id.to_owned())
                    .add_tag("epoch", epoch.as_u64())
                    .add_field("count", vals.len() as u64);
                queries.push(query);
            }
        }

        for query in queries {
            if let Err(e) = self.client.record_metric(query).await {
                error!("Failed to record received aggregates: {e:?}");
            }
        }
    }

    fn peer_scores_info(&self) -> RecordPeerScoresInfo {
        let gossipsub = self.swarm.behaviour();
        let scores = gossipsub
            .all_peers()
            .filter_map(|(peer, _)| gossipsub.peer_score(peer).map(|score| (*peer, score)))
            .collect::<Vec<_>>();

        RecordPeerScoresInfo {
            client: self.client.clone(),
            scores,
            attackers: self.attackers.clone(),
            peer_id: self.beacon_node_info.peer_id,
            current: Local::now(),
        }
    }

    fn metrics_info(&self, registry: &Registry) -> RecordMetricsInfo {
        RecordMetricsInfo {
            client: self.client.clone(),
            metrics: prometheus_client::encoding::protobuf::encode(registry).unwrap(),
            peer_id: self.beacon_node_info.peer_id,
            current: Local::now(),
        }
    }
}

/// Record peer scores
async fn record_peer_scores(peer_scores_info: RecordPeerScoresInfo) {
    if peer_scores_info.scores.is_empty() {
        return;
    }

    let measurement = format!("{}_scores", env!("CARGO_PKG_NAME"));

    let mut query = WriteQuery::new(peer_scores_info.current.into(), measurement)
        .add_tag(TAG_PEER_ID, peer_scores_info.peer_id.to_string())
        .add_tag(
            TAG_RUN_ID,
            peer_scores_info.client.run_parameters().test_run,
        );

    for (peer, score) in peer_scores_info.scores {
        let field = if peer_scores_info.attackers.contains(&peer) {
            format!("attacker_{}", peer)
        } else {
            peer.to_string()
        };
        query = query.add_field(field, score);
    }

    if let Err(e) = peer_scores_info.client.record_metric(query).await {
        warn!("Failed to record score: {e:?}");
    }
}

/// Record metrics
async fn record_metrics(metrics_info: RecordMetricsInfo) {
    let run_id = metrics_info.client.run_parameters().test_run;
    let mut queries = vec![];

    for family in metrics_info.metrics.metric_families.iter() {
        let q = match family.name.as_str() {
            // ///////////////////////////////////
            // Metrics per known topic
            // ///////////////////////////////////
            "topic_subscription_status" => {
                continue;
            }
            "topic_peers_counts" => {
                continue;
            }
            "invalid_messages_per_topic"
            | "accepted_messages_per_topic"
            | "ignored_messages_per_topic"
            | "rejected_messages_per_topic" => queries_for_counter(
                &metrics_info.current,
                family,
                &metrics_info.peer_id,
                &run_id,
            ),
            "publish_messages_dropped_per_topic" | "forward_messages_dropped_per_topic" => {
                continue;
            }
            // ///////////////////////////////////
            // Metrics regarding mesh state
            // ///////////////////////////////////
            "mesh_peer_counts" => {
                continue;
            }
            "mesh_peer_inclusion_events" => queries_for_counter(
                &metrics_info.current,
                family,
                &metrics_info.peer_id,
                &run_id,
            ),
            "mesh_peer_churn_events" => queries_for_counter(
                &metrics_info.current,
                family,
                &metrics_info.peer_id,
                &run_id,
            ),
            // ///////////////////////////////////
            // Metrics regarding messages sent/received
            // ///////////////////////////////////
            "topic_msg_sent_counts"
            | "topic_msg_published"
            | "topic_msg_sent_bytes"
            | "topic_msg_recv_counts_unfiltered"
            | "topic_msg_recv_counts"
            | "topic_msg_recv_bytes" => queries_for_counter(
                &metrics_info.current,
                family,
                &metrics_info.peer_id,
                &run_id,
            ),
            // ///////////////////////////////////
            // Metrics related to scoring
            // ///////////////////////////////////
            "score_per_mesh" => {
                continue;
            }
            "scoring_penalties" => queries_for_counter(
                &metrics_info.current,
                family,
                &metrics_info.peer_id,
                &run_id,
            ),
            // ///////////////////////////////////
            // General Metrics
            // ///////////////////////////////////
            "peers_per_protocol" => {
                continue;
            }
            "heartbeat_duration" => {
                continue;
            }
            // ///////////////////////////////////
            // Performance metrics
            // ///////////////////////////////////
            "topic_iwant_msgs" => queries_for_counter(
                &metrics_info.current,
                family,
                &metrics_info.peer_id,
                &run_id,
            ),
            "memcache_misses" => queries_for_counter(
                &metrics_info.current,
                family,
                &metrics_info.peer_id,
                &run_id,
            ),
            // ///////////////////////////////////
            // The size of the priority queue.
            // ///////////////////////////////////
            "priority_queue_size" | "non_priority_queue_size" => {
                continue;
            }
            _ => unreachable!(),
        };
        queries.extend(q);
    }

    // We can't do joins in InfluxDB easily, so do some custom queries here to calculate
    // duplicates.
    let recvd_unfiltered = metrics_info
        .metrics
        .metric_families
        .iter()
        .find(|family| family.name.as_str() == "topic_msg_recv_counts_unfiltered");

    if let Some(recvd_unfiltered) = recvd_unfiltered {
        let recvd = metrics_info
            .metrics
            .metric_families
            .iter()
            .find(|family| family.name.as_str() == "topic_msg_recv_counts");
        if let Some(recvd) = recvd {
            let q = queries_for_counter_join(
                &metrics_info.current,
                recvd_unfiltered,
                recvd,
                "topic_msg_recv_counts_duplicates",
                &metrics_info.peer_id,
                &run_id,
                |a, b| a.saturating_sub(b),
            );

            queries.extend(q);
        }
    }

    for q in queries {
        if let Err(e) = metrics_info.client.record_metric(q).await {
            error!("Failed to record metrics: {e:?}");
        }
    }
}
