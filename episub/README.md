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

`testground run single --plan=gossipsub-testground/episub --testcase=episub --builder=docker:generic --runner=local:docker --instances=16 --test-param config_file="/home/16instances.json"`
