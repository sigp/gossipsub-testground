#! /bin/sh
# This script is used to drop and re-create the InfluxDB docker container,
# removing all past testground runs.

docker rm -f testground-influxdb
testground healthcheck --runner local:docker --fix
