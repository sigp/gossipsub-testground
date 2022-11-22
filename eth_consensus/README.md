# Ethereum Consensus Simulation

This simulation mimics the timing, frequency and sizes of messages that would usually
occur on a normal Ethereum consensus gossipsub-network. The simulation can
specify the number of validators and nodes on the network in an attempt to
model various sizes of Ethereum consensus networks.

## Running the Simulation

This simulation can be run with the following command (from within the repos
root directory):

```sh
testground run composition -f ./eth_consensus/compositions/composition.toml --wait
```

Various aspects of the simulation can be modified. Please read the `eth_consensus/manifest.toml` to understand test parameters and `eth_consensus/compositions/composition.toml` to modify them.


## Influx DB Queries

The results of the simulation are stored in an Influx DB instance. Queries
inside grafana can be used to build dashboards. An example query is given:

`SELECT derivative("count", 10s) FROM "topic_msg_recv_bytes" WHERE $timeFilter GROUP BY "hash", "instance_name", "run_id"`

`derivative` = calculation of the rate
`hash` the topic
`instance_name` the number given to the instance inside the test run starting from 0
`run_id` the id of the run
