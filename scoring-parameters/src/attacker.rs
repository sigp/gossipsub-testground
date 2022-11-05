// use crate::beacon_node::PRUNE_BACKOFF;
// use crate::utils::{barrier_and_drive_swarm, BARRIER_DONE, BARRIER_STARTED_LIBP2P, BARRIER_WARMUP};
// use crate::InstanceInfo;
// use delay_map::HashSetDelay;
// use libp2p_testground::core::connection::ConnectionId;
// use libp2p_testground::core::muxing::StreamMuxerBox;
// use libp2p_testground::core::upgrade::{SelectUpgrade, Version};
// use libp2p_testground::dns::TokioDnsConfig;
// use libp2p_testground::futures::StreamExt;
// use libp2p_testground::gossipsub::error::PublishError;
// use libp2p_testground::gossipsub::handler::{GossipsubHandler, GossipsubHandlerIn, HandlerEvent};
// use libp2p_testground::gossipsub::protocol::ProtocolConfig;
// use libp2p_testground::gossipsub::types::{
//     GossipsubControlAction, GossipsubSubscription, GossipsubSubscriptionAction, PeerInfo,
// };
// use libp2p_testground::gossipsub::{
//     rpc_proto, GossipsubConfig, GossipsubEvent, GossipsubRpc, TopicHash,
// };
// use libp2p_testground::identity::Keypair;
// use libp2p_testground::mplex::MplexConfig;
// use libp2p_testground::noise::{NoiseConfig, X25519Spec};
// use libp2p_testground::swarm::{
//     ConnectionHandler, IntoConnectionHandler, NetworkBehaviour, NetworkBehaviourAction,
//     NotifyHandler, PollParameters, SwarmBuilder, SwarmEvent,
// };
// use libp2p_testground::tcp::{GenTcpConfig, TokioTcpTransport};
// use libp2p_testground::yamux::YamuxConfig;
// use libp2p_testground::Transport;
// use libp2p_testground::{PeerId, Swarm};
// use prost::Message;
// use std::collections::VecDeque;
// use std::sync::Arc;
// use std::task::{Context, Poll};
// use std::time::Duration;
// use testground::client::Client;
// use tracing::{debug, error};
//
// // In this `attacker` module, we use `libp2p_testground` instead of `libp2p`.
// // `libp2p_testground` is a fork of `libp2p`, some its module (e.g. `handler`) made public to
// // implement `MaliciousBehaviour`.
// // See https://github.com/ackintosh/rust-libp2p/pull/45 for changes made in `libp2p_testground`.
//
// /// A delay before sending GRAFT after being pruned.
// const PRUNE_BACKOFF_DELAY: u64 = 5;
//
// pub(crate) async fn run(
//     client: Client,
//     instance_info: InstanceInfo,
//     participants: Vec<InstanceInfo>,
//     // NOTE: this is a `Keypair` struct in `libp2p` (not `libp2p_testground`) which has been
//     // generated in the main function.
//     keypair: libp2p::identity::Keypair,
// ) -> Result<(), Box<dyn std::error::Error>> {
//     // Convert the `keypair` from `libp2p::identity::Keypair` to `libp2p_testground` one.
//     let keypair =
//         Keypair::from_protobuf_encoding(&keypair.to_protobuf_encoding().unwrap()).unwrap();
//
//     // ////////////////////////////////////////////////////////////////////////
//     // Start libp2p
//     // ////////////////////////////////////////////////////////////////////////
//     let mut swarm = build_swarm(keypair);
//     swarm.listen_on(instance_info.multiaddr.clone())?;
//     match swarm.next().await.unwrap() {
//         SwarmEvent::NewListenAddr { address, .. } if address == instance_info.multiaddr => {}
//         e => panic!("Unexpected event {:?}", e),
//     }
//
//     barrier_and_drive_swarm(&client, &mut swarm, BARRIER_STARTED_LIBP2P).await?;
//
//     // /////////////////////////////////////////////////////////////////////////////////////////////
//     // Setup discovery
//     // /////////////////////////////////////////////////////////////////////////////////////////////
//     let victim = {
//         participants
//             .iter()
//             .find(|&info| info.role.is_publisher() && info.group_seq == 1)
//             .expect("publisher should exists")
//             .clone()
//     };
//     client.record_message(format!("Victim: {:?}", victim));
//
//     swarm.dial(victim.multiaddr)?;
//
//     barrier_and_drive_swarm(&client, &mut swarm, BARRIER_WARMUP).await?;
//
//     barrier_and_drive_swarm(&client, &mut swarm, BARRIER_DONE).await?;
//
//     client.record_success().await?;
//     Ok(())
// }
//
// fn build_swarm(keypair: Keypair) -> Swarm<MaliciousBehaviour> {
//     SwarmBuilder::new(
//         build_transport(&keypair),
//         MaliciousBehaviour::new(),
//         PeerId::from(keypair.public()),
//     )
//     .executor(Box::new(|future| {
//         tokio::spawn(future);
//     }))
//     .build()
// }
//
// fn build_transport(
//     keypair: &Keypair,
// ) -> libp2p_testground::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
//     let transport = TokioDnsConfig::system(TokioTcpTransport::new(
//         GenTcpConfig::default().nodelay(true),
//     ))
//     .expect("DNS config");
//
//     let noise_keys = libp2p_testground::noise::Keypair::<X25519Spec>::new()
//         .into_authentic(keypair)
//         .expect("Signing libp2p-noise static DH keypair failed.");
//
//     transport
//         .upgrade(Version::V1)
//         .authenticate(NoiseConfig::xx(noise_keys).into_authenticated())
//         .multiplex(SelectUpgrade::new(
//             YamuxConfig::default(),
//             MplexConfig::default(),
//         ))
//         .timeout(Duration::from_secs(20))
//         .boxed()
// }
//
// type GossipsubNetworkBehaviourAction =
//     NetworkBehaviourAction<GossipsubEvent, GossipsubHandler, Arc<GossipsubHandlerIn>>;
//
// pub struct MaliciousBehaviour {
//     /// Configuration providing gossipsub performance parameters.
//     config: GossipsubConfig,
//     /// Events that need to be yielded to the outside when polling.
//     events: VecDeque<GossipsubNetworkBehaviourAction>,
//     /// A collection of peers awaiting to be re-grafted.
//     regraft: HashSetDelay<(PeerId, TopicHash)>,
// }
//
// impl MaliciousBehaviour {
//     fn new() -> Self {
//         MaliciousBehaviour {
//             config: GossipsubConfig::default(),
//             events: VecDeque::new(),
//             regraft: HashSetDelay::new(Duration::from_secs(PRUNE_BACKOFF + PRUNE_BACKOFF_DELAY)),
//         }
//     }
//
//     /// Handles received subscriptions.
//     fn handle_received_subscriptions(
//         &mut self,
//         subscriptions: &[GossipsubSubscription],
//         propagation_source: &PeerId,
//     ) {
//         debug!(
//             "Handling subscriptions: {:?}, from source: {}",
//             subscriptions,
//             propagation_source.to_string()
//         );
//
//         let mut topics_to_graft = Vec::new();
//         for subscription in subscriptions {
//             match subscription.action {
//                 GossipsubSubscriptionAction::Subscribe => {
//                     topics_to_graft.push(subscription.topic_hash.clone());
//                 }
//                 GossipsubSubscriptionAction::Unsubscribe => {}
//             }
//         }
//
//         if !topics_to_graft.is_empty() {
//             if let Err(error) = self.send_message(
//                 *propagation_source,
//                 GossipsubRpc {
//                     subscriptions: Vec::new(),
//                     messages: Vec::new(),
//                     control_msgs: topics_to_graft
//                         .into_iter()
//                         .map(|topic_hash| GossipsubControlAction::Graft { topic_hash })
//                         .collect(),
//                 }
//                 .into_protobuf(),
//             ) {
//                 error!("Failed sending grafts: {}", error);
//             }
//         }
//     }
//
//     /// Handles PRUNE control messages.
//     fn handle_prune(
//         &mut self,
//         peer_id: &PeerId,
//         prune_data: Vec<(TopicHash, Vec<PeerInfo>, Option<u64>)>,
//     ) {
//         for prune in prune_data {
//             if let Some(backoff) = prune.2 {
//                 // If a backoff is specified by the peer obey it.
//                 self.regraft.insert_at(
//                     (*peer_id, prune.0),
//                     Duration::from_secs(backoff + PRUNE_BACKOFF_DELAY),
//                 );
//             } else {
//                 self.regraft.insert((*peer_id, prune.0));
//             }
//         }
//     }
//
//     // /////////////////////////////////////////////////////////////////////////////////////////////
//     // The methods from here down is copied from `libp2p::protocols::gossipsub::Gossipsub`.
//     // /////////////////////////////////////////////////////////////////////////////////////////////
//
//     /// Send a GossipsubRpc message to a peer. This will wrap the message in an arc if it
//     /// is not already an arc.
//     fn send_message(
//         &mut self,
//         peer_id: PeerId,
//         message: rpc_proto::Rpc,
//     ) -> Result<(), PublishError> {
//         // If the message is oversized, try and fragment it. If it cannot be fragmented, log an
//         // error and drop the message (all individual messages should be small enough to fit in the
//         // max_transmit_size)
//
//         let messages = self.fragment_message(message)?;
//
//         for message in messages {
//             self.events
//                 .push_back(NetworkBehaviourAction::NotifyHandler {
//                     peer_id,
//                     event: Arc::new(GossipsubHandlerIn::Message(message)),
//                     handler: NotifyHandler::Any,
//                 })
//         }
//         Ok(())
//     }
//
//     // If a message is too large to be sent as-is, this attempts to fragment it into smaller RPC
//     // messages to be sent.
//     fn fragment_message(&self, rpc: rpc_proto::Rpc) -> Result<Vec<rpc_proto::Rpc>, PublishError> {
//         if rpc.encoded_len() < self.config.max_transmit_size() {
//             return Ok(vec![rpc]);
//         }
//
//         let new_rpc = rpc_proto::Rpc {
//             subscriptions: Vec::new(),
//             publish: Vec::new(),
//             control: None,
//         };
//
//         let mut rpc_list = vec![new_rpc.clone()];
//
//         // Gets an RPC if the object size will fit, otherwise create a new RPC. The last element
//         // will be the RPC to add an object.
//         macro_rules! create_or_add_rpc {
//             ($object_size: ident ) => {
//                 let list_index = rpc_list.len() - 1; // the list is never empty
//
//                 // create a new RPC if the new object plus 5% of its size (for length prefix
//                 // buffers) exceeds the max transmit size.
//                 if rpc_list[list_index].encoded_len() + (($object_size as f64) * 1.05) as usize
//                     > self.config.max_transmit_size()
//                     && rpc_list[list_index] != new_rpc
//                 {
//                     // create a new rpc and use this as the current
//                     rpc_list.push(new_rpc.clone());
//                 }
//             };
//         }
//
//         macro_rules! add_item {
//             ($object: ident, $type: ident ) => {
//                 let object_size = $object.encoded_len();
//
//                 if object_size + 2 > self.config.max_transmit_size() {
//                     // This should not be possible. All received and published messages have already
//                     // been vetted to fit within the size.
//                     error!("Individual message too large to fragment");
//                     return Err(PublishError::MessageTooLarge);
//                 }
//
//                 create_or_add_rpc!(object_size);
//                 rpc_list
//                     .last_mut()
//                     .expect("Must have at least one element")
//                     .$type
//                     .push($object.clone());
//             };
//         }
//
//         // Add messages until the limit
//         for message in &rpc.publish {
//             add_item!(message, publish);
//         }
//         for subscription in &rpc.subscriptions {
//             add_item!(subscription, subscriptions);
//         }
//
//         // handle the control messages. If all are within the max_transmit_size, send them without
//         // fragmenting, otherwise, fragment the control messages
//         let empty_control = rpc_proto::ControlMessage::default();
//         if let Some(control) = rpc.control.as_ref() {
//             if control.encoded_len() + 2 > self.config.max_transmit_size() {
//                 // fragment the RPC
//                 for ihave in &control.ihave {
//                     let len = ihave.encoded_len();
//                     create_or_add_rpc!(len);
//                     rpc_list
//                         .last_mut()
//                         .expect("Always an element")
//                         .control
//                         .get_or_insert_with(|| empty_control.clone())
//                         .ihave
//                         .push(ihave.clone());
//                 }
//                 for iwant in &control.iwant {
//                     let len = iwant.encoded_len();
//                     create_or_add_rpc!(len);
//                     rpc_list
//                         .last_mut()
//                         .expect("Always an element")
//                         .control
//                         .get_or_insert_with(|| empty_control.clone())
//                         .iwant
//                         .push(iwant.clone());
//                 }
//                 for graft in &control.graft {
//                     let len = graft.encoded_len();
//                     create_or_add_rpc!(len);
//                     rpc_list
//                         .last_mut()
//                         .expect("Always an element")
//                         .control
//                         .get_or_insert_with(|| empty_control.clone())
//                         .graft
//                         .push(graft.clone());
//                 }
//                 for prune in &control.prune {
//                     let len = prune.encoded_len();
//                     create_or_add_rpc!(len);
//                     rpc_list
//                         .last_mut()
//                         .expect("Always an element")
//                         .control
//                         .get_or_insert_with(|| empty_control.clone())
//                         .prune
//                         .push(prune.clone());
//                 }
//             } else {
//                 let len = control.encoded_len();
//                 create_or_add_rpc!(len);
//                 rpc_list.last_mut().expect("Always an element").control = Some(control.clone());
//             }
//         }
//
//         Ok(rpc_list)
//     }
// }
//
// impl NetworkBehaviour for MaliciousBehaviour {
//     // Using `GossipsubHandler` which is made public in `libp2p_testground` crate.
//     type ConnectionHandler = GossipsubHandler;
//     type OutEvent = GossipsubEvent;
//
//     fn new_handler(&mut self) -> Self::ConnectionHandler {
//         let protocol_config = ProtocolConfig::new(
//             self.config.protocol_id().clone(),
//             self.config.custom_id_version().clone(),
//             self.config.max_transmit_size(),
//             self.config.validation_mode().clone(),
//             self.config.support_floodsub(),
//         );
//
//         GossipsubHandler::new(protocol_config, self.config.idle_timeout())
//     }
//
//     fn inject_event(
//         &mut self,
//         propagation_source: PeerId,
//         _connection: ConnectionId,
//         handler_event: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
//     ) {
//         match handler_event {
//             HandlerEvent::Message {
//                 rpc,
//                 invalid_messages: _,
//             } => {
//                 // Handle subscriptions
//                 if !rpc.subscriptions.is_empty() {
//                     self.handle_received_subscriptions(&rpc.subscriptions, &propagation_source);
//                 }
//
//                 // Handle control messages
//                 let mut prune_msgs = vec![];
//                 for control_msg in rpc.control_msgs {
//                     match control_msg {
//                         GossipsubControlAction::IHave { .. }
//                         | GossipsubControlAction::IWant { .. }
//                         | GossipsubControlAction::Graft { .. } => {}
//                         GossipsubControlAction::Prune {
//                             topic_hash,
//                             peers,
//                             backoff,
//                         } => prune_msgs.push((topic_hash, peers, backoff)),
//                     }
//                 }
//                 if !prune_msgs.is_empty() {
//                     self.handle_prune(&propagation_source, prune_msgs);
//                 }
//             }
//             HandlerEvent::PeerKind(_) => {}
//         }
//     }
//
//     fn poll(
//         &mut self,
//         cx: &mut Context<'_>,
//         _params: &mut impl PollParameters,
//     ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
//         if let Some(event) = self.events.pop_front() {
//             return Poll::Ready(event.map_in(|e: Arc<GossipsubHandlerIn>| {
//                 // clone send event reference if others references are present
//                 Arc::try_unwrap(e).unwrap_or_else(|e| (*e).clone())
//             }));
//         }
//
//         loop {
//             match self.regraft.poll_next_unpin(cx) {
//                 Poll::Ready(Some(Ok((peer_id, topic_hash)))) => {
//                     if let Err(error) = self.send_message(
//                         peer_id,
//                         GossipsubRpc {
//                             subscriptions: Vec::new(),
//                             messages: Vec::new(),
//                             control_msgs: vec![GossipsubControlAction::Graft { topic_hash }],
//                         }
//                         .into_protobuf(),
//                     ) {
//                         error!("Failed sending grafts: {}", error);
//                     }
//                 }
//                 Poll::Ready(Some(Err(error))) => {
//                     error!("Failed to check for peers to re-GRAFT: {}", error);
//                 }
//                 Poll::Ready(None) | Poll::Pending => break,
//             }
//         }
//
//         Poll::Pending
//     }
// }
