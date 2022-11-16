use crate::params::parse_topology_params;
use crate::publish_and_collect;
use crate::utils::{
    queries_for_counter, BARRIER_LIBP2P_READY, BARRIER_SIMULATION_COMPLETED,
    BARRIER_TOPOLOGY_READY, TAG_PEER_ID, TAG_RUN_ID,
};
use chrono::Local;
use chrono::TimeZone;
use gen_topology::Params;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::metrics::Config;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, GossipsubEvent, IdentTopic, IdentityTransform,
    MessageAuthenticity, PeerScoreParams, PeerScoreThresholds, Topic as GossipTopic, TopicHash,
    TopicScoreParams,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::multiaddr::Protocol;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::{Multiaddr, Transport};
use libp2p::{PeerId, Swarm};
use npg::slot_generator::Subnet;
use npg::slot_generator::ValId;
use npg::{Generator, Message};
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use slot_clock::SlotClock;
use slot_clock::SystemTimeSlotClock;
use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use testground::client::Client;
use testground::{Timestamp, WriteQuery};
use tokio::time::{interval, Interval};
use tracing::{debug, error, info, warn};
use types::{Epoch, Slot};

const ATTESTATION_SUBNETS: u64 = 4;
const SYNC_SUBNETS: u64 = 4;

const SLOT: u64 = 12;
const SLOTS_PER_EPOCH: u64 = 32;

const BEACON_BLOCK_WEIGHT: f64 = 0.5;

/// The backoff time for pruned peers.
pub(crate) const PRUNE_BACKOFF: u64 = 60;

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

impl From<&str> for Topic {
    fn from(s: &str) -> Self {
        serde_json::from_str(s).expect("json deserialization of topics never fails")
    }
}

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
    println!("attackers: {attackers:?}");

    info!(
        "Running with params {params:?} and {} participants",
        participants.len()
    );

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Start libp2p and dial the designated outbound peers
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let mut registry: Registry<Box<dyn EncodeMetric>> = Registry::default();
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

    if let Err(e) = client
        .signal_and_wait(
            BARRIER_LIBP2P_READY,
            client.run_parameters().test_instance_count,
        )
        .await
    {
        panic!("error : BARRIER_LIBP2P_READY : {:?}", e);
    }

    // Dial the designated outbound peers
    network.dial_peers(&outbound_peers).await;

    if let Err(e) = client
        .signal_and_wait(
            BARRIER_TOPOLOGY_READY,
            client.run_parameters().test_instance_count,
        )
        .await
    {
        panic!("error : BARRIER_TOPOLOGY_READY : {:?}", e);
    }

    if let Err(e) = network.subscribe_topics() {
        error!("[{}] Failed to subscribe to topics {e}", network.node_id);
        return Err(e);
    };

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Run simulation
    // /////////////////////////////////////////////////////////////////////////////////////////////
    network.run_sim(run_duration, &registry).await;

    if let Err(e) = client
        .signal_and_wait(
            BARRIER_SIMULATION_COMPLETED,
            client.run_parameters().test_instance_count,
        )
        .await
    {
        panic!("error : BARRIER_SIMULATION_COMPLETED : {:?}", e);
    }

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

pub(crate) struct Network {
    swarm: Swarm<Gossipsub>,
    node_id: usize,
    beacon_node_info: BeaconNodeInfo,
    participants: HashMap<usize, BeaconNodeInfo>,
    attackers: Vec<PeerId>,
    client: Client,
    score_interval: Interval,
    messages_gen: Generator,
    received_beacon_blocks: HashMap<Epoch, HashSet<Slot>>,
    slot_clock: SystemTimeSlotClock,
}

