use crate::InstanceInfo;
use chrono::{DateTime, Utc};
use futures::stream::FuturesUnordered;
use libp2p::gossipsub::{
    error::GossipsubHandlerError, Gossipsub, GossipsubEvent, IdentTopic, MessageId,
    Topic as GossipTopic,
};
use libp2p::swarm::SwarmEvent;
use libp2p::PeerId;
use libp2p::Swarm;
use npg::Generator;
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use rand;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use testground::client::Client;
use tokio::task::JoinHandle;
use tokio::time::Interval;
use tokio_util::time::DelayQueue;
use tracing::{debug, info};

mod metrics;
mod run;

pub(crate) use run::run;
use run::{ATTESTATION_SUBNETS, SYNC_SUBNETS};

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
    /// A delay queue indicating when to validate messages
    messages_to_validate: DelayQueue<(MessageId, PeerId)>,
}

impl Network {
    #[allow(clippy::too_many_arguments)]

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
                message_id,
                message,
            }) => {
                // Log the event
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

                // Perform custom validation and artificial delay
                self.custom_validation(message_id, propagation_source, message.data.len());
            }
            _ => debug!("SwarmEvent: {:?}", event),
        }
    }

    /// Create an artificial delay to the validation which half-represents propagation time, with
    /// the caveat that the delay applies to all peers.
    fn custom_validation(&mut self, message_id: MessageId, peer_id: PeerId, msg_size: usize) {
        // Lets use tiers for message propagation and validation
        let mut rng = rand::thread_rng();

        let ms_duration = match msg_size {
            0..=300 => {
                // 500 bytes, up to a 50ms delay
                rng.gen_range(0..50)
            }
            301..=600 => {
                // 300-600 bytes up to 80ms delay
                rng.gen_range(0..80)
            }
            _ => {
                // Blocks can be up to a few hundred kb
                // The delay can be up to 500ms
                rng.gen_range(0..500)
            }
        };
        self.messages_to_validate.insert(
            (message_id, peer_id),
            tokio::time::Duration::from_millis(ms_duration),
        );
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
            Topic::Attestations(x) => format!("Attestations/{}", x).into(),
            Topic::SyncMessages(x) => format!("SyncMessages/{}", x).into(),
            Topic::SignedContributionAndProof(x) => {
                format!("SignedContributionAndProof/{}", x).into()
            }
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
            x => match x.rsplit_once("/") {
                Some(("Attestations", x)) => {
                    Topic::Attestations(x.parse::<u64>().expect("no malicious topics"))
                }
                Some(("SyncMessages", x)) => {
                    Topic::SyncMessages(x.parse::<u64>().expect("no malicious topics"))
                }
                Some(("SignedContributionAndProof", x)) => Topic::SignedContributionAndProof(
                    x.parse::<u64>().expect("no malicious topics"),
                ),
                _ => unreachable!(),
            },
        }
    }
}
