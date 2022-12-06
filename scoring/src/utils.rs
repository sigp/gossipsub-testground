use crate::beacon_node::BeaconNodeInfo;
use chrono::{DateTime, Local};
use libp2p::futures::StreamExt;
use libp2p::PeerId;
use prometheus_client::encoding::proto::openmetrics_data_model::counter_value;
use prometheus_client::encoding::proto::openmetrics_data_model::metric_point;
use prometheus_client::encoding::proto::openmetrics_data_model::Metric;
use prometheus_client::encoding::proto::openmetrics_data_model::MetricFamily;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use testground::client::Client;
use testground::WriteQuery;
use tracing::warn;

// States for `barrier()`
pub(crate) const BARRIER_NETWORK_READY: &str = "Network configured";
pub(crate) const BARRIER_LIBP2P_READY: &str = "Started libp2p";
pub(crate) const BARRIER_TOPOLOGY_READY: &str = "Topology generated";
pub(crate) const BARRIER_SIMULATION_COMPLETED: &str = "Simulation completed";

// Tags for InfluxDB
pub(crate) const TAG_PEER_ID: &str = "peer_id";
pub(crate) const TAG_RUN_ID: &str = "run_id";

/// Publish info and collect it from the participants. The return value includes one published by
/// myself.
pub(crate) async fn publish_and_collect<T: Serialize + DeserializeOwned>(
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

/// Create InfluxDB queries for Counter metrics.
pub(crate) fn queries_for_counter(
    datetime: &DateTime<Local>,
    family: &MetricFamily,
    peer_id: &PeerId,
    run_id: &str,
) -> Vec<WriteQuery> {
    let measurement = format!("{}_{}", env!("CARGO_PKG_NAME"), family.name);
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let mut query = WriteQuery::new((*datetime).into(), &measurement)
            .add_tag(TAG_PEER_ID, peer_id.to_string())
            .add_tag(TAG_RUN_ID, run_id.to_owned())
            .add_field(
                "count",
                get_counter_value(metric).0.expect("should have int value"),
            );

        for l in &metric.labels {
            query = query.add_tag(l.name.clone(), l.value.clone());
        }

        queries.push(query);
    }

    queries
}

/// Create InfluxDB queries joining counter metrics
#[allow(clippy::too_many_arguments)]
pub(crate) fn queries_for_counter_join(
    datetime: &DateTime<Local>,
    family1: &MetricFamily,
    family2: &MetricFamily,
    name: &str,
    peer_id: &PeerId,
    run_id: &str,
    predicate: fn(u64, u64) -> u64,
) -> Vec<WriteQuery> {
    let measurement = format!("{}_{name}", env!("CARGO_PKG_NAME"));
    let mut queries = vec![];

    for metric in family1.metrics.iter() {
        // Match on metric values
        let value = {
            let current_val = get_counter_value(metric).0.expect("should have int value");
            let other_val = family2
                .metrics
                .iter()
                .find(|m2| {
                    // match on all labels
                    let mut found = true;
                    for label in &metric.labels {
                        if !m2
                            .labels
                            .iter()
                            .any(|l| l.name == label.name && l.value == label.value)
                        {
                            found = false;
                            break;
                        }
                    }
                    found
                })
                .and_then(|m| get_counter_value(m).0);
            other_val.map(|other| predicate(current_val, other))
        };

        if let Some(val) = value {
            let mut query = WriteQuery::new((*datetime).into(), &measurement)
                .add_tag(TAG_PEER_ID, peer_id.to_string())
                .add_tag(TAG_RUN_ID, run_id.to_owned())
                .add_field("count", val);

            for l in &metric.labels {
                query = query.add_tag(l.name.clone(), l.value.clone());
            }

            queries.push(query);
        }
    }

    queries
}