impl Network {
    #[allow(clippy::too_many_arguments)]
    fn new(
        registry: &mut Registry<Box<dyn EncodeMetric>>,
        keypair: Keypair,
        node_id: usize,
        beacon_node_info: BeaconNodeInfo,
        participants: HashMap<usize, BeaconNodeInfo>,
        attackers: Vec<PeerId>,
        client: Client,
        validator_set: HashSet<ValId>,
        params: Params,
    ) -> Self {
        let slot_duration = Duration::from_secs(SLOT);

        let gossipsub = {
            let gossipsub_config = GossipsubConfigBuilder::default()
                .max_transmit_size(10 * 1_048_576) // 10M
                .prune_backoff(Duration::from_secs(PRUNE_BACKOFF))
                .history_length(12)
                .build()
                .expect("Valid configuration");

            let mut gs = Gossipsub::new_with_subscription_filter_and_transform(
                MessageAuthenticity::Signed(keypair.clone()),
                gossipsub_config.clone(),
                Some((registry, Config::default())),
                AllowAllSubscriptionFilter {},
                IdentityTransform {},
            )
            .expect("Valid configuration");

            // Setup the scoring system.
            gs.with_peer_score(
                peer_score_params(slot_duration, gossipsub_config.mesh_n()),
                PeerScoreThresholds::default(),
            )
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

        Network {
            swarm,
            node_id,
            beacon_node_info,
            participants,
            attackers,
            client,
            score_interval: interval(Duration::from_secs(3)),
            messages_gen,
            received_beacon_blocks: HashMap::new(),
            slot_clock: SystemTimeSlotClock::new(
                Slot::new(genesis_slot as u64),
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

        // TODO
        // for subnet_n in 0..SYNC_SUBNETS {
        // let sync_subnet: IdentTopic = Topic::SyncMessages(subnet_n).into();
        // let sync_aggregates: IdentTopic = Topic::SignedContributionAndProof(subnet_n).into();
        // self.swarm.behaviour_mut().subscribe(&sync_subnet)?;
        // self.swarm.behaviour_mut().subscribe(&sync_aggregates)?;
        // }

        Ok(())
    }

    async fn run_sim(
        &mut self,
        run_duration: Duration,
        registry: &Registry<Box<dyn EncodeMetric>>,
    ) {
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
                        Message::AggregateAndProofAttestation { aggregator: ValId(_v), subnet: Subnet(_s) } => {
                            // TODO
                            continue;
                        },
                        Message::Attestation { attester: ValId(v), subnet: Subnet(s) } => {
                            let msg = serde_json::to_vec(&(v, payload)).expect("json serialization never fails");
                            (Topic::Attestations(s), msg)
                        },
                        Message::SignedContributionAndProof { validator: ValId(_v), subnet: Subnet(_s) } => {
                            // TODO
                            continue;
                        },
                        Message::SyncCommitteeMessage { validator: ValId(_v), subnet: Subnet(_s) } => {
                            // TODO
                            continue;
                        },
                    };

                    if let Err(e) = self.publish(topic.clone(), msg) {
                        error!("Failed to publish message {e} to topic {topic:?}");
                    }

                }
                // Record peer scores
                _ = self.score_interval.tick() => {
                    self.record_peer_scores().await;
                    self.record_metrics(registry).await;
                }
                event = self.swarm.select_next_some() => {
                    match event {
                        SwarmEvent::Behaviour(gossipsub_event) => self.handle_gossipsub_event(gossipsub_event),
                        _ => debug!("SwarmEvent: {:?}", event),
                    }
                }
            }
        }

