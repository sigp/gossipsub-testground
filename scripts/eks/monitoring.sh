#!/usr/bin/env bash

# This script deploys Grafana resources to the EKS cluster.
# Our custom dashboards and datasource settings are preset to the deployed grafana instance.

set -Eeuo pipefail

create_configmap() {
  # The name of configmap must consist of lower case alphanumeric characters, '-' or '.'
  name="$1_$(basename -s ".json" "$2")"
  name=$(echo "$name" | tr "[:upper:]" "[:lower:]" | tr "_" "-")
  kubectl create configmap "dashboard-$name" --from-file="$2"
  kubectl label configmap "dashboard-$name" grafana_dashboard=1
  kubectl annotate configmap "dashboard-$name" grafana_dashboard_folder="$1"
}

create_configmaps() {
  find "${project_root}/$1/dashboards/" -name "*.json" -type f | while read -r fname
  do
    create_configmap "$1" "${fname}"
  done
}

readonly script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd -P)
readonly project_root=$(cd "${script_dir}/../../" &>/dev/null && pwd -P)

# Create configmaps for dashboards
# https://github.com/grafana/helm-charts/blob/main/charts/grafana/README.md#sidecar-for-dashboards
create_configmaps "censoring"
create_configmaps "eth_consensus"
create_configmaps "scoring"

# Install Grafana
echo -e "Now obtaining the helm charts and installing Grafana. This might take a few minutes.\n"
helm repo add grafana https://grafana.github.io/helm-charts
helm repo update
helm install gossipsub-grafana grafana/grafana -f "${script_dir}/grafana-variables.yaml"

echo -e "\n============================================================================================================="
echo -e "You may run the following in order to obtain the grafana pod name:"
echo -e 'export POD_NAME=$(kubectl get pods --namespace default -l "app.kubernetes.io/name=grafana,app.kubernetes.io/instance=gossipsub-grafana" -o jsonpath="{.items[0].metadata.name}")\n'
echo -e "You may now run the following in order to port-forward and access the grafana dashboard from your laptop by opening 'localhost:3000' in your browser:"
echo -e 'kubectl --namespace default port-forward $POD_NAME 3000'
echo -e "============================================================================================================\n"