fn get_counter_value(metric: &Metric) -> (Option<u64>, Option<f64>) {
    assert_eq!(1, metric.metric_points.len());

    let metric_point = metric.metric_points.first().unwrap();
    let metric_point_value = metric_point.value.as_ref().unwrap().clone();
    match metric_point_value {
        metric_point::Value::CounterValue(counter_value) => match counter_value.total {
            Some(counter_value::Total::IntValue(i)) => (Some(i), None),
            Some(counter_value::Total::DoubleValue(f)) => (None, Some(f)),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

pub(crate) async fn record_run_id(client: &Client) {
    if client.group_seq() != 1 {
        return;
    }

    let measurement = format!("{}_run_id", env!("CARGO_PKG_NAME"));

    let query = WriteQuery::new(Local::now().into(), measurement)
        .add_tag(TAG_RUN_ID, client.run_parameters().test_run)
        .add_field(TAG_RUN_ID, client.run_parameters().test_run);

    if let Err(e) = client.record_metric(query).await {
        warn!("Failed to record run_id: {e:?}");
    }
}

pub(crate) async fn record_victim_id(client: &Client, victim: &BeaconNodeInfo) {
    if client.group_seq() != 1 {
        return;
    }

    let measurement = format!("{}_victim", env!("CARGO_PKG_NAME"));
    let query = WriteQuery::new(Local::now().into(), measurement)
        .add_tag(TAG_RUN_ID, client.run_parameters().test_run)
        .add_field("peer_id", victim.peer_id().to_string());

    if let Err(e) = client.record_metric(query).await {
        warn!(
            "Failed to record victim peer id. peer_id: {}, error: {e:?}",
            victim.peer_id()
        );
    }
}

pub(crate) async fn record_topology_beacon_node(client: &Client, info: &BeaconNodeInfo) {
    let measurement = format!("{}_topology_node", env!("CARGO_PKG_NAME"));
    let title = if info.validators().len() > 0 {
        format!("bn_{}", client.group_seq() - 1)
    } else {
        format!("bn_no_val_{}", client.group_seq() - 1)
    };

    // ref: https://grafana.com/docs/grafana/latest/panels-visualizations/visualizations/node-graph/#node-parameters
    let query = WriteQuery::new(Local::now().into(), measurement)
        .add_tag(TAG_RUN_ID, client.run_parameters().test_run)
        .add_field("id", info.peer_id().to_string())
        .add_field("title", title)
        .add_field("arc__bn", 1);

    if let Err(e) = client.record_metric(query).await {
        warn!("Failed to record topology. error: {e:?}");
    }
}

pub(crate) async fn record_topology_attacker(client: &Client, peer_id: &libp2p_testground::PeerId) {
    let measurement = format!("{}_topology_node", env!("CARGO_PKG_NAME"));

    // ref: https://grafana.com/docs/grafana/latest/panels-visualizations/visualizations/node-graph/#node-parameters
    let query = WriteQuery::new(Local::now().into(), measurement)
        .add_tag(TAG_RUN_ID, client.run_parameters().test_run)
        .add_field("id", peer_id.to_string())
        .add_field("title", format!("attacker_{}", client.group_seq() - 1))
        .add_field("arc__attacker", 1);

    if let Err(e) = client.record_metric(query).await {
        warn!("Failed to record topology. error: {e:?}");
    }
}

pub(crate) async fn record_topology_edge(client: &Client, source: String, target: String) {
    let measurement = format!("{}_topology_edge", env!("CARGO_PKG_NAME"));

    // ref: https://grafana.com/docs/grafana/latest/panels-visualizations/visualizations/node-graph/#edge-parameters
    let query = WriteQuery::new(Local::now().into(), measurement)
        .add_tag(TAG_RUN_ID, client.run_parameters().test_run)
        .add_field("id", rand::random::<u32>().to_string())
        .add_field("source", source)
        .add_field("target", target);

    if let Err(e) = client.record_metric(query).await {
        warn!("Failed to record topology. error: {e:?}");
    }
}
