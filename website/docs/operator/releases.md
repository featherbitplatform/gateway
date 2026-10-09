---
title: Releases
description: Operator artifacts, registries, how versions track the gateway, how to verify an image's SBOM, and the operator's metrics.
---

## Artifacts

Each `vX.Y.Z` tag of [`gateway-operator`](https://github.com/featherbitplatform/gateway-operator) publishes:

| Artifact | Location |
|---|---|
| Container image | `featherbit/operator` on Docker Hub only. Multi-arch (`linux/amd64`, `linux/arm64`), `FROM scratch`, non-root (uid 65532), read-only root filesystem. Tags `X.Y.Z` and `X.Y`; `edge` follows `develop`. |
| Helm chart | `oci://ghcr.io/featherbitplatform/charts/featherbit-operator` |
| Helm chart (mirror) | `oci://registry-1.docker.io/featherbit/featherbit-operator` |
| SBOMs | Two CycloneDX files attached to the GitHub release: `featherbit-operator-X.Y.Z.cdx.json` (the crate) and `featherbit-operator-image-X.Y.Z.cdx.json` (the published image). |

The chart and the operator share one version, and the chart's `appVersion` is the image tag it runs. Both registries carry the same chart; pick either:

```bash
helm install featherbit-operator oci://ghcr.io/featherbitplatform/charts/featherbit-operator --version 0.1.0 \
  --namespace featherbit-system --create-namespace
# or
helm install featherbit-operator oci://registry-1.docker.io/featherbit/featherbit-operator --version 0.1.0 \
  --namespace featherbit-system --create-namespace
```

Chart values (webhook TLS, cert-manager, ServiceMonitor, scheduling) are documented in the chart's [README](https://github.com/featherbitplatform/gateway-operator/blob/main/charts/featherbit-operator/README.md) and `values.yaml`.

## Versions track the gateway

The operator pins one gateway library version. Its webhook knows that version's node catalog, and its CRD schemas are generated from that version's types.

- Run an operator **at least as new** as the gateways it serves. An older operator rejects node types a newer gateway accepts.
- Operator minor versions track gateway minor versions: a `0.16.x` operator serves `0.16.x` gateways. Patch versions are independent.
- Schema changes within `v1alpha1` are additive only; removing a field requires a new API version.

Operator `0.1.0` is built against gateway `0.15.x`. File-mode installs also need a gateway chart that has the `config.gatewayConfigMap` value (0.16 or later, or `develop` until released). CRDs are installed from the chart's `crds/` directory, so Helm installs them first and never deletes them: `helm upgrade` does not update CRDs, so apply the new `crds/` manually when a release changes a schema.

## Verify an image

The image is `FROM scratch`, so there is no package manager to inspect. Two things make it auditable:

- The binary is built with [`cargo auditable`](https://github.com/rust-secure-code/cargo-auditable), which embeds the crate dependency list in the executable. Scanners recover the inventory straight from the image.
- Each release attaches CycloneDX SBOMs generated from the crate and from the exact image pushed for that tag. See [SBOM](../reference/sbom.md) for the same scheme on the gateway.

```bash
# Inventory and vulnerability-scan the published image
syft featherbit/operator:0.1.0
grype featherbit/operator:0.1.0

# Scan the SBOM attached to the release instead of pulling the image
grype sbom:featherbit-operator-image-0.1.0.cdx.json
```

To check that the image you run is the one the release describes, compare digests: the release's image SBOM was generated from `featherbit/operator:<tag>` as pushed, and `docker buildx imagetools inspect featherbit/operator:<tag>` prints the manifest digest to pin in `image.digest`. Per-platform build provenance attestations are not published at this time; the SBOM is the supply-chain record for a release.

## Observability

The operator serves Prometheus metrics at `/metrics` on port 8080 (no authentication, cluster-internal), and `/healthz` (process up) and `/readyz` (informers synced, webhook certificate loaded) on the same port. Set `metrics.serviceMonitor.enabled=true` in the chart for a Prometheus Operator `ServiceMonitor`. Logs use `tracing` in `text` or `json` format (`logging.format`), with the level from `logging.level`.

| Metric | Labels | Meaning |
|---|---|---|
| `featherbit_operator_reconcile_total` | `gateway`, `result` | Reconciles per gateway and result. |
| `featherbit_operator_reconcile_duration_seconds` | `gateway` | Reconcile duration. |
| `featherbit_operator_excluded_objects` | `gateway`, `kind`, `reason` | Objects excluded from a gateway's render, by reason. |
| `featherbit_operator_rendered_config_info` | `gateway`, `hash` | Gauge `1` for the hash currently rendered per gateway. |
| `featherbit_operator_webhook_requests_total` | `kind`, `allowed` | Admission requests per kind and verdict. |

A non-zero `featherbit_operator_excluded_objects` is the signal to look at [Conditions](./conditions.md); `featherbit_operator_reconcile_total` with a non-success `result` points at a gateway whose `Ready` condition is `False`.
