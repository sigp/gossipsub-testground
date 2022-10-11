use libp2p::futures::FutureExt;
use libp2p::futures::{Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use std::fmt::Debug;
use testground::client::Client;
use tracing::{debug, info};

// States for `barrier()`
pub(crate) const BARRIER_TOPOLOGY_READY: &str = "Started libp2p, dialed outboud peers";
pub(crate) const BARRIER_WARMUP: &str = "Warmup";
pub(crate) const BARRIER_DONE: &str = "Done";

/// Publish info and collect it from the participants. The return value includes one published by
/// myself.
pub(crate) async fn publish_and_collect<T: Serialize + DeserializeOwned>(
    topic: &'static str,
    client: &Client,
    info: T,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    let instance_count = client.run_parameters().test_instance_count as usize;
    let serialized = Cow::Owned(serde_json::to_value(&info)?);
    client.publish(topic, serialized).await?;

    let mut stream = client.subscribe(topic, instance_count * 2).await;

    let mut vec: Vec<T> = Vec::with_capacity(instance_count);

    for _ in 0..instance_count {
        match stream.next().await {
            Some(Ok(other)) => {
                let info: T = serde_json::from_value(other)?;
                vec.push(info);
            }
            Some(Err(e)) => return Err(Box::new(e)),
            None => unreachable!(),
        }
    }

    Ok(vec)
}

/// Sets a barrier on the supplied state that fires when it reaches all participants.
pub(crate) async fn barrier_and_drive_swarm<
    T: StreamExt + Unpin + libp2p::futures::stream::FusedStream,
>(
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
