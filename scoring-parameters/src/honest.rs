use crate::utils::{
    queries_for_counter, BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY, TAG_INSTANCE_PEER_ID,
    TAG_RUN_ID,
};
use crate::InstanceInfo;
use chrono::TimeZone;
use chrono::{DateTime, Local, Utc};
use gen_topology::Params;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::metrics::Config;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, GossipsubEvent, IdentTopic, IdentityTransform,
    MessageAuthenticity, PeerScoreParams, PeerScoreThresholds, Topic as GossipTopic,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::Transport;
use libp2p::{PeerId, Swarm};
use npg::slot_generator::Subnet;
use npg::slot_generator::ValId;
use npg::{Generator, Message};
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use slot_clock::SlotClock;
use slot_clock::SystemTimeSlotClock;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use testground::client::Client;
use testground::{Timestamp, WriteQuery};
use tokio::time::{interval, Interval};
use tracing::{debug, error, info, warn};
use types::{Epoch, Slot};

const ATTESTATION_SUBNETS: u64 = 4;
const SYNC_SUBNETS: u64 = 4;

pub(crate) const SLOTS_PER_EPOCH: u64 = 2;

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

pub(crate) async fn run(
    client: Client,
    node_id: usize,
    instance_info: InstanceInfo,
    participants: HashMap<usize, InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Parse parameters and generate network topology
    // /////////////////////////////////////////////////////////////////////////////////////////////
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

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Start libp2p and dial the designated outbound peers
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let mut registry: Registry<Box<dyn EncodeMetric>> = Registry::default();
    registry.sub_registry_with_prefix("gossipsub");
    let mut network = Network::new(
        &mut registry,
        keypair,
        node_id,
        instance_info.clone(),
        participants.clone(),
        client.clone(),
        validator_set,
        params,
    );

    // Set up the listening address
    network.start_libp2p().await;

    if let Err(e) = client
        .signal_and_wait(BARRIER_LIBP2P_READY, test_instance_count)
        .await
    {
        panic!("error : BARRIER_LIBP2P_READY : {:?}", e);
    }

    // Dial the designated outbound peers
    network.dial_peers(outbound_peers).await;

    if let Err(e) = client
        .signal_and_wait(BARRIER_TOPOLOGY_READY, test_instance_count)
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
    network.run_sim(run_duration).await;

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Record metrics
    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Encode the metrics to an instance of the OpenMetrics protobuf format.
    // https://github.com/OpenObservability/OpenMetrics/blob/main/proto/openmetrics_data_model.proto
    let metric_set = prometheus_client::encoding::proto::encode(&registry);

    // The `test_start_time` is used as the timestamp of metrics instead of local time of each
    // instance so the timestamps between metrics of each instance are aligned.
    // This is helpful when we want to sort the metrics by something not timestamp
    // (e.g. instance_name).
    let test_start_time: DateTime<Utc> =
        DateTime::parse_from_rfc3339(&client.run_parameters().test_start_time)?.into();

    let run_id = &client.run_parameters().test_run;
    let mut queries = vec![];
    for family in metric_set.metric_families.iter() {
        let q = match family.name.as_str() {
            // ///////////////////////////////////
            // Metrics related to scoring
            // ///////////////////////////////////
            "scoring_penalties" => {
                queries_for_counter(&test_start_time, family, &instance_info, run_id)
            }
            _ => continue,
        };
        queries.extend(q);
    }

    for query in queries {
        if let Err(e) = client.record_metric(query).await {
            error!("Failed to record metrics: {e:?}");
        }
    }

    client.record_success().await?;
    Ok(())
}

fn parse_params(
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
    received_beacon_blocks: HashMap<Epoch, HashSet<Slot>>,
    slot_clock: SystemTimeSlotClock,
}

impl Network {
    fn new(
        registry: &mut Registry<Box<dyn EncodeMetric>>,
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
                .max_transmit_size(10 * 1_048_576) // 10M
                .prune_backoff(Duration::from_secs(60))
                .history_length(12)
                .build()
                .expect("Valid configuration");

            let mut gs = Gossipsub::new_with_subscription_filter_and_transform(
                MessageAuthenticity::Signed(keypair.clone()),
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
            instance_info,
            participants,
            client,
            score_interval: interval(Duration::from_secs(1)),
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

    pub fn subscribe_topics(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // blocks, attestations and aggregates, sync messages and aggregates
        let blocks_topic: IdentTopic = Topic::Blocks.into();
        self.swarm.behaviour_mut().subscribe(&blocks_topic)?;

        // TODO
        // for subnet_n in 0..ATTESTATION_SUBNETS {
        //     let attestation_subnet: IdentTopic = Topic::Attestations(subnet_n).into();
        //     let aggregate_subnet: IdentTopic = Topic::Aggregates(subnet_n).into();
        //     self.swarm.behaviour_mut().subscribe(&attestation_subnet)?;
        //     self.swarm.behaviour_mut().subscribe(&aggregate_subnet)?;
        // }

        // TODO
        // for subnet_n in 0..SYNC_SUBNETS {
        // let sync_subnet: IdentTopic = Topic::SyncMessages(subnet_n).into();
        // let sync_aggregates: IdentTopic = Topic::SignedContributionAndProof(subnet_n).into();
        // self.swarm.behaviour_mut().subscribe(&sync_subnet)?;
        // self.swarm.behaviour_mut().subscribe(&sync_aggregates)?;
        // }

        Ok(())
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
                    let (topic, msg) = match m {
                        Message::BeaconBlock { proposer: ValId(v), slot } => {
                            let msg = serde_json::to_vec(&(v, slot, payload)).expect("json serialization never fails");
                            (Topic::Blocks, msg)
                        },
                        Message::AggregateAndProofAttestation { aggregator: ValId(_v), subnet: Subnet(_s) } => {
                            // TODO
                            continue;
                        },
                        Message::Attestation { attester: ValId(_v), subnet: Subnet(_s) } => {
                            // TODO
                            continue;
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
                    // self.record_peer_scores().await;
                }
                event = self.swarm.select_next_some() => {
                    debug!("SwarmEvent: {:?}", event);

                    match event {
                        SwarmEvent::Behaviour(gossipsub_event) => self.handle_gossipsub_event(gossipsub_event),
                        _ => {}
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
                    _ => {}
                }
            }
            other => println!("GossipsubEvent: {:?}", other),
        }
    }

    /// Record the number of BeaconBlock messages received per epoch.
    async fn record_received_beacon_blocks(&self) {
        let mut queries = vec![];
        let run_id = self.client.run_parameters().test_run;

        for (epoch, slots) in self.received_beacon_blocks.iter() {
            let timestamp: Timestamp = {
                let duration = self
                    .slot_clock
                    .start_of(epoch.start_slot(SLOTS_PER_EPOCH))
                    .unwrap();

                Local.timestamp(duration.as_secs() as i64, 0).into()
            };

            let query = WriteQuery::new(timestamp, "beacon_block")
                .add_tag(TAG_INSTANCE_PEER_ID, self.instance_info.peer_id.to_string())
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
}
