use crate::utils::{barrier, BARRIER_DIALED, BARRIER_DONE, BARRIER_STARTED_LIBP2P};
use crate::InstanceInfo;
use libp2p_testground::core::connection::ConnectionId;
use libp2p_testground::core::muxing::StreamMuxerBox;
use libp2p_testground::core::upgrade::{SelectUpgrade, Version};
use libp2p_testground::dns::TokioDnsConfig;
use libp2p_testground::gossipsub::handler::GossipsubHandler;
use libp2p_testground::gossipsub::protocol::ProtocolConfig;
use libp2p_testground::gossipsub::GossipsubConfig;
use libp2p_testground::identity::Keypair;
use libp2p_testground::mplex::MplexConfig;
use libp2p_testground::noise::{NoiseConfig, X25519Spec};
use libp2p_testground::swarm::{
    ConnectionHandler, IntoConnectionHandler, NetworkBehaviour, NetworkBehaviourAction,
    PollParameters, SwarmBuilder,
};
use libp2p_testground::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p_testground::yamux::YamuxConfig;
use libp2p_testground::Transport;
use libp2p_testground::{PeerId, Swarm};
use std::task::{Context, Poll};
use std::time::Duration;
use testground::client::Client;

// In this `attacker` module, we use `libp2p_testground` instead of `libp2p`.
// `libp2p_testground` is a fork of `libp2p`, some its module (e.g. `handler`) made public to
// implement `MaliciousBehaviour`.
// See https://github.com/ackintosh/rust-libp2p/pull/45 for changes made in `libp2p_testground`.

pub(crate) async fn run(
    client: Client,
    instance_info: InstanceInfo,
    participants: Vec<InstanceInfo>,
    // NOTE: this is a `Keypair` struct in `libp2p` (not `libp2p_testground`) which has been
    // generated in the main function.
    keypair: libp2p::identity::Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    // Convert the `keypair` from `libp2p::identity::Keypair` to `libp2p_testground` one.
    let keypair =
        Keypair::from_protobuf_encoding(&keypair.to_protobuf_encoding().unwrap()).unwrap();

    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    let mut swarm = build_swarm(keypair);
    swarm
        .listen_on(instance_info.multiaddr.clone())
        .expect("Swarm starts listening");

    barrier(&client, &mut swarm, BARRIER_STARTED_LIBP2P).await;

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Setup discovery
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let victim = {
        participants
            .iter()
            .find(|&info| info.role.is_publisher() && info.group_seq == 1)
            .expect("publisher should exists")
            .clone()
    };
    client.record_message(format!("Victim: {:?}", victim));

    swarm.dial(victim.multiaddr)?;

    barrier(&client, &mut swarm, BARRIER_DIALED).await;

    barrier(&client, &mut swarm, BARRIER_DONE).await;
    client.record_success().await?;
    Ok(())
}

fn build_swarm(keypair: Keypair) -> Swarm<MaliciousBehaviour> {
    SwarmBuilder::new(
        build_transport(&keypair),
        MaliciousBehaviour {},
        PeerId::from(keypair.public()),
    )
    .executor(Box::new(|future| {
        tokio::spawn(future);
    }))
    .build()
}

fn build_transport(
    keypair: &Keypair,
) -> libp2p_testground::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
    let transport = TokioDnsConfig::system(TokioTcpTransport::new(
        GenTcpConfig::default().nodelay(true),
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

pub struct MaliciousBehaviour {}

impl NetworkBehaviour for MaliciousBehaviour {
    // Using `GossipsubHandler` which is made public in `libp2p_testground` crate.
    type ConnectionHandler = GossipsubHandler;
    type OutEvent = ();

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        let config = GossipsubConfig::default();
        let protocol_config = ProtocolConfig::new(
            config.protocol_id().clone(),
            config.custom_id_version().clone(),
            config.max_transmit_size(),
            config.validation_mode().clone(),
            config.support_floodsub(),
        );

        GossipsubHandler::new(protocol_config, config.idle_timeout())
    }

    fn inject_event(
        &mut self,
        _peer_id: PeerId,
        _connection: ConnectionId,
        _event: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
    ) {
        todo!()
    }

    fn poll(
        &mut self,
        _cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
        todo!()
    }
}
