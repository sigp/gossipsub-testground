use crate::utils::{
    initialise_counter, queries_for_counter, queries_for_counter_join, queries_for_gauge,
    queries_for_histogram,
};
use crate::InstanceInfo;
use chrono::{DateTime, Utc};
use libp2p::gossipsub::IdentTopic;
use prometheus_client::encoding::proto::openmetrics_data_model::MetricSet;
use std::sync::Arc;
use testground::client::Client;
use tracing::error;

pub use super::Network;
use super::Topic;
use super::{ATTESTATION_SUBNETS, SYNC_SUBNETS};

// A context struct for passing information into the `record_metrics` function that can be spawned
// into its own task.
pub(crate) struct RecordMetricsInfo {
    client: Arc<Client>,
    metrics: MetricSet,
    node_id: usize,
    instance_info: InstanceInfo,
    current: DateTime<Utc>,
}

impl Network {
    // Generates the necessary amount of information to record metrics.
    pub(super) fn record_metrics_info(&self) -> RecordMetricsInfo {
        // Encode the metrics to an instance of the OpenMetrics protobuf format.
        // https://github.com/OpenObservability/OpenMetrics/blob/main/proto/openmetrics_data_model.proto
        let metrics = prometheus_client::encoding::proto::encode(&self.registry);

        let elapsed = chrono::Duration::from_std(self.local_start_time.elapsed())
            .expect("Durations are small");
        let current = self.start_time + elapsed;

        RecordMetricsInfo {
            client: self.client.clone(),
            metrics,
            node_id: self.node_id,
            instance_info: self.instance_info.clone(),
            current,
        }
    }
}

/// Initialises some counters to the 0 value.
pub(crate) async fn initialise_metrics(info: RecordMetricsInfo) {
    let mut queries = vec![];
    let run_id = &info.client.run_parameters().test_run;
    let current = info.current;
    let node_id = info.node_id;
    let instance_info = &info.instance_info;

    let to_initialise_metrics = [
        "topic_msg_published",
        "topic_msg_recv_counts",
        "topic_msg_recv_counts_unfiltered",
        "topic_msg_recv_duplicates",
        "topic_msg_sent_bytes",
        "topic_msg_recv_bytes",
        "episub_current_choked_peers_in_mesh",
        "episub_received_unchoke_messages",
        "topic_iwant_msgs",
        "episub_mesh_additions",
        "episub_received_choke_messages"
    ];

    for name in to_initialise_metrics {
        let mut topics = vec![Topic::Blocks, Topic::Aggregates];
        for x in 0..ATTESTATION_SUBNETS {
            topics.push(Topic::Attestations(x));
        }
        for x in 0..SYNC_SUBNETS {
            topics.push(Topic::SyncMessages(x));
            topics.push(Topic::SignedContributionAndProof(x));
        }

        for topic in topics
            .into_iter()
            .map(|t| IdentTopic::from(t).hash().into_string())
        {
            queries.push(initialise_counter(
                &current,
                name.into(),
                topic,
                node_id,
                instance_info,
                run_id,
            ));
        }
    }

    for query in queries {
        if let Err(e) = info.client.record_metric(query).await {
            error!("Failed to record metrics: {:?}", e);
        }
    }
}