        println!(
            "Done simulation: received blocks: {:?}",
            self.received_beacon_blocks
        );
        self.record_received_beacon_blocks().await;
    }

    fn publish(
        &mut self,
        topic: Topic,
        msg: Vec<u8>,
    ) -> Result<libp2p::gossipsub::MessageId, libp2p::gossipsub::error::PublishError> {
        info!("Publish {:?}", topic);
        let ident_topic: IdentTopic = topic.into();
        self.swarm.behaviour_mut().publish(ident_topic, msg)
    }

    fn handle_gossipsub_event(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message {
                propagation_source: _,
                message_id: _,
                message,
            } => {
                let topic: Topic = message.topic.as_str().into();
                println!("received topic: {:?}", topic);

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
                            warn!("The BeaconBlock message on slot {slot} is already received.")
                        }
                    }
                    Topic::Attestations(subnet_id) => {
                        let (validator, _payload): (u64, String) =
                            serde_json::from_slice(&message.data).unwrap();
                        // TODO
                        println!("Topic::Attestations ... {subnet_id}, {validator}");
                    }
                    _ => todo!(),
                }
            }
            other => println!("GossipsubEvent: {:?}", other),
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

                Local.timestamp(duration.as_secs() as i64, 0).into()
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

    /// Record peer scores
    async fn record_peer_scores(&mut self) {
        let gossipsub = self.swarm.behaviour_mut();
        let scores = gossipsub
            .all_peers()
            .filter_map(|(peer, _)| gossipsub.peer_score(peer).map(|score| (peer, score)))
            .collect::<Vec<_>>();

        if !scores.is_empty() {
            let measurement = format!("{}_scores", env!("CARGO_PKG_NAME"));

            let mut query = WriteQuery::new(Local::now().into(), measurement)
                .add_tag(TAG_PEER_ID, self.beacon_node_info.peer_id.to_string())
                .add_tag(TAG_RUN_ID, self.client.run_parameters().test_run);

            for (peer, score) in scores {
                let field = if self.attackers.contains(peer) {
                    format!("attacker_{}", peer.to_string())
                } else {
                    peer.to_string()
                };
                query = query.add_field(field, score);
            }

            if let Err(e) = self.client.record_metric(query).await {
                warn!("Failed to record score: {e:?}");
            }
        }
    }

    /// Record gossipsub metrics
    async fn record_metrics(&mut self, registry: &Registry<Box<dyn EncodeMetric>>) {
        let metric_set = prometheus_client::encoding::proto::encode(&registry);

        let now = Local::now();
        let run_id = self.client.run_parameters().test_run;
        let mut queries = vec![];

        for family in metric_set.metric_families.iter() {
            let q = match family.name.as_str() {
                // ///////////////////////////////////
                // Metrics per known topic
                // ///////////////////////////////////
                "topic_subscription_status" => {
                    continue;
                }
                "topic_peers_counts" => {
                    // TODO
                    continue;
                    // queries_for_gauge(&test_start_time, family, &instance_info, run_id, "count")
                }
                "invalid_messages_per_topic"
                | "accepted_messages_per_topic"
                | "ignored_messages_per_topic"
                | "rejected_messages_per_topic" => {
                    queries_for_counter(&now, family, &self.beacon_node_info, &run_id)
                }
                // ///////////////////////////////////
                // Metrics regarding mesh state
                // ///////////////////////////////////
                "mesh_peer_counts" => {
                    // TODO
                    continue;
                    // queries_for_gauge(&test_start_time, family, &instance_info, run_id, "count")
                }
                "mesh_peer_inclusion_events" => {
                    queries_for_counter(&now, family, &self.beacon_node_info, &run_id)
                }
                "mesh_peer_churn_events" => {
                    queries_for_counter(&now, family, &self.beacon_node_info, &run_id)
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
                    queries_for_counter(&now, family, &self.beacon_node_info, &run_id)
                }
                // ///////////////////////////////////
                // Metrics related to scoring
                // ///////////////////////////////////
                "score_per_mesh" => {
                    // TODO
                    continue;
                    // queries_for_histogram(&test_start_time, family, &instance_info, run_id)
                }
                "scoring_penalties" => {
                    queries_for_counter(&now, family, &self.beacon_node_info, &run_id)
                }
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
                "topic_iwant_msgs" => {
                    queries_for_counter(&now, family, &self.beacon_node_info, &run_id)
                }
                "memcache_misses" => {
                    queries_for_counter(&now, family, &self.beacon_node_info, &run_id)
                }
                _ => unreachable!(),
            };
            queries.extend(q);
        }

        for q in queries {
            if let Err(e) = self.client.record_metric(q).await {
                error!("Failed to record metrics: {e:?}");
            }
        }
    }
}

fn peer_score_params(slot_duration: Duration, mesh_n: usize) -> PeerScoreParams {
    let mut params = PeerScoreParams::default();

    let get_hash = |topic: Topic| -> TopicHash {
        let ident_topic: IdentTopic = topic.into();
        ident_topic.hash()
    };

    params.topics.insert(
        get_hash(Topic::Blocks),
        beacon_block_param(slot_duration, mesh_n),
    );

    params
}

