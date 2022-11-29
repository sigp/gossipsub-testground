use crate::InstanceInfo;
use chrono::{DateTime, Local, Utc};
use libp2p::futures::StreamExt;
use libp2p::PeerId;
use prometheus_client::encoding::proto::openmetrics_data_model::counter_value;
use prometheus_client::encoding::proto::openmetrics_data_model::gauge_value;
use prometheus_client::encoding::proto::openmetrics_data_model::metric_point;
use prometheus_client::encoding::proto::openmetrics_data_model::Metric;
use prometheus_client::encoding::proto::openmetrics_data_model::MetricFamily;
use prometheus_client::encoding::proto::HistogramValue;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use testground::client::Client;
use testground::WriteQuery;

// States for `barrier()`
pub(crate) const BARRIER_LIBP2P_READY: &str = "Started libp2p";
pub(crate) const BARRIER_TOPOLOGY_READY: &str = "Topology generated";

// Tags for InfluxDB
const TAG_INSTANCE_PEER_ID: &str = "instance_peer_id";
const TAG_INSTANCE_NAME: &str = "instance_name";
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
    node_id: usize,
    instance_info: &InstanceInfo,
    run_id: &str,
) -> Vec<WriteQuery> {
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let mut query = WriteQuery::new((*datetime).into(), family.name.clone())
            .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
            .add_tag(TAG_INSTANCE_NAME, node_id.to_string())
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
    datetime: &DateTime<Utc>,
    family1: &MetricFamily,
    family2: &MetricFamily,
    name: &str,
    node_id: usize,
    instance_info: &InstanceInfo,
    run_id: &str,
    predicate: fn(u64, u64) -> u64,
) -> Vec<WriteQuery> {
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
            let mut query = WriteQuery::new((*datetime).into(), name)
                .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
                .add_tag(TAG_INSTANCE_NAME, node_id.to_string())
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

/// Create InfluxDB queries for Gauge metrics.
pub(crate) fn queries_for_gauge(
    datetime: &DateTime<Utc>,
    family: &MetricFamily,
    node_id: usize,
    instance_info: &InstanceInfo,
    run_id: &str,
    field_name: &str,
) -> Vec<WriteQuery> {
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let mut query = WriteQuery::new((*datetime).into(), family.name.clone())
            .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
            .add_tag(TAG_INSTANCE_NAME, node_id.to_string())
            .add_tag(TAG_RUN_ID, run_id.to_owned())
            .add_field(
                field_name,
                get_gauge_value(metric).0.expect("should have int value"),
            );

        for l in &metric.labels {
            query = query.add_tag(l.name.clone(), l.value.clone());
        }

        queries.push(query);
    }

    queries
}

/// Create InfluxDB queries for Histogram metrics.
pub(crate) fn queries_for_histogram(
    datetime: &DateTime<Utc>,
    family: &MetricFamily,
    node_id: usize,
    instance_info: &InstanceInfo,
    run_id: &str,
) -> Vec<WriteQuery> {
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let histogram = get_histogram_value(metric);
        for bucket in histogram.buckets.iter() {
            let mut query = WriteQuery::new((*datetime).into(), family.name.clone())
                .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
                .add_tag(TAG_INSTANCE_NAME, node_id.to_string())
                .add_tag(TAG_RUN_ID, run_id.to_owned())
                .add_field("count", bucket.count)
                .add_field("upper_bound", bucket.upper_bound);

            for l in &metric.labels {
                query = query.add_tag(l.name.clone(), l.value.clone());
            }
            queries.push(query);
        }
    }

    queries
}

fn get_gauge_value(metric: &Metric) -> (Option<i64>, Option<f64>) {
    assert_eq!(1, metric.metric_points.len());

    let metric_point = metric.metric_points.first().unwrap();
    let metric_point_value = metric_point.value.as_ref().unwrap().clone();
    match metric_point_value {
        metric_point::Value::GaugeValue(gauge_value) => match gauge_value.value {
            Some(gauge_value::Value::IntValue(i)) => (Some(i), None),
            Some(gauge_value::Value::DoubleValue(f)) => (None, Some(f)),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

pub(crate) fn get_counter_value(metric: &Metric) -> (Option<u64>, Option<f64>) {
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

fn get_histogram_value(metric: &Metric) -> HistogramValue {
    assert_eq!(1, metric.metric_points.len());

    let metric_point = metric.metric_points.first().unwrap();
    let metric_point_value = metric_point.value.as_ref().unwrap().clone();
    match metric_point_value {
        metric_point::Value::HistogramValue(histogram_value) => histogram_value,
        _ => unreachable!(),
    }
}

/// Record an InstanceInfo to InfluxDB. This is useful on Grafana dashboard.
pub(crate) async fn record_instance_info(
    client: &Client,
    node_id: usize,
    peer_id: &PeerId,
    run_id: &str,
) -> Result<(), testground::errors::Error> {
    let query = WriteQuery::new(Local::now().into(), "participants")
        .add_tag(TAG_RUN_ID, run_id.to_owned())
        // Add below as "field" not tag, because in InfluxQL, SELECT clause can't specify only tag.
        // https://docs.influxdata.com/influxdb/v1.8/query_language/explore-data/#select-clause
        // > The SELECT clause must specify at least one field when it includes a tag.
        .add_field(TAG_INSTANCE_NAME, node_id.to_string())
        .add_field(TAG_INSTANCE_PEER_ID, peer_id.to_string());

    client.record_metric(query).await
}