// Records all the metrics into InfluxDB
pub(crate) async fn record_metrics(info: RecordMetricsInfo) {
    let run_id = &info.client.run_parameters().test_run;

    // Encode the metrics to an instance of the OpenMetrics protobuf format.
    // https://github.com/OpenObservability/OpenMetrics/blob/main/proto/openmetrics_data_model.proto
    let metric_set = info.metrics;

    let mut queries = vec![];
    let current = info.current;
    let node_id = info.node_id;

    for family in metric_set.metric_families.iter() {
        let q = match family.name.as_str() {
            // ///////////////////////////////////
            // Metrics per known topic
            // ///////////////////////////////////
            "topic_subscription_status" => Some(queries_for_gauge(
                &current,
                family,
                node_id,
                &info.instance_info,
                run_id,
                "status",
            )),
            "topic_peers_counts" => Some(queries_for_gauge(
                &current,
                family,
                node_id,
                &info.instance_info,
                run_id,
                "count",
            )),
            "invalid_messages_per_topic"
            | "accepted_messages_per_topic"
            | "ignored_messages_per_topic"
            | "rejected_messages_per_topic" => {
                Some(queries_for_counter(&current, family, node_id, &info.instance_info, run_id))
            }
            // ///////////////////////////////////
            // Metrics regarding mesh state
            // ///////////////////////////////////
            "mesh_peer_counts" => Some(queries_for_gauge(
                &current,
                family,
                info.node_id,
                &info.instance_info,
                run_id,
                "count",
            )),
            "mesh_peer_inclusion_events" => {
                Some(queries_for_counter(&current, family, info.node_id, &info.instance_info, run_id))
            }
            "mesh_peer_churn_events" => {
                Some(queries_for_counter(&current, family, info.node_id, &info.instance_info, run_id))
            }
            // ///////////////////////////////////
            // Metrics regarding messages sent/received
            // ///////////////////////////////////
            "topic_msg_sent_counts"
            | "topic_msg_published"
            | "topic_msg_sent_bytes"
            | "topic_msg_recv_counts_unfiltered"
            | "topic_msg_recv_counts"
            | "topic_msg_recv_bytes" => {
                Some(queries_for_counter(&current, family, info.node_id, &info.instance_info, run_id))
            }

            "topic_msg_last_sent_bytes"
            | "topic_msg_last_recv_bytes"
            | "topic_msg_last_recv_bytes_unfiltered" => Some(queries_for_gauge(
                &current,
                family,
                info.node_id,
                &info.instance_info,
                run_id,
                "count",
            )),
            // ///////////////////////////////////
            // Metrics related to scoring
            // ///////////////////////////////////
            "memcache_size" => Some(queries_for_gauge(
                &current,
                family,
                info.node_id,
                &info.instance_info,
                run_id,
                "count",
            )),
            "score_per_mesh" => {
                Some(queries_for_histogram(&current, family, info.node_id, &info.instance_info, run_id))
            }
            "scoring_penalties" => {
                Some(queries_for_counter(&current, family, info.node_id, &info.instance_info, run_id))
            }
            // ///////////////////////////////////
            // General Metrics
            // ///////////////////////////////////
            "peers_per_protocol" => Some(queries_for_gauge(
                &current,
                family,
                info.node_id,
                &info.instance_info,
                run_id,
                "peers",
            )),
            "heartbeat_duration" => {
                Some(queries_for_histogram(&current, family, info.node_id, &info.instance_info, run_id))
            }
            // ///////////////////////////////////
            // Performance metrics
            // ///////////////////////////////////
            "topic_iwant_msgs" => {
                Some(queries_for_counter(&current, family, info.node_id, &info.instance_info, run_id))
            }
            "memcache_misses" => {
                Some(queries_for_counter(&current, family, info.node_id, &info.instance_info, run_id))
            }
            // ///////////////////////////////////
            // Episub Metrics
            // ///////////////////////////////////
            "episub_current_choked_peers_in_mesh"
            | "episub_current_peers_choked_us"
            | "episub_metrics_cache_size"
            | "episub_metrics_ihave_cache_size" => Some(queries_for_gauge(
                &current,
                family,
                info.node_id,
                &info.instance_info,
                run_id,
                "count",
            )),

            "episub_mesh_additions"
            | "episub_received_choke_messages"
            | "episub_received_unchoke_messages" => {
                Some(queries_for_counter(&current, family, info.node_id, &info.instance_info, run_id))
            }
            "episub_heartbeat_duration" => {
                Some(queries_for_histogram(&current, family, info.node_id, &info.instance_info, run_id))
            }
            "episub_mesh_message_latency"
            | "episub_ihave_message_stats" => { None } // Can't graph so currently useless to us 
            x => unreachable!("Metric {} is unknown", x),
        };

        if let Some(q) = q {
            queries.extend(q);
        }
    }

    // We can't do joins in InfluxDB easily, so do some custom queries here to calculate
    // duplicates.
    let recvd_unfiltered = metric_set
        .metric_families
        .iter()
        .find(|family| family.name.as_str() == "topic_msg_recv_counts_unfiltered");

    if let Some(recvd_unfiltered) = recvd_unfiltered {
        let recvd = metric_set
            .metric_families
            .iter()
            .find(|family| family.name.as_str() == "topic_msg_recv_counts");
        if let Some(recvd) = recvd {
            let q = queries_for_counter_join(
                &current,
                recvd_unfiltered,
                recvd,
                "topic_msg_recv_duplicates",
                info.node_id,
                &info.instance_info,
                run_id,
                |a, b| a.saturating_sub(b),
            );

            queries.extend(q);
        }
    }

    for query in queries {
        if let Err(e) = info.client.record_metric(query).await {
            error!("Failed to record metrics: {:?}", e);
        }
    }
}