fn beacon_block_param(slot_duration: Duration, mesh_n: usize) -> TopicScoreParams {
    let expected_message_rate = 1.0;
    let mesh_message_deliveries_window = 2;
    let epoch = slot_duration * SLOTS_PER_EPOCH as u32;
    let decay_interval = max(Duration::from_secs(1), slot_duration);
    let decay_to_zero: f64 = 0.01;

    let mut param = TopicScoreParams::default();
    param.topic_weight = BEACON_BLOCK_WEIGHT;

    // ////////////////////////////////////////////////////////////////////////
    //  P1: time in the mesh
    // ////////////////////////////////////////////////////////////////////////
    // 12s
    param.time_in_mesh_quantum = slot_duration;
    // 300.0
    param.time_in_mesh_cap = 3600.0 / param.time_in_mesh_quantum.as_secs_f64();
    // 0.03333333333333333
    param.time_in_mesh_weight = 10.0 / param.time_in_mesh_cap;

    // ////////////////////////////////////////////////////////////////////////
    //  P2: first message deliveries
    // ////////////////////////////////////////////////////////////////////////
    // 0.9928302477768374
    param.first_message_deliveries_decay = {
        let first_message_decay_time = epoch * 20;
        score_parameter_decay(first_message_decay_time, decay_interval, decay_to_zero)
    };
    // 46.491611280019605
    param.first_message_deliveries_cap = decay_convergence(
        param.first_message_deliveries_decay,
        2.0 * expected_message_rate / mesh_n as f64,
    );
    // 0.8603702667795156
    param.first_message_deliveries_weight = 40.0 / param.first_message_deliveries_cap;

    // ////////////////////////////////////////////////////////////////////////
    //  P3: mesh message deliveries
    // ////////////////////////////////////////////////////////////////////////
    // -0.5
    param.mesh_message_deliveries_weight = -param.topic_weight;
    // 0.9716279515771061
    param.mesh_message_deliveries_decay = {
        let decay_slots = SLOTS_PER_EPOCH * 5;
        let decay_time = slot_duration * decay_slots as u32;
        score_parameter_decay(decay_time, decay_interval, decay_to_zero)
    };
    // 0.6849191409056553
    param.mesh_message_deliveries_threshold = threshold(
        param.mesh_message_deliveries_decay,
        expected_message_rate / 50.0,
    );
    // 2.054757422716966
    param.mesh_message_deliveries_cap = {
        let cap_factor = 3.0;
        if cap_factor * param.mesh_message_deliveries_threshold < 2.0 {
            2.0
        } else {
            cap_factor * param.mesh_message_deliveries_threshold
        }
    };
    // 384s
    param.mesh_message_deliveries_activation = epoch;
    // 2s
    param.mesh_message_deliveries_window = Duration::from_secs(mesh_message_deliveries_window);

    // ////////////////////////////////////////////////////////////////////////
    //  P3b: sticky mesh propagation failures
    // ////////////////////////////////////////////////////////////////////////
    // -0.5
    param.mesh_failure_penalty_weight = param.mesh_message_deliveries_weight;
    // 0.9716279515771061
    param.mesh_failure_penalty_decay = param.mesh_message_deliveries_decay;

    // ////////////////////////////////////////////////////////////////////////
    //  P4: invalid messages
    // ////////////////////////////////////////////////////////////////////////
    // -214.99999999999994
    param.invalid_message_deliveries_weight = -214.99999999999994;
    // 0.9971259067705325
    param.invalid_message_deliveries_decay =
        score_parameter_decay(epoch * 50, decay_interval, decay_to_zero);

    println!("beacon_block_param: {:?}", param);

    param
}

fn score_parameter_decay(
    decay_time: Duration,
    decay_interval: Duration,
    decay_to_zero: f64,
) -> f64 {
    let ticks = decay_time.as_secs_f64() / decay_interval.as_secs_f64();
    decay_to_zero.powf(1.0 / ticks)
}

fn decay_convergence(decay: f64, rate: f64) -> f64 {
    rate / (1.0 - decay)
}

fn threshold(decay: f64, rate: f64) -> f64 {
    decay_convergence(decay, rate) * decay
}
