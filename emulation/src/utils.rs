use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::{Stream, StreamExt};
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, IdentityTransform, MessageAuthenticity,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::SwarmBuilder;
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::{PeerId, Swarm, Transport};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use std::fmt::Debug;
use std::time::Duration;
use testground::client::Client;

// States for `barrier()`
pub(crate) const BARRIER_STARTED_LIBP2P: &str = "Started libp2p";
pub(crate) const BARRIER_DIALED: &str = "Dialed";
pub(crate) const BARRIER_DONE: &str = "Done";

// The backoff time for pruned peers.
pub(crate) const PRUNE_BACKOFF: u64 = 60;

// Publish info and collect it from the participants. The return value includes one published by
// myself.
pub(crate) async fn publish_and_collect<T: Serialize + DeserializeOwned>(
    client: &Client,
    info: T,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    const TOPIC: &str = "publish_and_collect";

    client.publish(TOPIC, serde_json::to_string(&info)?).await?;

    let mut stream = client.subscribe(TOPIC).await;

    let mut vec: Vec<T> = vec![];

    for _ in 0..client.run_parameters().test_instance_count {
        match stream.next().await {
            Some(Ok(other)) => {
                let info: T = serde_json::from_str(&other)?;
                vec.push(info);
            }
            Some(Err(e)) => return Err(Box::new(e)),
            None => unreachable!(),
        }
    }

    Ok(vec)
}

pub(crate) fn build_swarm(keypair: Keypair) -> Swarm<Gossipsub> {
    // Build a Gossipsub network behaviour.
    let gossipsub_config = GossipsubConfigBuilder::default()
        .prune_backoff(Duration::from_secs(PRUNE_BACKOFF))
        .history_length(12)
        .build()
        .expect("Valid configuration");
    let gossipsub = Gossipsub::new_with_subscription_filter_and_transform(
        MessageAuthenticity::Signed(keypair.clone()),
        gossipsub_config,
        None,
        AllowAllSubscriptionFilter {},
        IdentityTransform {},
    )
    .expect("Valid configuration");

    SwarmBuilder::new(
        build_transport(&keypair),
        gossipsub,
        PeerId::from(keypair.public()),
    )
    .executor(Box::new(|future| {
        tokio::spawn(future);
    }))
    .build()
}

// Set up an encrypted TCP transport over the Mplex and Yamux protocols.
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
        .timeout(std::time::Duration::from_secs(20))
        .boxed()
}

// Sets a barrier on the supplied state that fires when it reaches all participants.
pub(crate) async fn barrier<T: StreamExt + Unpin + libp2p::futures::stream::FusedStream>(
    client: &Client,
    swarm: &mut T,
    state: impl Into<Cow<'static, str>> + Copy,
) where
    <T as Stream>::Item: Debug,
{
    loop {
        tokio::select! {
            _ = client.signal_and_wait(state, client.run_parameters().test_instance_count) => {
                break;
            }
            // Record the Swarm events that happen while waiting for the barrier.
            event = swarm.select_next_some() => {
                client.record_message(format!("{:?}", event));
            }
        }
    }
}
