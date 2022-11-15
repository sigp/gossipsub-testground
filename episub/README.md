# emulation

## How to run

```shell
# Import test plans
git clone https://github.com/sigp/gossipsub-testground.git
testground plan import --from ./gossipsub-testground/
```

`testground run composition -f ./composition_episub.toml --wait`

Go to `http://localhost:3000` and log in with `admin`, `admin` to get metrics

Add the influx data source with `http://testground-influxdb:8086` and `Database` name `testground`

Read the `manifest.toml` to understand test params and `composition_episub.toml` to change them

Example query: 

`SELECT derivative("count", 10s) FROM "topic_msg_recv_bytes" WHERE $timeFilter GROUP BY "hash", "instance_name", "run_id"`

`derivative` = calculation of the rate
`hash` the topic
`instance_name` the number given to the instance inside the test run starting from 0
`run_id` the id of the run
