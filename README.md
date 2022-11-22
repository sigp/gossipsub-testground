# Gossipsub Testground Simulations


This repository contains a number of simulations aimed for testing and
simulating gossipsub for a variety of experiments.


## Initial Setup

### Testground

The simulations contained within this repository are dependent on [Testground](https://github.com/testground/testground). The simulations are testground test plans for rust [libp2p-gossipsub](https://github.com/libp2p/rust-libp2p/tree/master/protocols/gossipsub).

In order to run any of the simulations, testground must be installed and the
testground daemon must be running. Please see the [testground getting-started](https://docs.testground.ai/getting-started) page for further information for installing and running the testground daemon.


The testground plans must then be imported. This can be done by running the
following command from the parent directory of this repository:
```sh
testground plan import --from ./gossipsub-testground/
```

## Simulations

### [Smoke](./smoke/README.md)

This is a basic run of a few gossipsub nodes sending messages between
themselves. This serves as a simple example of how rust-gossipsub can be
integrated with testground and a simple simulation can be run.

See the [smoke documentation](./smoke/README) for instructions on how to run
the simulation.

### [Censoring](./censoring/README.md)

This simulation constructs a network of malicious peers which attempt to censor
messages on the network. The gossipsub scoring parameters are recorded and can
be examined to determine their effectiveness.

See the [censoring documentation](./censoring/README) for instructions on how to run
the simulation.

## Dashboards

Grafana dashboards are provided for some of the simulations. These are provided
via a grafana docker container with preconfigured data sources and dashboards. 

To access the grafana UI, run (with docker-compose installed):
```sh
docker-compose up -d
```

The grafana UI should then be accessible at: `http://localhost:13000/dashboards`.

For further details of the dashboards please see READMEs in each of the
simulation directories.
