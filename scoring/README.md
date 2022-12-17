# Scoring Simulation

This simulation creates a number of malicious nodes which do not propagate
received messages, and mimics gossipsub messages on an Ethereum consensus network.

## Running the Simulation

This simulation can be run with the following command (from within the repos
root directory):

```sh
testground run composition -f ./scoring/compositions/simulation.toml --wait
```

Various aspects of the simulation can be modified. Please read the
`scoring/manifest.toml` to understand test parameters and
`scoring/compositions/simulation.toml` to modify them.


## Scenario: Censor Attack Against a Single Target

```mermaid
sequenceDiagram
    participant BeaconNode
    participant BeaconNode without validators
    participant Attackers
    %% Discovery
    Note over BeaconNode,BeaconNode without validators: Setup discovery<br />Connect to some of the honest nodes<br />randomly selected
    BeaconNode->>BeaconNode without validators: Connect
    BeaconNode without validators->>BeaconNode: Connect
    Note over Attackers: Setup discovery<br />Connect to a single target with the highest number of validators
    Attackers->>BeaconNode: Connect to a single target
    %% Subscribe topics
    Note over BeaconNode,BeaconNode without validators: Subscribe topic(s)
    BeaconNode->>BeaconNode without validators: Subscribe/GRAFT
    BeaconNode without validators->>BeaconNode: Subscribe/GRAFT
    BeaconNode->>Attackers: Subscribe/GRAFT
    Note over Attackers: Subscribe to the topic in the message from the target, <br/>and send back Subscribe/GRAFT.
    Attackers->>BeaconNode: Subscribe/GRAFT
    %% Publish messages
    Note over BeaconNode: Periodically publish messages.
    loop Publish messages
        Note over BeaconNode,BeaconNode without validators: Record scores and gossipsub metrics while this loop.
        BeaconNode->>BeaconNode without validators: Message
        BeaconNode->>Attackers: Message
        Note over Attackers: **Don't propagate messages**
    end
    %% Record metrics
    Note over BeaconNode,BeaconNode without validators: Record the number of messages per epoch after the simulation.
```

## Dashboards

Please see the root [README](../README.md) for how to run Grafana.

### Settings

Settings of the specific run. (e.g. topology)

Variables for this dashboard:

- `run_id`: The run_id for the test run you want to see.

<img width="1760" alt="image" src="https://user-images.githubusercontent.com/1885716/204079758-4308a67c-3ae5-4fe4-bb51-bd5b0bd16246.png">


### Messages

The number of messages (min, max, mean) each peers received per epoch.

Variables for this dashboard:

- `run_id`: The run_id for the test run you want to see.

<img width="1613" alt="image" src="https://user-images.githubusercontent.com/1885716/203179770-b3d23257-9817-4956-8b20-eb44cc5ca444.png">


### Peer Scores and Gossipsub Metrics

Variables for this dashboard:

- `run_id`: The run_id for the test run you want to see.
- `peer_id`: The PeerId you want to see.
	- Note: The censoring target ID can be found on the `Settings` dashboard.



<img width="1322" alt="image" src="https://user-images.githubusercontent.com/1885716/206589239-4fb304c3-9d6d-458b-b9c1-32066869c75a.png">
