mod network;

use crate::network::Network;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::FloodPublish;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use testground::client::Client;
use testground::network_conf::{
    FilterAction, LinkShape, NetworkConfiguration, RoutingPolicyType, DEFAULT_DATA_NETWORK,
};

// States for `barrier()`
pub(crate) const BARRIER_NETWORK_READY: &str = "Network configured";
pub(crate) const BARRIER_LIBP2P_READY: &str = "Started libp2p";
pub(crate) const BARRIER_TOPOLOGY_READY: &str = "Topology generated";
pub(crate) const BARRIER_SUBSCRIBED: &str = "Subscribed";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let client = Client::new_and_init().await?;

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Parse test parameters
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let test_instance_params = client.run_parameters().test_instance_params;
    let warm_up = Duration::from_secs(get_param::<u64>("warm_up", &test_instance_params)?);
    let run = Duration::from_secs(get_param::<u64>("run", &test_instance_params)?);
    let publish_interval =
        Duration::from_secs(get_param::<u64>("publish_interval", &test_instance_params)?);
    let bandwidth = get_param::<u64>("bandwidth", &test_instance_params)?;
    let flood_publish = match get_param::<String>("flood_publish", &test_instance_params)
        .unwrap()
        .as_str()
    {
        "rapid" => FloodPublish::Rapid,
        "heartbeat" => FloodPublish::Heartbeat(0),
        _ => panic!("Unknown flood publish type"),
    };

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Configure network
    // /////////////////////////////////////////////////////////////////////////////////////////////
    client
        .configure_network(NetworkConfiguration {
            network: DEFAULT_DATA_NETWORK.to_owned(),
            ipv4: None,
            ipv6: None,
            enable: true,
            default: LinkShape {
                latency: 1 * 1_000_000, // 1 msec
                jitter: 0,
                bandwidth: bandwidth * 1024 * 1024, // MiB
                filter: FilterAction::Accept,
                loss: 0.0,
                corrupt: 0.0,
                corrupt_corr: 0.0,
                reorder: 0.0,
                reorder_corr: 0.0,
                duplicate: 0.0,
                duplicate_corr: 0.0,
            },
            rules: None,
            callback_state: BARRIER_NETWORK_READY.to_owned(),
            callback_target: None,
            routing_policy: RoutingPolicyType::DenyAll,
        })
        .await?;

    client
        .barrier(
            BARRIER_NETWORK_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Start libp2p and dial peers
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let keypair = Keypair::generate_ed25519();
    let peer_id = PeerId::from(keypair.public());
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

    let participants = participants(
        "peer_info",
        &client,
        (peer_id.clone(), multiaddr.clone()),
        client.run_parameters().test_group_instance_count as usize,
    )
    .await?;

    let is_publisher = client.group_seq() == 1;

    println!(
        r#"[flood_publishing_test]{{"event":"peer_id","peer_id":"{peer_id}","is_publisher":{}}}"#,
        is_publisher
    );

    let mut network = Network::new(
        keypair,
        is_publisher,
        (peer_id, multiaddr),
        participants,
        flood_publish,
    );

    network.start_libp2p().await;

    client
        .signal_and_wait(
            BARRIER_LIBP2P_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

    network.dial_peers();

    client
        .signal_and_wait(
            BARRIER_TOPOLOGY_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

    network.subscribe()?;
    network.warm_up(warm_up).await;

    client
        .signal_and_wait(
            BARRIER_SUBSCRIBED,
            client.run_parameters().test_instance_count,
        )
        .await?;

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Run simulation
    // /////////////////////////////////////////////////////////////////////////////////////////////
    network.run_sim(run, publish_interval).await;

    client.record_success().await?;
    Ok(())
}

fn get_param<T: FromStr>(k: &str, instance_params: &HashMap<String, String>) -> Result<T, String> {
    instance_params
        .get(k)
        .ok_or(format!("{k} is not specified."))?
        .parse::<T>()
        .map_err(|_| format!("Failed to parse instance_param. key: {}", k))
}

async fn participants(
    topic: &'static str,
    client: &Client,
    info: (PeerId, Multiaddr),
    count: usize,
) -> Result<Vec<(PeerId, Multiaddr)>, Box<dyn std::error::Error>> {
    let infos = publish_and_collect(topic, &client, info.clone(), count).await?;

    Ok(infos
        .into_iter()
        .filter(|(peer_id, _)| peer_id != &info.0)
        .collect::<Vec<_>>())
}

async fn publish_and_collect<T: Serialize + DeserializeOwned>(
    topic: &'static str,
    client: &Client,
    info: T,
    count: usize,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    let serialized = Cow::Owned(serde_json::to_value(&info)?);
    client.publish(topic, serialized).await?;

    let mut stream = client.subscribe(topic, count * 2).await;

    let mut vec: Vec<T> = Vec::with_capacity(count);

    for _ in 0..count {
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
