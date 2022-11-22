use crate::utils::{
    queries_for_counter, queries_for_gauge, queries_for_histogram, record_instance_info,
    BARRIER_DONE, BARRIER_STARTED_LIBP2P, BARRIER_WARMUP,
};
use crate::{InstanceInfo, Role};
use chrono::{DateTime, Local, Utc};
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::upgrade::{SelectUpgrade, Version};
use libp2p::dns::TokioDnsConfig;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::metrics::Config;
use libp2p::gossipsub::subscription_filter::AllowAllSubscriptionFilter;
use libp2p::gossipsub::{
    Gossipsub, GossipsubConfigBuilder, IdentTopic, IdentityTransform, MessageAuthenticity,
    PeerScoreParams, PeerScoreThresholds, Topic, TopicScoreParams,
};
use libp2p::identity::Keypair;
use libp2p::mplex::MplexConfig;
use libp2p::noise::NoiseConfig;
use libp2p::swarm::{DialError, SwarmBuilder, SwarmEvent};
use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
use libp2p::yamux::YamuxConfig;
use libp2p::PeerId;
use libp2p::Transport;
use libp2p::{Multiaddr, Swarm};
use prometheus_client::encoding::proto::EncodeMetric;
use prometheus_client::registry::Registry;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use std::collections::HashMap;
use std::time::Duration;
use testground::client::Client;
use testground::WriteQuery;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, Interval};
use tracing::debug;

/// The backoff time for pruned peers.
pub(crate) const PRUNE_BACKOFF: u64 = 60;

#[derive(Clone)]
pub(crate) struct TestParams {
    pub(crate) peers_to_connect: usize,
    pub(crate) message_rate: u64,
    pub(crate) warmup: Duration,
    pub(crate) run: Duration,
}

impl TestParams {
    pub(crate) fn new(
        instance_params: HashMap<String, String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let peers_to_connect = instance_params
            .get("peers_to_connect")
            .ok_or("peers_to_connect is not specified.")?
            .parse::<usize>()?;
        let message_rate = instance_params
            .get("message_rate")
            .ok_or("message_rate is not specified.")?
            .parse::<u64>()?;
        let warmup = instance_params
            .get("warmup")
            .ok_or("warmup is not specified.")?
            .parse::<u64>()?;
        let run = instance_params
            .get("run")
            .ok_or("run is not specified.")?
            .parse::<u64>()?;

        Ok(TestParams {
            peers_to_connect,
            message_rate,
            warmup: Duration::from_secs(warmup),
            run: Duration::from_secs(run),
        })
    }
}

