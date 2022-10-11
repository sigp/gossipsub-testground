use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::RangeInclusive;

use rand::seq::SliceRandom;
use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

pub type NodeId = usize;
pub type ValId = usize;

#[derive(Serialize, Deserialize, Debug)]
pub struct Params {
    seed: u64,
    total_validators: usize,
    total_nodes_with_vals: usize,
    total_nodes: usize,
    connections_range: RangeInclusive<usize>,
}

impl Params {
    pub fn new(
        seed: u64,
        total_validators: usize,
        total_nodes_with_vals: usize,
        total_nodes_without_vals: usize,
        min_peers_per_node: usize,
        max_peers_per_node_inclusive: usize,
    ) -> Result<Self, &'static str> {
        let total_nodes = total_nodes_with_vals + total_nodes_without_vals;
        if total_nodes_with_vals > total_nodes {
            return Err("bad number of nodes with validators");
        }

        if total_nodes == 0 {
            return Err("Empty network");
        }

        if total_validators == 0 {
            return Err("no validators in the network");
        }

        let connections_range = min_peers_per_node..=max_peers_per_node_inclusive;
        if connections_range.is_empty() {
            return Err("bad connection range");
        }

        Ok(Self {
            seed,
            total_validators,
            total_nodes_with_vals,
            total_nodes,
            connections_range,
        })
    }
    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn total_validators(&self) -> usize {
        self.total_validators
    }

    pub fn total_nodes_with_vals(&self) -> usize {
        self.total_nodes_with_vals
    }

    pub fn total_nodes(&self) -> usize {
        self.total_nodes
    }

    pub fn connections_range(&self) -> std::ops::RangeInclusive<usize> {
        self.connections_range.clone()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Network {
    /// Params used to generate this network configuration. Stored to allow reproduction.
    params: Params,
    /// Validators managed by each node.
    validator_assignments: BTreeMap<NodeId, BTreeSet<ValId>>,
    /// Peers to connect to for each node.
    outbound_peers: BTreeMap<NodeId, Vec<NodeId>>,
}

impl Network {
    fn new(
        params: Params,
        validator_assignments: BTreeMap<NodeId, BTreeSet<ValId>>,
        outbound_peers: BTreeMap<NodeId, Vec<NodeId>>,
    ) -> Result<Self, String> {
        let total_vals: usize = validator_assignments
            .values()
            .map(|validator_list| validator_list.len())
            .sum();

        //
        // validator assignment checks
        //

        // first check that under possibility of repetition the number of validators is right
        if total_vals != params.total_validators {
            return Err(format!("the number of assigned validators does not match the expected number. Found {}, expected {}", total_vals, params.total_validators));
        }

        let assigned_vals: HashSet<&ValId> = validator_assignments
            .values()
            .flat_map(|validator_list| validator_list.iter())
            .collect();
        if assigned_vals.len() != total_vals {
            return Err("a validator id was assigned more than once".to_string());
        }

        if assigned_vals.iter().max().expect("total_validators > 0")
            != &&(params.total_validators - 1)
        {
            return Err("validator ids do not cover the expected range (wrong max)".to_string());
        }

        if assigned_vals.iter().min().expect("total_validators > 0") != &&0 {
            return Err("validator ids do not cover the expected range (wrong min)".to_string());
        }

        //
        // topology checks
        //

        let connected_peers: HashSet<NodeId> = outbound_peers
            .iter()
            .flat_map(|(peer_a, dialed_peers)| dialed_peers.iter().chain(Some(peer_a)))
            .cloned()
            .collect();
        let expected_peers: HashSet<usize> = (0..params.total_nodes).collect();
        if connected_peers != expected_peers {
            return Err(format!(
                "set of dialed peers and expected peers differ: missing {:?}",
                expected_peers.difference(&connected_peers)
            ));
        }

        println!("Connectedness should be checked with an external tool!");
        Ok(Self {
            params,
            validator_assignments,
            outbound_peers,
        })
    }

    pub fn generate(params: Params) -> Result<Self, String> {
        // Use a deterministic pseudo random number generator
        let mut gen = ChaCha8Rng::seed_from_u64(params.seed);

        let validator_assignments = {
            // Assing validators to each node that has any validator at all
            let mut all_validators = (0..params.total_validators).collect::<Vec<_>>();
            let mut cuts: Vec<_> = all_validators
                .choose_multiple(&mut gen, params.total_nodes_with_vals - 1)
                .cloned()
                .collect();
            cuts.push(params.total_validators);
            cuts.push(0);
            all_validators.shuffle(&mut gen);
            cuts.sort();

            let validator_assignments: BTreeMap<NodeId, BTreeSet<ValId>> = cuts
                .windows(2)
                .enumerate()
                .map(|(node_id, current_cut)| {
                    // NOTE: this means the nodes that have validators are the first
                    // `params.total_nodes_with_vals`. Since the node_id is an abstract construct not
                    // used anywhere I don't think it's worth randomizing this part
                    let start = current_cut[0];
                    let end = current_cut[1];
                    let assigned_vals: BTreeSet<ValId> =
                        all_validators[start..end].into_iter().cloned().collect();
                    (node_id, assigned_vals)
                })
                .collect();

            validator_assignments
        };

        let outbound_peers = {
            let mut outbound_peers: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::default();

            // First build the set of all possible connections (a,b) ignoring connection direction
            let mut all_connections: Vec<(usize, usize)> = (0..params.total_nodes)
                .flat_map(|i| (i + 1..params.total_nodes).map(move |j| (i, j)))
                .collect();

            // Keep track of all connections (inbound and outbound) per peer.
            // BTreeSet is useful for debugging with consistent order.
            type IsOutbound = bool;
            let mut topology = BTreeMap::<NodeId, (usize, BTreeMap<NodeId, IsOutbound>)>::default();

            let connections_range = params.connections_range();

            // for each node_id, generate a random expected number of connections within the given
            // range
            for p in 0..params.total_nodes {
                // decide how many connections should the node have.
                let num_peers = gen.gen_range(connections_range.clone());
                // store the expected number of connections and a default map to keep track of the
                // added connections.
                topology.insert(p, (num_peers, BTreeMap::default()));
            }

            // Pick a random connection and add it if it's useful.
            all_connections.shuffle(&mut gen);
            while let Some((a, b)) = all_connections.pop() {
                let (expected, current) = topology.get(&a).unwrap();
                if current.len() >= *expected {
                    continue;
                }

                let (expected, current) = topology.get(&b).unwrap();
                if current.len() >= *expected {
                    continue;
                }

                let from_a_to_b = gen.gen_ratio(3, 5);

                topology.get_mut(&a).unwrap().1.insert(b, from_a_to_b);
                topology.get_mut(&b).unwrap().1.insert(a, !from_a_to_b);

                if from_a_to_b {
                    outbound_peers.entry(a).or_default().push(b);
                } else {
                    outbound_peers.entry(b).or_default().push(a);
                }
            }

            if topology
                .values()
                .any(|(_expected_connections, connections)| {
                    !connections_range.contains(&connections.len())
                })
            {
                // The _expected_connections number might not be reached for a few nodes. We really
                // care about the number of connections being withing the set range. I haven't seen
                // this happen so far.
                eprintln!("Some nodes didn't reach the expected number of connections");
            }
            outbound_peers
        };

        Network::new(params, validator_assignments, outbound_peers)
            .map_err(|e| format!("Network generation failed: {e}"))
    }

    pub fn outbound_peers(&self) -> &BTreeMap<NodeId, Vec<NodeId>> {
        &self.outbound_peers
    }

    pub fn validator_assignments(&self) -> &BTreeMap<NodeId, BTreeSet<ValId>> {
        &self.validator_assignments
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn destructure(
        self,
    ) -> (
        Params,
        BTreeMap<NodeId, Vec<NodeId>>,
        BTreeMap<NodeId, BTreeSet<ValId>>,
    ) {
        let Network {
            params,
            validator_assignments,
            outbound_peers,
        } = self;
        (params, outbound_peers, validator_assignments)
    }
}
