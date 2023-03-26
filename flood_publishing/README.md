# Flood Publishing Simulation

## Running the Simulation

This simulation can be run with the following command (from within the repos
root directory):

```sh
testground run single --plan gossipsub-testground/flood_publishing --testcase flood_publishing --builder docker:generic --runner local:docker --instances 2 --wait
```
