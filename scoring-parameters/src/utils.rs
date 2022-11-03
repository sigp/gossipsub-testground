use crate::InstanceInfo;
use chrono::{DateTime, Local, Utc};
use libp2p::futures::StreamExt;
use prometheus_client::encoding::proto::openmetrics_data_model::counter_value;
use prometheus_client::encoding::proto::openmetrics_data_model::metric_point;
use prometheus_client::encoding::proto::openmetrics_data_model::Metric;
use prometheus_client::encoding::proto::openmetrics_data_model::MetricFamily;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use testground::client::Client;
use testground::WriteQuery;
use types::{Epoch, Slot};

// States for `barrier()`
pub(crate) const BARRIER_LIBP2P_READY: &str = "Started libp2p";
pub(crate) const BARRIER_TOPOLOGY_READY: &str = "Topology generated";

// Tags for InfluxDB
const TAG_INSTANCE_PEER_ID: &str = "instance_peer_id";
const TAG_RUN_ID: &str = "run_id";

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

/// Create InfluxDB queries for Counter metrics.
pub(crate) fn queries_for_counter(
    datetime: &DateTime<Utc>,
    family: &MetricFamily,
    instance_info: &InstanceInfo,
    run_id: &str,
) -> Vec<WriteQuery> {
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let mut query = WriteQuery::new((*datetime).into(), family.name.clone())
            .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
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

/// Create InfluxDB queries for received BeaconBlock messages.
pub(crate) fn queries_for_received_beacon_blocks(
    instance_info: &InstanceInfo,
    received_blocks: &HashMap<Epoch, HashSet<Slot>>,
    run_id: &str,
) -> Vec<WriteQuery> {
    let mut queries = vec![];

    for (epoch, slots) in received_blocks.iter() {
        let query = WriteQuery::new(Local::now().into(), "beacon_block")
            .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
            .add_tag(TAG_RUN_ID, run_id.to_owned())
            .add_tag("epoch", epoch.as_u64())
            .add_field("count", slots.len() as u64);
        queries.push(query);
    }

    queries
}
