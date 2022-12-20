use crate::beacon_node::{ATTESTATION_SUBNETS, SLOT, SLOTS_PER_EPOCH, SYNC_SUBNETS};
use crate::topic::Topic;
use gen_topology::Params;
use libp2p::gossipsub::{
    IdentTopic, PeerScoreParams, PeerScoreThresholds, TopicHash, TopicScoreParams,
};
use rand::Rng;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

/// Parse `test_instance_params` and returns `gen_topology::Params`
pub(crate) fn parse_topology_params(
    total_nodes: usize,
    instance_params: HashMap<String, String>,
) -> Result<(Duration, Params), Box<dyn std::error::Error>> {
    let seed = get_param::<u64>("seed", &instance_params)?;
    let no_val_percentage = get_param::<usize>("no_val_percentage", &instance_params)?.min(100);
    let total_validators = get_param::<usize>("total_validators", &instance_params)?;
    let min_peers_per_node = get_param::<usize>("min_peers_per_node", &instance_params)?;
    let max_peers_per_node_inclusive =
        get_param::<usize>("max_peers_per_node_inclusive", &instance_params)?;
    let total_nodes_without_vals = total_nodes * no_val_percentage / 100;
    let total_nodes_with_vals = total_nodes - total_nodes_without_vals;
    let run = get_param::<u64>("run", &instance_params)?;

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

/// Parse `test_instance_params` and returns `PeerScoreThresholds`.
pub(crate) fn parse_peer_score_thresholds(
    instance_params: &HashMap<String, String>,
) -> Result<PeerScoreThresholds, Box<dyn std::error::Error>> {
    Ok(PeerScoreThresholds {
        gossip_threshold: get_param::<f64>("gossip_threshold", instance_params)?,
        publish_threshold: get_param::<f64>("publish_threshold", instance_params)?,
        graylist_threshold: get_param::<f64>("graylist_threshold", instance_params)?,
        accept_px_threshold: get_param::<f64>("accept_px_threshold", instance_params)?,
        opportunistic_graft_threshold: get_param::<f64>(
            "opportunistic_graft_threshold",
            instance_params,
        )?,
    })
}

/// Parse `test_instance_params` and returns `PeerScoreParams`.
pub(crate) fn build_peer_score_params(
    instance_params: &HashMap<String, String>,
) -> PeerScoreParams {
    let mut params = parse_peer_score_params(instance_params).expect("Valid peer score params");

    let get_hash = |topic: Topic| -> TopicHash {
        let ident_topic: IdentTopic = topic.into();
        ident_topic.hash()
    };

    // BeaconBlock
    params.topics.insert(
        get_hash(Topic::Blocks),
        parse_topic_score_params("bb", instance_params).expect("Valid topic params"),
    );

    // BeaconAggregateAndProof and Attestation
    let beacon_aggregate_proof_param =
        parse_topic_score_params("baap", instance_params).expect("Valid topic params");
    let beacon_attestation_subnet_param =
        parse_topic_score_params("a", instance_params).expect("Valid topic params");

    for subnet_n in 0..ATTESTATION_SUBNETS {
        params.topics.insert(
            get_hash(Topic::Aggregates(subnet_n)),
            beacon_aggregate_proof_param.clone(),
        );
        params.topics.insert(
            get_hash(Topic::Attestations(subnet_n)),
            beacon_attestation_subnet_param.clone(),
        );
    }

    // SignedContributionAndProof
    if get_param::<bool>("scap_enable_topic_params", instance_params).expect("Valid param") {
        let signed_contribution_and_proof_subnet_param =
            parse_topic_score_params("scap", instance_params).expect("Valid topic params");

        for subnet_n in 0..SYNC_SUBNETS {
            params.topics.insert(
                get_hash(Topic::SignedContributionAndProof(subnet_n)),
                signed_contribution_and_proof_subnet_param.clone(),
            );
        }
    }

    // SyncCommitteeMessage
    if get_param::<bool>("scm_enable_topic_params", instance_params).expect("Valid param") {
        let sync_committee_message_subnet_param =
            parse_topic_score_params("scm", instance_params).expect("Valid topic params");

        for subnet_n in 0..SYNC_SUBNETS {
            params.topics.insert(
                get_hash(Topic::SyncMessages(subnet_n)),
                sync_committee_message_subnet_param.clone(),
            );
        }
    }

    params
}

fn parse_peer_score_params(
    instance_params: &HashMap<String, String>,
) -> Result<PeerScoreParams, Box<dyn std::error::Error>> {
    Ok(PeerScoreParams {
        topic_score_cap: get_param::<f64>("topic_score_cap", instance_params)?,
        decay_interval: Duration::from_secs(get_param::<u64>("decay_interval", instance_params)?),
        decay_to_zero: get_param::<f64>("decay_to_zero", instance_params)?,
        retain_score: Duration::from_secs(get_param::<u64>("retain_score", instance_params)?),
        // P5: Application-specific peer scoring
        app_specific_weight: get_param::<f64>("app_specific_weight", instance_params)?,
        // P6: IP-colocation factor.
        ip_colocation_factor_weight: get_param::<f64>(
            "ip_colocation_factor_weight",
            instance_params,
        )?,
        ip_colocation_factor_threshold: get_param::<f64>(
            "ip_colocation_factor_threshold",
            instance_params,
        )?,
        // P7: behavioural pattern penalties.
        behaviour_penalty_weight: get_param::<f64>("behaviour_penalty_weight", instance_params)?,
        behaviour_penalty_threshold: get_param::<f64>(
            "behaviour_penalty_threshold",
            instance_params,
        )?,
        behaviour_penalty_decay: get_param::<f64>("behaviour_penalty_decay", instance_params)?,
        ..Default::default()
    })
}

fn parse_topic_score_params(
    prefix: &str,
    instance_params: &HashMap<String, String>,
) -> Result<TopicScoreParams, Box<dyn std::error::Error>> {
    Ok(TopicScoreParams {
        topic_weight: get_param::<f64>(&format!("{prefix}_topic_weight"), instance_params)?,
        // P1: time in the mesh
        time_in_mesh_weight: get_param::<f64>(
            &format!("{prefix}_time_in_mesh_weight"),
            instance_params,
        )?,
        time_in_mesh_quantum: {
            let n = get_param::<u64>(&format!("{prefix}_time_in_mesh_quantum"), instance_params)?;
            Duration::from_secs(SLOT * n)
        },
        time_in_mesh_cap: get_param::<f64>(&format!("{prefix}_time_in_mesh_cap"), instance_params)?,
        // P2: first message deliveries
        first_message_deliveries_weight: get_param::<f64>(
            &format!("{prefix}_first_message_deliveries_weight"),
            instance_params,
        )?,
        first_message_deliveries_decay: get_param::<f64>(
            &format!("{prefix}_first_message_deliveries_decay"),
            instance_params,
        )?,
        first_message_deliveries_cap: get_param::<f64>(
            &format!("{prefix}_first_message_deliveries_cap"),
            instance_params,
        )?,
        // P3: mesh message deliveries
        mesh_message_deliveries_weight: get_param::<f64>(
            &format!("{prefix}_mesh_message_deliveries_weight"),
            instance_params,
        )?,
        mesh_message_deliveries_decay: get_param::<f64>(
            &format!("{prefix}_mesh_message_deliveries_decay"),
            instance_params,
        )?,
        mesh_message_deliveries_threshold: get_param::<f64>(
            &format!("{prefix}_mesh_message_deliveries_threshold"),
            instance_params,
        )?,
        mesh_message_deliveries_cap: get_param::<f64>(
            &format!("{prefix}_mesh_message_deliveries_cap"),
            instance_params,
        )?,
        mesh_message_deliveries_activation: {
            let n = get_param::<u64>(
                &format!("{prefix}_mesh_message_deliveries_activation"),
                instance_params,
            )?;
            Duration::from_secs(SLOT * SLOTS_PER_EPOCH * n)
        },
        mesh_message_deliveries_window: {
            let n = get_param::<u64>(
                &format!("{prefix}_mesh_message_deliveries_window"),
                instance_params,
            )?;
            Duration::from_secs(n)
        },
        // P3b: sticky mesh propagation failures
        mesh_failure_penalty_weight: get_param::<f64>(
            &format!("{prefix}_mesh_failure_penalty_weight"),
            instance_params,
        )?,
        mesh_failure_penalty_decay: get_param::<f64>(
            &format!("{prefix}_mesh_failure_penalty_decay"),
            instance_params,
        )?,
        // P4: invalid messages
        invalid_message_deliveries_weight: get_param::<f64>(
            &format!("{prefix}_invalid_message_deliveries_weight"),
            instance_params,
        )?,
        invalid_message_deliveries_decay: get_param::<f64>(
            &format!("{prefix}_invalid_message_deliveries_decay"),
            instance_params,
        )?,
    })
}

fn get_param<T: FromStr>(k: &str, instance_params: &HashMap<String, String>) -> Result<T, String> {
    instance_params
        .get(k)
        .ok_or(format!("{k} is not specified."))?
        .parse::<T>()
        .map_err(|_| format!("Failed to parse instance_param. key: {}", k))
}

#[derive(Debug)]
pub(crate) struct NetworkParams {
    /// Network latency between nodes in millisecond.
    pub(crate) latency: u64,
    /// Bandwidth in MiB.
    pub(crate) bandwidth: u64,
}

impl NetworkParams {
    pub(crate) fn new(
        instance_params: &HashMap<String, String>,
    ) -> Result<NetworkParams, Box<dyn std::error::Error>> {
        let mut latency = get_param::<u64>("latency", instance_params)?;
        let latency_max = get_param::<u64>("latency_max", instance_params)?;

        if latency < latency_max {
            let mut rng = rand::thread_rng();
            let uniform = rand::distributions::Uniform::new(latency, latency_max);
            latency = rng.sample(uniform);
        }

        let bandwidth = get_param::<u64>("bandwidth", instance_params)?;

        Ok(NetworkParams { latency, bandwidth })
    }
}
