use crate::InstanceInfo;
use chrono::{DateTime, FixedOffset, Local};
use libp2p::futures::FutureExt;
use libp2p::futures::{Stream, StreamExt};
use prometheus_client::encoding::protobuf::openmetrics_data_model::counter_value;
use prometheus_client::encoding::protobuf::openmetrics_data_model::gauge_value;
use prometheus_client::encoding::protobuf::openmetrics_data_model::metric_point;
use prometheus_client::encoding::protobuf::openmetrics_data_model::HistogramValue;
use prometheus_client::encoding::protobuf::openmetrics_data_model::Metric;
use prometheus_client::encoding::protobuf::openmetrics_data_model::MetricFamily;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::fmt::Debug;
use testground::client::Client;
use testground::WriteQuery;
use tracing::{debug, info};

// States for `barrier()`
pub(crate) const BARRIER_STARTED_LIBP2P: &str = "Started libp2p";
pub(crate) const BARRIER_WARMUP: &str = "Warmup";
pub(crate) const BARRIER_DONE: &str = "Done";

// Tags for InfluxDB
const TAG_INSTANCE_PEER_ID: &str = "instance_peer_id";
const TAG_INSTANCE_NAME: &str = "instance_name";
const TAG_RUN_ID: &str = "run_id";

/// Publish info and collect it from the participants. The return value includes one published by
/// myself.
pub(crate) async fn publish_and_collect<T: Serialize + DeserializeOwned>(
    client: &Client,
    info: T,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    const TOPIC: &str = "publish_and_collect";

    client
        .publish(
            TOPIC,
            Cow::Owned(Value::String(serde_json::to_string(&info)?)),
        )
        .await?;

    let mut stream = client.subscribe(TOPIC, u16::MAX.into()).await;

    let mut vec: Vec<T> = vec![];

    for _ in 0..client.run_parameters().test_instance_count {
        match stream.next().await {
            Some(Ok(other)) => {
                let info: T = match other {
                    Value::String(str) => serde_json::from_str(&str)?,
                    _ => unreachable!(),
                };
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

/// Create InfluxDB queries for Counter metrics.
pub(crate) fn queries_for_counter(
    datetime: &DateTime<FixedOffset>,
    family: &MetricFamily,
    instance_info: &InstanceInfo,
    run_id: &str,
) -> Vec<WriteQuery> {
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let mut query = WriteQuery::new((*datetime).into(), family.name.clone())
            .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
            .add_tag(TAG_INSTANCE_NAME, instance_info.name())
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

/// Create InfluxDB queries for Gauge metrics.
pub(crate) fn queries_for_gauge(
    datetime: &DateTime<FixedOffset>,
    family: &MetricFamily,
    instance_info: &InstanceInfo,
    run_id: &str,
    field_name: &str,
) -> Vec<WriteQuery> {
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let mut query = WriteQuery::new((*datetime).into(), family.name.clone())
            .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
            .add_tag(TAG_INSTANCE_NAME, instance_info.name())
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
    datetime: &DateTime<FixedOffset>,
    family: &MetricFamily,
    instance_info: &InstanceInfo,
    run_id: &str,
) -> Vec<WriteQuery> {
    let mut queries = vec![];

    for metric in family.metrics.iter() {
        let histogram = get_histogram_value(metric);
        for bucket in histogram.buckets.iter() {
            let mut query = WriteQuery::new((*datetime).into(), family.name.clone())
                .add_tag(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string())
                .add_tag(TAG_INSTANCE_NAME, instance_info.name())
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
    instance_info: &InstanceInfo,
    run_id: &str,
) -> Result<(), testground::errors::Error> {
    let query = WriteQuery::new(Local::now().into(), "participants")
        .add_tag(TAG_RUN_ID, run_id.to_owned())
        // Add below as "field" not tag, because in InfluxQL, SELECT clause can't specify only tag.
        // https://docs.influxdata.com/influxdb/v1.8/query_language/explore-data/#select-clause
        // > The SELECT clause must specify at least one field when it includes a tag.
        .add_field(TAG_INSTANCE_NAME, instance_info.name())
        .add_field(TAG_INSTANCE_PEER_ID, instance_info.peer_id.to_string());

    client.record_metric(query).await
}
