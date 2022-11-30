#!/bin/sh

# Run this from the root directory of the repository
# i.e ./scripts/run_eth_consensus.sh

cd eth_consensus
cargo clean
cd ..
testground run composition -f ./eth_consensus/compositions/composition.toml --wait
