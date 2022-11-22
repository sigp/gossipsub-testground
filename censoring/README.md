# Censoring Simulation

This simulation creates a number of malicious nodes which do not propagate
received messages. This simulation is aimed to help fine-tune gossipsub scoring
parameters to mitigate censoring attacks on gossipsub networks.

## Running the simulation

```shell
testground run composition -f censoring/compositions/censoring.toml --wait
```

## How the Simulation Works

Note: Attackers connect to a single publisher (victim). [`Publisher1` is the victim](https://github.com/sigp/gossipsub-testground/censoring/src/attacker.rs#L76) in this test plan.

```mermaid
sequenceDiagram
    participant Publishers
    participant Lurkers
    participant Attackers

    %% Discovery
    Note over Publishers,Lurkers: Setup discovery<br />Connect to some of the honest nodes (publishers + lukers)<br />randomly selected
    Note over Attackers: Setup discovery<br />Connect to a single publisher
    Publishers->>Lurkers: Connect
    Lurkers->>Publishers: Connect
    Attackers->>Publishers: Connect to a single publisher (victim)

    %% Subscribe topics
    Note over Publishers,Lurkers: Subscribe topic(s)
    Publishers->>Lurkers: Subscribe/GRAFT
    Lurkers->>Publishers: Subscribe/GRAFT
    Publishers->>Attackers: Subscribe/GRAFT
    Note over Attackers: Subscribe to the topic in the message from the publisher, <br/>and send back Subscribe/GRAFT.
    Attackers->>Publishers: Subscribe/GRAFT

    %% Publish messages
    Note over Publishers: Periodically publish messages on all topics subscribing.
    loop Publish messages
        Publishers->>Lurkers: Message
        Publishers->>Attackers: Message
        Note over Attackers: **Don't propagate messages**
    end

    %% Record metrics
    Note over Publishers,Lurkers: Record metrics.
```

## Dashboards

Please see the root [README](https://github.com/sigp/gossipsub-testground/blob/main/README.md) for how to run Grafana.

### Gossipsub Metrics

The metrics of gossipsub are recorded once the simulation has been completed. [All of the metrics on libp2p-gossipsub](https://github.com/sigp/gossipsub-testground/blob/censoring/src/honest.rs#L235-L275) are available in this dashboard.

Variables for this dashboard:

- `run_id`: The run_id for the test run you want to see.
- `topic`: The gossipsub topic, currently it's fixed to `emulate`.
- `instance_name`: Some panels in this dashboard need an instance name to be specified. (e.g. score_per_mesh histogram)

<img width="1675" alt="image" src="https://user-images.githubusercontent.com/1885716/194744972-2d6e5f42-48c7-4c89-8ada-5ab992599046.png">

### Peer Scores

The peer scores are recorded periodically (every second) while the simulation is running.

Variables for this dashboard:

- `run_id`: The run_id for the test run you want to see.
- `instance_name`: It's default to `All`, you can select specific instances.

<img width="1673" alt="image" src="https://user-images.githubusercontent.com/1885716/194745649-323aee65-8b2b-439a-9c5f-c97863a844fd.png">
