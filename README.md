# gossipsub-testground

This repository contains [Testground](https://github.com/testground/testground) test plans for [libp2p-gossipsub](https://github.com/libp2p/rust-libp2p/tree/master/protocols/gossipsub).

## Getting started

Before running test plans, make sure the testground daemon is running. See [here](https://docs.testground.ai/getting-started) for how to install and run the daemon.

```shell
# Cloning this repo
git clone https://github.com/sigp/gossipsub-testground.git
# Import the test plans from this repo
testground plan import --from ./gossipsub-testground/
# Run smoke tests
testground run single \
  --plan=gossipsub-testground \
  --testcase=smoke \
  --builder=docker:generic \
  --runner=local:docker \
  --instances=3 \
  --wait
```
