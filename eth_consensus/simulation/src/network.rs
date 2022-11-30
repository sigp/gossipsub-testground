use crate::InstanceInfo;
use chrono::{DateTime, Utc};
use gen_topology::Params;
use libp2p::gossipsub::metrics::Config;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    error::GossipsubHandlerError, Gossipsub, GossipsubConfigBuilder, GossipsubEvent,
    GossipsubMessage, IdentTopic, IdentityTransform, MessageAuthenticity, MessageId,
    PeerScoreParams, PeerScoreThresholds, Topic as GossipTopic, ValidationMode, 
};
use libp2p::identity::Keypair;
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::PeerId;
use libp2p::Swarm;
use npg::slot_generator::ValId;
use npg::Generator;
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use testground::client::Client;
use tokio::time::{interval, Interval};
use tracing::{debug, info};
use futures::stream::FuturesUnordered;
use tokio::task::JoinHandle;

mod metrics;
mod run;

pub(crate) use run::run;

const ATTESTATION_SUBNETS: u64 = 4;
const SYNC_SUBNETS: u64 = 4;
const SLOTS_PER_EPOCH: u64 = 2;
const SLOT_DURATION: u64 = 12;

/// Main struct to run the simulation.
pub struct Network {
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
    client: Arc<Client>,
    /// Chronos time reported by testground as the start of the test run.
    start_time: DateTime<Utc>,
    /// Instant in which the simmulation starts running, according to the local time.
    local_start_time: Instant,
    /// How often metrics are recorded.
    metrics_interval: Interval,
    /// Generator of messages per slot.
    messages_gen: Generator,
    /// Keeps track of futures spawned for the influx db to end gracefully.
    influx_db_handles: FuturesUnordered<JoinHandle<()>>,
}

impl Network {
    #[allow(clippy::too_many_arguments)]

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
            let gossip_message_id = move |message: &GossipsubMessage| {
                MessageId::from(
                    &Sha256::digest([message.topic.as_str().as_bytes(), &message.data].concat())
                        [..20],
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
                // .validate_messages() // TODO: Reintroduce message validation delays
                .validation_mode(ValidationMode::Anonymous)
                .duplicate_cache_time(Duration::from_secs(SLOT_DURATION * SLOTS_PER_EPOCH + 1))
                .message_id_fn(gossip_message_id)
                .allow_self_origin(true)
                .build()
                .expect("valid gossipsub configuration");

            let mut gs = Gossipsub::new_with_subscription_filter_and_transform(
                MessageAuthenticity::Anonymous,
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
            run::build_transport(&keypair),
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
        }
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

    /// Subscribes to a set of topics.
    pub fn subscribe_topics(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // blocks, attestations and aggregates, sync messages and aggregates
        let blocks_topic: IdentTopic = Topic::Blocks.into();
        let aggregate_topic: IdentTopic = Topic::Aggregates.into();
        self.swarm.behaviour_mut().subscribe(&blocks_topic)?;
        self.swarm.behaviour_mut().subscribe(&aggregate_topic)?;
        for subnet_n in 0..ATTESTATION_SUBNETS {
            let attestation_subnet: IdentTopic = Topic::Attestations(subnet_n).into();
            self.swarm.behaviour_mut().subscribe(&attestation_subnet)?;
        }

        for subnet_n in 0..SYNC_SUBNETS {
            let sync_subnet: IdentTopic = Topic::SyncMessages(subnet_n).into();
            let sync_aggregates: IdentTopic = Topic::SignedContributionAndProof(subnet_n).into();
            self.swarm.behaviour_mut().subscribe(&sync_subnet)?;
            self.swarm.behaviour_mut().subscribe(&sync_aggregates)?;
        }
        Ok(())
    }

    /// Publishes a payload to a given topic.
    fn publish(
        &mut self,
        topic: Topic,
        validator: u64,
        mut payload: Vec<u8>,
    ) -> Result<libp2p::gossipsub::MessageId, libp2p::gossipsub::error::PublishError> {
        // Plain binary as messages, coupled with the validator
        payload.append(&mut validator.to_be_bytes().to_vec());

        if let Topic::Blocks = topic {
            info!(
                "[{}] Publishing message topic: {}, size: {}",
                self.node_id,
                IdentTopic::from(topic.clone()),
                payload.len()
            );
        }
        let ident_topic: IdentTopic = topic.into();
        self.swarm.behaviour_mut().publish(ident_topic, payload)
    }

    // An inbound event or swarm event gets sent here
    fn handle_swarm_event(&mut self, event: SwarmEvent<GossipsubEvent, GossipsubHandlerError>) {
        match event {
            SwarmEvent::Behaviour(GossipsubEvent::Message {
                propagation_source,
                message_id: _,
                message,
            }) => {
                let src_node = self
                    .participants
                    .iter()
                    .find(|(_k, v)| v.peer_id == propagation_source)
                    .map(|(k, _v)| k);
                if message.topic == IdentTopic::from(Topic::Blocks).hash() {
                    info!(
                        "[{}] Received block from: {:?}, size {}",
                        self.node_id,
                        src_node,
                        message.data.len()
                    );
                }
            }
            _ => debug!("SwarmEvent: {:?}", event),
        }
    }
}

#[derive(Clone, Debug)]
enum Topic {
    Blocks,
    Attestations(u64),
    Aggregates,
    SyncMessages(u64),
    SignedContributionAndProof(u64),
}

impl From<Topic> for IdentTopic {
    fn from(t: Topic) -> Self {
        let rep: String = match t {
            Topic::Blocks => "Blocks".into(),
            Topic::Aggregates => "Aggregates".into(),
            Topic::Attestations(x) => format!("Attestations/{}",x).into(),
            Topic::SyncMessages(x) => format!("SyncMessages/{}",x).into(),
            Topic::SignedContributionAndProof(x) => format!("SignedContributionAndProof/{}",x).into(),
        };
        GossipTopic::new(rep)
    }
}

impl From<IdentTopic> for Topic {
    fn from(t: IdentTopic) -> Self {
        let repr = t.hash().into_string();
        match repr.as_str() {
            "Blocks" => Topic::Blocks,
            "Aggregates" => Topic::Aggregates,
            x => {
           match x.rsplit_once("/") {
                Some(("Attestations", x)) => Topic::Attestations(x.parse::<u64>().expect("no malicious topics")),
                Some(("SyncMessages", x)) => Topic::SyncMessages(x.parse::<u64>().expect("no malicious topics")),
                Some(("SignedContributionAndProof", x)) => Topic::SignedContributionAndProof(x.parse::<u64>().expect("no malicious topics")),
                _ => unreachable!()
            }
       }

    }
    }
}
