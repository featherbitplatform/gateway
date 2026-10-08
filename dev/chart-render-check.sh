#!/usr/bin/env bash
# dev/chart-render-check.sh — asserts on `helm template` output for the
# config.gatewayConfigMap value. Run from the repo root; exits non-zero on
# the first failed assertion.
set -euo pipefail
CHART=charts/featherbit-gateway

fail() { echo "FAIL: $1" >&2; exit 1; }

# 1. Default render: chart ConfigMap carries gateway.yaml, no external source.
default=$(helm template fb "$CHART")
grep -q '^  gateway.yaml: |' <<<"$default" || fail "default render lacks gateway.yaml in the chart ConfigMap"
grep -q 'optional: true' <<<"$default" && fail "default render must not reference an external ConfigMap"

# 2. External ConfigMap: key gone from the chart ConfigMap, optional projected source present.
ext=$(helm template fb "$CHART" --set config.gatewayConfigMap=edge-gateway-config)
grep -q '^  gateway.yaml: |' <<<"$ext" && fail "external render still renders gateway.yaml in the chart ConfigMap"
grep -q '^  system.yaml: |' <<<"$ext" || fail "external render lost system.yaml"
grep -A4 'name: edge-gateway-config' <<<"$ext" | grep -q 'optional: true' || fail "external ConfigMap source is not optional"
grep -A6 'name: edge-gateway-config' <<<"$ext" | grep -q 'path: gateway.yaml' || fail "external source does not project gateway.yaml"

# 3. Works together with etcd mode (the file is only a seed there).
helm template fb "$CHART" --set config.gatewayConfigMap=edge-gateway-config --set config.source=etcd \
  --set 'config.etcd.endpoints[0]=http://etcd:2379' >/dev/null || fail "etcd + external ConfigMap does not render"

# 4. NOTES mention the external ConfigMap (`helm template` does not render
#    NOTES.txt; a dry-run install does, without cluster access).
helm install fb "$CHART" --dry-run=client --set config.gatewayConfigMap=edge-gateway-config \
  | grep -q 'mounted from ConfigMap edge-gateway-config' || fail "NOTES do not mention the external ConfigMap"

# 5. With an external ConfigMap there is no known route to probe: neither the
#    test hook nor NOTES may mention the demo /hello route.
grep -q '/hello' <<<"$ext" && fail "external render still mentions the demo /hello route"
helm install fb "$CHART" --dry-run=client --set config.gatewayConfigMap=edge-gateway-config   | grep -q '/hello' && fail "NOTES with an external ConfigMap still mention /hello"

# 6. The schema rejects a ConfigMap name that is not a DNS-1123 name.
helm template fb "$CHART" --set config.gatewayConfigMap='Not Valid' >/dev/null 2>&1   && fail "schema accepted an invalid gatewayConfigMap name"

echo "chart render check: OK"
