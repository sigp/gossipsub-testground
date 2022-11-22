# Smoke Simulation

This is a simple simulation demonstrating how to integrate rust-gossipsub with
testground.


## Running the Simulation

This simulation can be run with the following command:

```sh
testground run single \
  --plan=gossipsub-testground/smoke \
  --testcase=smoke \
  --builder=docker:generic \
  --runner=local:docker \
  --instances=3 \
  --wait
 ```
