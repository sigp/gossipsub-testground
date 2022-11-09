# emulation

## How to run

```shell
# Import test plans
git clone https://github.com/sigp/gossipsub-testground.git
testground plan import --from ./gossipsub-testground/
```

`testground run single --plan=gossipsub-testground/episub --testcase=episub --builder=docker:generic --runner=local:docker --instances=16`

Check the test params in the manifest.toml Those can be passed as `--test-param param_name=param_value`
Example:

`testground run single --plan=gossipsub-testground/episub --testcase=episub --builder=docker:generic --runner=local:docker --instances=16 --test-param seed=300`

New one, still need to figure out the test params
`testground run composition -f ./composition_episub.toml --wait`

Go to `http://localhost:3000` and log in with `admin`, `admin` to get metrics
