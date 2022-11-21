# emulation

## How to run

```shell
# Import test plans
git clone https://github.com/sigp/gossipsub-testground.git
testground plan import --from ./gossipsub-testground/

# Run `emulation` test plan
cd gossipsub-testground
testground run composition -f emulation/compositions/emulation.toml --wait
```

## What the emulation does

Note: Attackers connect to a single publisher (victim). [`Publisher1` is the victim](https://github.com/ackintosh/gossipsub-testground/blob/0d715d79e0a75300e30fdacf97c7d9961fd6f5af/emulation/src/attacker.rs#L76) in this test plan.

```mermaid
sequenceDiagram
    participant Publishers
    participant Lukers
    participant Attackers

    %% Discovery
    Note over Publishers,Lukers: Setup discovery<br />Connect to some of the honest nodes (publishers + lukers)<br />randomly selected
    Note over Attackers: Setup discovery<br />Connect to a single publisher
    Publishers->>Lukers: Connect
    Lukers->>Publishers: Connect
    Attackers->>Publishers: Connect to a single publisher (victim)

    %% Subscribe topics
    Note over Publishers,Lukers: Subscribe topic(s)
    Publishers->>Lukers: Subscribe/GRAFT
    Lukers->>Publishers: Subscribe/GRAFT
    Publishers->>Attackers: Subscribe/GRAFT
    Note over Attackers: Subscribe to the topic in the message from the publisher, <br/>and send back Subscribe/GRAFT.
    Attackers->>Publishers: Subscribe/GRAFT

    %% Publish messages
    Note over Publishers: Periodically publish messages on all topics subscribing.
    loop Publish messages
        Publishers->>Lukers: Message
        Publishers->>Attackers: Message
        Note over Attackers: **Don't propagate messages**
    end

    %% Record metrics
    Note over Publishers,Lukers: Record metrics.
```

## Dashboards

Please see the root [README](https://github.com/sigp/gossipsub-testground/blob/main/README.md) for how to run Grafana.

### Gossipsub Metrics

The metrics of gossipsub are recorded once the emulation has been completed. [All of the metrics on libp2p-gossipsub](https://github.com/ackintosh/gossipsub-testground/blob/test-plan-emulation/emulation/src/honest.rs#L235-L275) are available in this dashboard.

Variables for this dashboard:

- `run_id`: The run_id for the test run you want to see.
- `topic`: The gossipsub topic, currently it's fixed to `emulate`.
- `instance_name`: Some panels in this dashboard need an instance name to be specified. (e.g. score_per_mesh histogram)

<img width="1675" alt="image" src="https://user-images.githubusercontent.com/1885716/194744972-2d6e5f42-48c7-4c89-8ada-5ab992599046.png">

### Peer Scores

The peer scores are recorded periodically (every second) while the emulation is running.

Variables for this dashboard:

- `run_id`: The run_id for the test run you want to see.
- `instance_name`: It's default to `All`, you can select specific instances.

<img width="1673" alt="image" src="https://user-images.githubusercontent.com/1885716/194745649-323aee65-8b2b-439a-9c5f-c97863a844fd.png">
