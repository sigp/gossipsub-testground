use libp2p::futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use testground::client::Client;

// States for `barrier()`
pub(crate) const BARRIER_LIBP2P_READY: &str = "Started libp2p";
pub(crate) const BARRIER_TOPOLOGY_READY: &str = "Topology generated";

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
