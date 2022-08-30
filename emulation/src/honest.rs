use crate::utils::{
    barrier, queries_for_counter, queries_for_gauge, queries_for_histogram, BARRIER_DIALED,
    BARRIER_DONE, BARRIER_STARTED_LIBP2P,
};
use crate::{InstanceInfo, Role};
use chrono::Local;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::{FutureExt, StreamExt};
use libp2p::gossipsub::error::{PublishError, SubscriptionError};
use libp2p::gossipsub::metrics::Config;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, GossipsubEvent, IdentTopic, IdentityTransform,
    MessageAuthenticity, MessageId, PeerScoreParams, PeerScoreThresholds, Topic, TopicScoreParams,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{
    NetworkBehaviour, NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters,
    SwarmBuilder, SwarmEvent,
};
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::Swarm;
use libp2p::Transport;
use libp2p::{NetworkBehaviour, PeerId};
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use std::collections::HashMap;
use std::task::{Context, Poll};
use std::time::Duration;
use testground::client::Client;
use testground::WriteQuery;
use tokio::time::{interval, Interval};

// The backoff time for pruned peers.
pub(crate) const PRUNE_BACKOFF: u64 = 60;

pub(crate) async fn run(
    client: Client,
    instance_info: InstanceInfo,
    participants: Vec<InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    // A topic used in this test plan. Only a single topic is supported for now.
    let topic: IdentTopic = Topic::new("emulate");

    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    let mut registry: Registry<Box<dyn EncodeMetric>> = Registry::default();
    // let mut registry: Registry<_> = Registry::default();
    registry.sub_registry_with_prefix("gossipsub");

    let mut swarm = {
        let gossipsub_config = GossipsubConfigBuilder::default()
            .prune_backoff(Duration::from_secs(PRUNE_BACKOFF))
            .history_length(12)
            .build()
            .expect("Valid configuration");
        let mut gossipsub = Gossipsub::new_with_subscription_filter_and_transform(
            MessageAuthenticity::Signed(keypair.clone()),
            gossipsub_config,
            Some((&mut registry, Config::default())),
            AllowAllSubscriptionFilter {},
            IdentityTransform {},
        )
        .expect("Valid configuration");

        // Setup the scoring system.
        let mut peer_score_params = PeerScoreParams::default();
        peer_score_params
            .topics
            .insert(topic.hash(), TopicScoreParams::default());
        gossipsub
            .with_peer_score(peer_score_params, PeerScoreThresholds::default())
            .expect("Valid score params and thresholds");

        SwarmBuilder::new(
            build_transport(&keypair),
            HonestBehaviour::new(
                gossipsub,
                instance_info.clone(),
                participants.clone(),
                client.clone(),
            ),
            PeerId::from(keypair.public()),
        )
        .executor(Box::new(|future| {
            tokio::spawn(future);
        }))
        .build()
    };

    swarm
        .listen_on(instance_info.multiaddr.clone())
        .expect("Swarm starts listening");

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { .. } => break,
            event => {
                client.record_message(format!("{:?}", event));
            }
        }
    }

    barrier(&client, &mut swarm, BARRIER_STARTED_LIBP2P).await;

    // /////////////////////////////////////////////////////////////////////////////////////////////
    // Setup discovery
    // /////////////////////////////////////////////////////////////////////////////////////////////
    let peers_to_connect = {
        let mut honest = participants
            .iter()
            .filter(|&info| info.role.is_honest())
            .collect::<Vec<_>>();

        // TODO: Parameterize
        let n_peers: usize = 5;

        // Select peers to connect from the honest.
        let mut rnd = rand::rngs::StdRng::seed_from_u64(client.global_seq());
        honest.shuffle(&mut rnd);
        honest[..n_peers]
            .iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>()
    };
    client.record_message(format!("Peers to connect: {:?}", peers_to_connect));

    for peer in peers_to_connect {
        swarm.dial(peer.multiaddr)?;
    }

    barrier(&client, &mut swarm, BARRIER_DIALED).await;

    // ////////////////////////////////////////////////////////////////////////
    // Subscribe to a topic and wait for `warmup` time to expire
    // ////////////////////////////////////////////////////////////////////////
    swarm.behaviour_mut().subscribe(&topic)?;

    // TODO: Parameterize
    let warmup = Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(warmup) => {
                break;
            }
            event = swarm.select_next_some() => {
                client.record_message(format!("{:?}", event));
            }
        }
    }

    if matches!(instance_info.role, Role::Publisher) {
        // ////////////////////////////////////////////////////////////////////////
        // Publish messages
        // ////////////////////////////////////////////////////////////////////////
        // TODO: Parameterize
        let runtime = Duration::from_secs(10);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(runtime) => {
                    break;
                }
                _ = publish_message_periodically(&client, &mut swarm, topic.clone()) => {}
            }
        }
    }

    barrier(&client, &mut swarm, BARRIER_DONE).await;

    // ////////////////////////////////////////////////////////////////////////
    // Record metrics
    // ////////////////////////////////////////////////////////////////////////

    // Encode the metrics to an instance of the OpenMetrics protobuf format.
    // https://github.com/OpenObservability/OpenMetrics/blob/main/proto/openmetrics_data_model.proto
    let metric_set = prometheus_client::encoding::proto::encode(&registry);

    let mut queries = vec![];
    let instance_name = instance_info.name();
    let run_id = &client.run_parameters().test_run;

    for family in metric_set.metric_families.iter() {
        let q = match family.name.as_str() {
            // ///////////////////////////////////
            // Metrics per known topic
            // ///////////////////////////////////
            "topic_subscription_status" => {
                queries_for_gauge(family, &instance_name, run_id, "status")
            }
            "topic_peers_counts" => queries_for_gauge(family, &instance_name, run_id, "count"),
            "invalid_messages_per_topic"
            | "accepted_messages_per_topic"
            | "ignored_messages_per_topic"
            | "rejected_messages_per_topic" => queries_for_counter(family, &instance_name, run_id),
            // ///////////////////////////////////
            // Metrics regarding mesh state
            // ///////////////////////////////////
            "mesh_peer_counts" => queries_for_gauge(family, &instance_name, run_id, "count"),
            "mesh_peer_inclusion_events" => queries_for_counter(family, &instance_name, run_id),
            "mesh_peer_churn_events" => queries_for_counter(family, &instance_name, run_id),
            // ///////////////////////////////////
            // Metrics regarding messages sent/received
            // ///////////////////////////////////
            "topic_msg_sent_counts"
            | "topic_msg_published"
            | "topic_msg_sent_bytes"
            | "topic_msg_recv_counts_unfiltered"
            | "topic_msg_recv_counts"
            | "topic_msg_recv_bytes" => queries_for_counter(family, &instance_name, run_id),
            // ///////////////////////////////////
            // Metrics related to scoring
            // ///////////////////////////////////
            "score_per_mesh" => queries_for_histogram(family, &instance_name, run_id),
            "scoring_penalties" => queries_for_counter(family, &instance_name, run_id),
            // ///////////////////////////////////
            // General Metrics
            // ///////////////////////////////////
            "peers_per_protocol" => queries_for_gauge(family, &instance_name, run_id, "status"),
            "heartbeat_duration" => queries_for_histogram(family, &instance_name, run_id),
            // ///////////////////////////////////
            // Performance metrics
            // ///////////////////////////////////
            "topic_iwant_msgs" => queries_for_counter(family, &instance_name, run_id),
            "memcache_misses" => queries_for_counter(family, &instance_name, run_id),
            _ => unreachable!(),
        };

        queries.extend(q);
    }

    for query in queries {
        if let Err(e) = client.record_metric(query).await {
            client.record_message(format!("Failed to record metrics: {:?}", e));
        }
    }

    client.record_success().await?;
    Ok(())
}