pub(crate) async fn run(
    client: Client,
    instance_info: InstanceInfo,
    participants: Vec<InstanceInfo>,
    keypair: Keypair,
) -> Result<(), Box<dyn std::error::Error>> {
    let run_id = &client.run_parameters().test_run;
    record_instance_info(&client, &instance_info, run_id).await?;

    // A topic used in this test plan. Only a single topic is supported for now.
    let topic: IdentTopic = Topic::new("emulate");

    let test_params = TestParams::new(client.run_parameters().test_instance_params)?;

    // ////////////////////////////////////////////////////////////////////////
    // Start libp2p
    // ////////////////////////////////////////////////////////////////////////
    let mut registry: Registry<Box<dyn EncodeMetric>> = Registry::default();
    registry.sub_registry_with_prefix("gossipsub");

    let network_send = spawn_honest_network(
        &mut registry,
        keypair.clone(),
        instance_info.clone(),
        &participants,
        client.clone(),
        &topic,
        &test_params,
    )
    .await?;

    client
        .signal_and_wait(
            BARRIER_STARTED_LIBP2P,
            client.run_parameters().test_instance_count,
        )
        .await?;

    // ////////////////////////////////////////////////////////////////////////
    // Subscribe to a topic and wait for `warmup` time to expire
    // ////////////////////////////////////////////////////////////////////////
    network_send.send(HonestMessage::Subscribe(topic.clone()))?;
    tokio::time::sleep(test_params.warmup).await;

    client
        .signal_and_wait(BARRIER_WARMUP, client.run_parameters().test_instance_count)
        .await?;

    // ////////////////////////////////////////////////////////////////////////
    // Publish messages
    // ////////////////////////////////////////////////////////////////////////
    if matches!(instance_info.role, Role::Publisher) {
        let publish_interval = Duration::from_millis(1000 / test_params.message_rate);
        let total_expected_messages = test_params.run.as_millis() / publish_interval.as_millis();

        client.record_message(format!(
            "Publishing to topic `{}`. message_rate: {}/1s, publish_interval {:?}, total expected messages: {}",
            topic,
            test_params.message_rate,
            publish_interval,
            total_expected_messages
        ));

        network_send.send(HonestMessage::StartPublishing)?;
        tokio::time::sleep(test_params.run).await;
        network_send.send(HonestMessage::StopPublishing)?;
    }

    client
        .signal_and_wait(BARRIER_DONE, client.run_parameters().test_instance_count)
        .await?;

    // ////////////////////////////////////////////////////////////////////////
    // Record metrics
    // ////////////////////////////////////////////////////////////////////////

    // Encode the metrics to an instance of the OpenMetrics protobuf format.
    // https://github.com/OpenObservability/OpenMetrics/blob/main/proto/openmetrics_data_model.proto
    let metric_set = prometheus_client::encoding::proto::encode(&registry);

    let mut queries = vec![];

    // The `test_start_time` is used as the timestamp of metrics instead of local time of each
    // instance so the timestamps between metrics of each instance are aligned.
    // This is helpful when we want to sort metrics by something not timestamp(e.g. instance_name).
    let test_start_time: DateTime<Utc> =
        DateTime::parse_from_rfc3339(&client.run_parameters().test_start_time)?.into();

    for family in metric_set.metric_families.iter() {
        let q = match family.name.as_str() {
            // ///////////////////////////////////
            // Metrics per known topic
            // ///////////////////////////////////
            "topic_subscription_status" => {
                queries_for_gauge(&test_start_time, family, &instance_info, run_id, "status")
            }
            "topic_peers_counts" => {
                queries_for_gauge(&test_start_time, family, &instance_info, run_id, "count")
            }
            "invalid_messages_per_topic"
            | "accepted_messages_per_topic"
            | "ignored_messages_per_topic"
            | "rejected_messages_per_topic" => {
                queries_for_counter(&test_start_time, family, &instance_info, run_id)
            }
            // ///////////////////////////////////
            // Metrics regarding mesh state
            // ///////////////////////////////////
            "mesh_peer_counts" => {
                queries_for_gauge(&test_start_time, family, &instance_info, run_id, "count")
            }
            "mesh_peer_inclusion_events" => {
                queries_for_counter(&test_start_time, family, &instance_info, run_id)
            }
            "mesh_peer_churn_events" => {
                queries_for_counter(&test_start_time, family, &instance_info, run_id)
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
                queries_for_counter(&test_start_time, family, &instance_info, run_id)
            }
            // ///////////////////////////////////
            // Metrics related to scoring
            // ///////////////////////////////////
            "score_per_mesh" => {
                queries_for_histogram(&test_start_time, family, &instance_info, run_id)
            }
            "scoring_penalties" => {
                queries_for_counter(&test_start_time, family, &instance_info, run_id)
            }
            // ///////////////////////////////////
            // General Metrics
            // ///////////////////////////////////
            "peers_per_protocol" => {
                queries_for_gauge(&test_start_time, family, &instance_info, run_id, "peers")
            }
            "heartbeat_duration" => {
                queries_for_histogram(&test_start_time, family, &instance_info, run_id)
            }
            // ///////////////////////////////////
            // Performance metrics
            // ///////////////////////////////////
            "topic_iwant_msgs" => {
                queries_for_counter(&test_start_time, family, &instance_info, run_id)
            }
            "memcache_misses" => {
                queries_for_counter(&test_start_time, family, &instance_info, run_id)
            }
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

/// Set up an encrypted TCP transport over the Mplex and Yamux protocols.
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

#[derive(Debug)]
enum HonestMessage {
    Subscribe(IdentTopic),
    StartPublishing,
    StopPublishing,
}

enum PublishState {
    Awaiting,
    Started,
    Stopped,
}

pub(crate) struct HonestNetwork {
    swarm: Swarm<Gossipsub>,
    instance_info: InstanceInfo,
    participants: HashMap<PeerId, String>,
    client: Client,
    score_interval: Interval,
    publish_interval: Interval,
    publish_state: PublishState,
    topic: IdentTopic,
    recv: UnboundedReceiver<HonestMessage>,
}

impl HonestNetwork {
    #[allow(clippy::too_many_arguments)]
    fn new(
        registry: &mut Registry<Box<dyn EncodeMetric>>,
        keypair: Keypair,
        instance_info: InstanceInfo,
        participants: &Vec<InstanceInfo>,
        client: Client,
        topic: &IdentTopic,
        test_params: TestParams,
        recv: UnboundedReceiver<HonestMessage>,
    ) -> Self {
        let gossipsub = {
            let gossipsub_config = GossipsubConfigBuilder::default()
                .prune_backoff(Duration::from_secs(PRUNE_BACKOFF))
                .history_length(12)
                .build()
                .expect("Valid configuration");

            let mut gs = Gossipsub::new_with_subscription_filter_and_transform(
                MessageAuthenticity::Signed(keypair.clone()),
                gossipsub_config,
                Some((registry, Config::default())),
                AllowAllSubscriptionFilter {},
                IdentityTransform {},
            )
            .expect("Valid configuration");

            // Setup the scoring system.
            let mut peer_score_params = PeerScoreParams::default();
            peer_score_params
                .topics
                .insert(topic.hash(), TopicScoreParams::default());
            gs.with_peer_score(peer_score_params, PeerScoreThresholds::default())
                .expect("Valid score params and thresholds");

            gs
        };

        let swarm = SwarmBuilder::new(
            build_transport(&keypair),
            gossipsub,
            PeerId::from(keypair.public()),
        )
        .executor(Box::new(|future| {
            tokio::spawn(future);
        }))
        .build();

        let mut peer_to_instance_name = HashMap::new();
        for info in participants {
            peer_to_instance_name.insert(info.peer_id, info.name());
        }

        HonestNetwork {
            swarm,
            instance_info,
            participants: peer_to_instance_name,
            client,
            score_interval: interval(Duration::from_secs(1)),
            publish_interval: interval(Duration::from_millis(1000 / test_params.message_rate)),
            publish_state: PublishState::Awaiting,
            topic: topic.clone(),
            recv,
        }
    }

    async fn start(&mut self) {
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

    fn dial(&mut self, addr: Multiaddr) -> Result<(), DialError> {
        self.swarm.dial(addr)
    }

    fn spawn(mut self) {
        let fut = async move {
            loop {
                tokio::select! {
                    // Record peer scores
                    _ = self.score_interval.tick() => {
                        self.record_peer_scores().await;
                    }
                    // Publish messages
                    _ = self.publish_interval.tick(), if matches!(self.publish_state, PublishState::Started) => {
                        if let Err(e) = self.swarm.behaviour_mut().publish(self.topic.clone(), "message".as_bytes()) {
                            self.client.record_message(format!("Failed to publish message: {}", e))
                        }
                    }
                    event = self.swarm.select_next_some() => {
                        debug!("SwarmEvent: {:?}", event);
                    }
                    Some(message) = self.recv.recv() => {
                        match message {
                            HonestMessage::Subscribe(topic) => {
                                if let Err(e) = self.swarm.behaviour_mut().subscribe(&topic) {
                                    self.client.record_message(format!("Failed to subscribe: {}", e));
                                }
                            }
                            HonestMessage::StartPublishing => {
                                self.publish_state = PublishState::Started;
                            }
                            HonestMessage::StopPublishing => {
                                self.publish_state = PublishState::Stopped;
                            }
                        }
                    }
                }
            }
        };

        tokio::runtime::Handle::current().spawn(fut);
    }

    async fn record_peer_scores(&mut self) {
        let gossipsub = self.swarm.behaviour_mut();
        let scores = gossipsub
            .all_peers()
            .filter_map(|(peer, _)| gossipsub.peer_score(peer).map(|score| (peer, score)))
            .collect::<Vec<_>>();

        if !scores.is_empty() {
            let mut query = WriteQuery::new(Local::now().into(), "scores")
                .add_tag("instance_peer_id", self.instance_info.peer_id.to_string())
                .add_tag("instance_name", self.instance_info.name())
                .add_tag("run_id", self.client.run_parameters().test_run);

            for (peer, score) in scores {
                query = query.add_field(self.participants.get(peer).unwrap(), score);
            }

            if let Err(e) = self.client.record_metric(query).await {
                self.client
                    .record_message(format!("Failed to record score: {:?}", e))
            }
        }
    }
}

async fn spawn_honest_network(
    registry: &mut Registry<Box<dyn EncodeMetric>>,
    keypair: Keypair,
    instance_info: InstanceInfo,
    participants: &Vec<InstanceInfo>,
    client: Client,
    topic: &IdentTopic,
    test_params: &TestParams,
) -> Result<UnboundedSender<HonestMessage>, Box<dyn std::error::Error>> {
    let (send, recv) = tokio::sync::mpsc::unbounded_channel();

    let mut honest_network = HonestNetwork::new(
        registry,
        keypair,
        instance_info,
        participants,
        client.clone(),
        topic,
        test_params.clone(),
        recv,
    );
    honest_network.start().await;

    // Setup discovery
    let peers_to_connect = {
        let mut honest = participants
            .iter()
            .filter(|&info| info.role.is_honest())
            .collect::<Vec<_>>();

        // Select peers to connect from the honest.
        let mut rnd = rand::rngs::StdRng::seed_from_u64(client.global_seq());
        honest.shuffle(&mut rnd);
        honest[..test_params.peers_to_connect]
            .iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>()
    };
    client.record_message(format!("Peers to connect: {:?}", peers_to_connect));

    for peer in peers_to_connect {
        honest_network.dial(peer.multiaddr)?;
    }

    honest_network.spawn();

    Ok(send)
}
