use crate::utils::{record_instance_info, BARRIER_LIBP2P_READY, BARRIER_TOPOLOGY_READY};
use crate::InstanceInfo;
use gen_topology::Params;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::SwarmEvent;
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::PeerId;
use libp2p::Transport;
use npg::slot_generator::{Subnet, ValId};
use npg::Message;
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use testground::client::Client;
use tracing::{error, info};

use super::metrics;
use super::Network;
use super::Topic;

pub(crate) fn parse_params(
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

/// The main entry point of the sim.
pub(crate) async fn run(
    client: Client,
    node_id: usize,
    instance_info: InstanceInfo,
    participants: HashMap<usize, InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
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
    info!("[{}] Validators on this node: {:?}", node_id, validator_set);
    let validator_set: HashSet<ValId> =
        validator_set.into_iter().map(|v| ValId(v as u64)).collect();

    record_instance_info(
        &client,
        node_id,
        &instance_info.peer_id,
        &client.run_parameters().test_run,
    )
    .await?;

    let registry: Registry<Box<dyn EncodeMetric>> = Registry::default();
    let mut network = Network::new(
        registry,
        keypair,
        node_id,
        instance_info,
        participants.clone(),
        client.clone(),
        validator_set,
        params,
    );

    client
        .signal_and_wait(BARRIER_TOPOLOGY_READY, test_instance_count)
        .await?;

    // Set up the listening address
    network.start_libp2p().await;

    client
        .signal_and_wait(BARRIER_LIBP2P_READY, test_instance_count)
        .await?;
    // Dial the designated outbound peers
    network.dial_peers(outbound_peers).await;

    client
        .signal_and_wait(
            BARRIER_TOPOLOGY_READY,
            client.run_parameters().test_instance_count,
        )
        .await?;

    if let Err(e) = network.subscribe_topics() {
        error!("[{}] Failed to subscribe to topics {e}", network.node_id);
    };
    network.run_sim(run_duration).await;

    client.record_success().await?;
    Ok(())
}

/// Set up an encrypted TCP transport over the Mplex and Yamux protocols.
pub fn build_transport(
    keypair: &Keypair,
) -> libp2p::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
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

impl Network {
    // Starts libp2p
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

    // Main execution loop of the sim.
    async fn run_sim(&mut self, run_duration: Duration) {
        let deadline = tokio::time::sleep(run_duration);
        futures::pin_mut!(deadline);

        // Initialise some metrics
        // This just adds the 0 value to some counters.

        self.influx_db_handles.push(tokio::spawn(metrics::initialise_metrics(self.record_metrics_info())));

        loop {
            tokio::select! {
            _ = deadline.as_mut() => {
                // Sim complete
                break;
            }
            Some(m) = self.messages_gen.next() => {
                let payload = m.payload();
                let (topic, val) = match m {
                    Message::BeaconBlock { proposer: ValId(v), slot: _ } => {
                        (Topic::Blocks, v)

                    },
                    Message::AggregateAndProofAttestation { aggregator: ValId(v), subnet: Subnet(_s), slot: _ } => {
                        (Topic::Aggregates, v)
                    },
                    Message::Attestation { attester: ValId(v), subnet: Subnet(s), slot: _ } => {
                        (Topic::Attestations(s), v)
                    },
                    Message::SignedContributionAndProof { validator: ValId(v), subnet: Subnet(s), slot: _ } => {
                        (Topic::SignedContributionAndProof(s), v)
                    },
                    Message::SyncCommitteeMessage { validator: ValId(v), subnet: Subnet(s), slot: _ } => {
                        (Topic::SyncMessages(s), v)
                    },
                };
                if let Err(e) = self.publish(topic.clone(), val, payload) {
                    error!("Failed to publish message {e} to topic {topic:?}");
                }

            }
            // Record peer scores
            _ = self.metrics_interval.tick() => {
                let metrics_info = self.record_metrics_info();
                // Spawn into its own task
                self.influx_db_handles.push(tokio::spawn(metrics::record_metrics(metrics_info)));
            },
            _ = self.influx_db_handles.next() => {} // Remove excess db handles
            event = self.swarm.select_next_some() => self.handle_swarm_event(event),
            }
        }

        // Waiting for influx db handles to end
        while let Some(_) = self.influx_db_handles.next().await {}

    }
}
