use libp2p::futures::{Stream, StreamExt};
use prometheus_client::encoding::proto::openmetrics_data_model::counter_value;
use prometheus_client::encoding::proto::openmetrics_data_model::gauge_value;
use prometheus_client::encoding::proto::openmetrics_data_model::metric_point;
use prometheus_client::encoding::proto::openmetrics_data_model::Label;
use prometheus_client::encoding::proto::openmetrics_data_model::Metric;
use prometheus_client::encoding::proto::openmetrics_data_model::MetricFamily;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::borrow::Cow;
use std::fmt::Debug;
use testground::client::Client;
use testground::WriteQuery;

// States for `barrier()`
pub(crate) const BARRIER_STARTED_LIBP2P: &str = "Started libp2p";
pub(crate) const BARRIER_DIALED: &str = "Dialed";
pub(crate) const BARRIER_DONE: &str = "Done";

// Publish info and collect it from the participants. The return value includes one published by
// myself.
pub(crate) async fn publish_and_collect<T: Serialize + DeserializeOwned>(
    client: &Client,
    info: T,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    const TOPIC: &str = "publish_and_collect";

    client.publish(TOPIC, serde_json::to_string(&info)?).await?;

    let mut stream = client.subscribe(TOPIC).await;

    let mut vec: Vec<T> = vec![];

    for _ in 0..client.run_parameters().test_instance_count {
        match stream.next().await {
            Some(Ok(other)) => {
                let info: T = serde_json::from_str(&other)?;
                vec.push(info);
            }
            Some(Err(e)) => return Err(Box::new(e)),
            None => unreachable!(),
        }
    }

    Ok(vec)
}

// Sets a barrier on the supplied state that fires when it reaches all participants.
pub(crate) async fn barrier<T: StreamExt + Unpin + libp2p::futures::stream::FusedStream>(
    client: &Client,
    swarm: &mut T,
    state: impl Into<Cow<'static, str>> + Copy,
) where
    <T as Stream>::Item: Debug,
{
    loop {
        tokio::select! {
            _ = client.signal_and_wait(state, client.run_parameters().test_instance_count) => {
                break;
            }
            // Record the Swarm events that happen while waiting for the barrier.
            event = swarm.select_next_some() => {
                client.record_message(format!("{:?}", event));
            }
        }
    }
}

// Add fields to the InfluxDB write query.
// This function is dedicated for the metrics type `Family<TopicHash, Counter>`.
pub(crate) fn add_counter_metrics(mut query: WriteQuery, family: &MetricFamily) -> WriteQuery {
    for metric in family.metrics.iter() {
        // Field name: `{Family}_{TopicHash}` (e.g. `invalid_messages_per_topic_emulate`)
        query = query.add_field(
            format!("{}_{}", family.name, get_topic_hash(&metric.labels)),
            get_counter_value(metric).0.expect("should have int value"),
        );
    }

    query
}

// Add fields to the InfluxDB write query.
// This function is dedicated for the metrics type `Family<TopicHash, Gauge>`.
pub(crate) fn add_gauge_metrics(mut query: WriteQuery, family: &MetricFamily) -> WriteQuery {
    for metric in family.metrics.iter() {
        // Field name: `{Family}_{TopicHash}` (e.g. `topic_subscription_status_emulate`)
        query = query.add_field(
            format!("{}_{}", family.name, get_topic_hash(&metric.labels)),
            get_gauge_value(metric).0.expect("should have int value"),
        );
    }

    query
}

pub(crate) fn get_gauge_value(metric: &Metric) -> (Option<i64>, Option<f64>) {
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

pub(crate) fn get_topic_hash(labels: &[Label]) -> String {
    labels
        .iter()
        .find(|l| l.name == "hash")
        .expect("have topic hash")
        .value
        .clone()
}
