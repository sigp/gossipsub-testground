use clap::Parser;
extern crate gen_topology;
use std::io::Write;

#[derive(Parser)]
pub struct RunParams {
    seed: u64,
    total_validators: usize,
    total_nodes_with_vals: usize,
    total_nodes: usize,
    min_peers_per_node: usize,
    max_peers_per_node_inc: usize,
    dotfile_name: String,
    config_file_name: String,
}

fn main() {
    if let Err(e) = gen_and_save() {
        eprintln!("{}", e);
        std::process::exit(1)
    }
}

fn gen_and_save() -> Result<(), String> {
    let RunParams {
        seed,
        total_validators,
        total_nodes_with_vals,
        total_nodes,
        min_peers_per_node,
        max_peers_per_node_inc,
        dotfile_name,
        config_file_name,
    } = RunParams::parse();
    let params = gen_topology::Params::new(
        seed,
        total_validators,
        total_nodes_with_vals,
        total_nodes,
        min_peers_per_node,
        max_peers_per_node_inc,
    )?;
    let network = gen_topology::Network::generate(params)?;

    // gen the dotfile
    let mut file =
        std::fs::File::create(dotfile_name).map_err(|e| format!("Failed to create dotfile {e}"))?;
    writeln!(file, "digraph {{").map_err(|e| format!("Failed writing dotfile {e}"))?;
    for (peer_a, dialed_peers) in network.outbound_peers() {
        for peer_b in dialed_peers {
            writeln!(file, "\t{peer_a} -> {peer_b};")
                .map_err(|e| format!("Failed writing dotfile {e}"))?;
        }
    }
    writeln!(file, "}}").map_err(|e| format!("Failed writing dotfile {e}"))?;

    // gen the config file
    let mut file = std::fs::File::create(config_file_name)
        .map_err(|e| format!("Failed creating network file {e}"))?;
    let network_rep = serde_json::to_string_pretty(&network)
        .map_err(|e| format!("Failed to create network file {e}"))?;
    write!(file, "{}", network_rep).map_err(|e| format!("Failed to write network file {e}"))?;

    Ok(())
}