// Set up an encrypted TCP transport over the Mplex and Yamux protocols.
fn build_transport(keypair: &Keypair) -> libp2p::core::transport::Boxed<(PeerId, StreamMuxerBox)> {
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

async fn publish_message_periodically(
    client: &Client,
    swarm: &mut Swarm<HonestBehaviour>,
    topic: IdentTopic,
) {
    // TODO: Parameterize
    let mut interval = interval(Duration::from_millis(500));
    let mut message_counter = 0;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = swarm.behaviour_mut().publish(topic.clone(), format!("message {}", message_counter).as_bytes()) {
                    client.record_message(format!("Failed to publish message: {}", e))
                }
                message_counter += 1;
            }
            event = swarm.select_next_some() => {
                client.record_message(format!("{:?}", event));
            }
        }
    }
}

#[derive(NetworkBehaviour)]
#[behaviour(event_process = true, poll_method = "poll")]
pub(crate) struct HonestBehaviour {
    gossipsub: Gossipsub,
    #[behaviour(ignore)]
    instance_info: InstanceInfo,
    #[behaviour(ignore)]
    participants: HashMap<PeerId, String>,
    #[behaviour(ignore)]
    client: Client,
    #[behaviour(ignore)]
    score_interval: Interval,
}

impl HonestBehaviour {
    pub(crate) fn new(
        gossipsub: Gossipsub,
        instance_info: InstanceInfo,
        participants: Vec<InstanceInfo>,
        client: Client,
    ) -> Self {
        let mut peer_to_instance_name = HashMap::new();
        for info in participants {
            peer_to_instance_name.insert(info.peer_id, info.name());
        }

        HonestBehaviour {
            gossipsub,
            instance_info,
            participants: peer_to_instance_name,
            client,
            score_interval: interval(Duration::from_secs(1)),
        }
    }

    fn publish(
        &mut self,
        topic: IdentTopic,
        message: impl Into<Vec<u8>>,
    ) -> Result<MessageId, PublishError> {
        self.gossipsub.publish(topic, message)
    }

    fn subscribe(&mut self, topic: &IdentTopic) -> Result<bool, SubscriptionError> {
        self.gossipsub.subscribe(topic)
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<(), <HonestBehaviour as NetworkBehaviour>::ConnectionHandler>>
    {
        // Store scores to InfluxDB.
        let run_id = self.client.run_parameters().test_run;
        while self.score_interval.poll_tick(cx).is_ready() {
            let scores = self
                .gossipsub
                .all_peers()
                .map(|(peer, _)| (peer, self.gossipsub.peer_score(peer)))
                .filter(|(_, score)| score.is_some())
                .collect::<Vec<_>>();

            if !scores.is_empty() {
                let mut query = WriteQuery::new(Local::now().into(), "scores")
                    .add_tag("instance_name", self.instance_info.name())
                    .add_tag("run_id", run_id.clone());

                for (peer, score) in scores {
                    query = query.add_field(self.participants.get(peer).unwrap(), score.unwrap());
                }

                let mut fut = Box::pin(self.client.record_metric(query));
                loop {
                    match fut.poll_unpin(cx) {
                        Poll::Ready(result) => {
                            if let Err(e) = result {
                                self.client
                                    .record_message(format!("Failed to record score: {:?}", e))
                            }
                            break;
                        }
                        Poll::Pending => {}
                    }
                }
            }
        }

        Poll::Pending
    }
}

impl NetworkBehaviourEventProcess<GossipsubEvent> for HonestBehaviour {
    fn inject_event(&mut self, _: GossipsubEvent) {}
}
