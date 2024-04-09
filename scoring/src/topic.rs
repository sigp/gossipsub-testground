use gossipsub::{IdentTopic, Topic as GossipTopic};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) enum Topic {
    Blocks,
    Attestations(u64),
    Aggregates(u64),
    SyncMessages(u64),
    SignedContributionAndProof(u64),
}

impl From<Topic> for IdentTopic {
    fn from(t: Topic) -> Self {
        let rep = serde_json::to_string(&t).expect("json serialization of topics never fails");
        GossipTopic::new(rep)
    }
}

impl From<IdentTopic> for Topic {
    fn from(t: IdentTopic) -> Self {
        let repr = t.hash().into_string();
        serde_json::from_str(&repr).expect("json deserialization of topics never fails")
    }
}

impl From<&str> for Topic {
    fn from(s: &str) -> Self {
        serde_json::from_str(s).expect("json deserialization of topics never fails")
    }
}
