use crate::beacon_node::BeaconNodeInfo;
use chrono::{DateTime, Local};
use libp2p::futures::StreamExt;
use prometheus_client::encoding::proto::openmetrics_data_model::counter_value;
use prometheus_client::encoding::proto::openmetrics_data_model::metric_point;
use prometheus_client::encoding::proto::openmetrics_data_model::Metric;
use prometheus_client::encoding::proto::openmetrics_data_model::MetricFamily;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use testground::client::Client;
use testground::WriteQuery;

// States for `barrier()`
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
    beacon_node_info: &BeaconNodeInfo,
    run_id: &str,
) -> Vec<WriteQuery> {
    let measurement = format!("{}_{}", env!("CARGO_PKG_NAME"), family.name);
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let mut query = WriteQuery::new((*datetime).into(), &measurement)
            .add_tag(TAG_PEER_ID, beacon_node_info.peer_id().to_string())
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
