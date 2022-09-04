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

Note: Attackers connect to a single publisher (victim).

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

## Metrics / Scores

- Open Grafana: http://localhost:3000
- admin/admin by default

### Datasource settings

- URL http://testground-influxdb:8086
- Database testground

<img width="800" alt="image" src="https://user-images.githubusercontent.com/1885716/186311134-8980bb82-3e59-469d-a752-8c4442257862.png">

### Gossipsub metrics

The metrics of gossipsub are recorded once the emulation has been completed. All of the metrics available on libp2p-gossipsub are recorded.

```sql
SELECT * FROM "topic_msg_sent_counts" WHERE $timeFilter 
```

<img width="800" alt="image" src="https://user-images.githubusercontent.com/1885716/187597882-4ff2d68b-88d4-4f51-ac32-87fe8d85d536.png">


### Scores

The peer scores are recorded periodically (every second) while the emulation is running.

```sql
SELECT * FROM "scores" WHERE $timeFilter and instance_name = 'Publisher1' and run_id = 'cc7gl2k3r49i9pbqr9ng'
```

You can identify the score to get by [`instance_name`, `instance_peer_id`, or `run_id`](https://github.com/ackintosh/gossipsub-testground/blob/test-plan-emulation/emulation/src/honest.rs#L408-L410).

Note: [`Publisher1` is a victim in this test plan](https://github.com/ackintosh/gossipsub-testground/blob/0d715d79e0a75300e30fdacf97c7d9961fd6f5af/emulation/src/attacker.rs#L76).

<img width="800" alt="image" src="https://user-images.githubusercontent.com/1885716/187617292-81e387f0-04c5-4e93-9e30-f8fbc6eadf3b.png">
