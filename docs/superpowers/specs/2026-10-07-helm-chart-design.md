# Helm chart for the gateway

**Status:** approved in discussion, 2026-10-07
**Branch:** `feature/helm-chart`

## 1. Goal

Make the gateway installable on Kubernetes with one `helm install`, from a chart published as an OCI artifact on both GHCR and Docker Hub, released in lockstep with the gateway and tested in CI on a real cluster.

Constraints agreed in discussion:

- **Chart only.** A Kubernetes operator (CRDs, controller, admission webhook, Gateway API implementation) is a separate future repository. Nothing in this chart anticipates it beyond keeping the config model untouched.
- **Chart name `featherbit-gateway`.** `helm push` derives the target repository from the chart name, and `featherbit/featherbit` on Docker Hub is the image repository; a chart named `featherbit` would push its tags into it. The chart therefore lives at:
  - `oci://ghcr.io/featherbitplatform/charts/featherbit-gateway`
  - `oci://registry-1.docker.io/featherbit/featherbit-gateway`
- **Version lockstep.** Chart `version` and `appVersion` both equal the gateway version (`Cargo.toml`), bumped in the same `chore(release)` commit. The chart publishes on `vX.Y.Z` tags only; there is no `edge` chart.
- **Config-only chart, file source by default.** The chart renders the gateway's own `system.yaml` and `gateway.yaml` into a ConfigMap. Opting into `config.source: etcd` points at an etcd the user already runs; the chart ships no etcd subchart.
- **Admin plane stays internal** unless explicitly exposed.

Success means: on a fresh kind cluster, `helm install featherbit oci://ghcr.io/featherbitplatform/charts/featherbit-gateway` brings up a gateway serving the minimal example config, readiness goes green, a request through the data-plane Service reaches the configured upstream, and the identical chart installs from the Docker Hub reference.

## 2. Approach

An in-repo chart under `charts/featherbit-gateway/`, published by a dedicated `helm.yml` workflow. The chart is a conventional Deployment-based chart with two Services (data plane, admin), generated config in a ConfigMap, credentials in a Secret, and optional edge resources (Ingress or Gateway API `HTTPRoute`) that are off by default.

Alternatives rejected:

- **Classic Helm repository on GitHub Pages.** The docs site already occupies the repo's Pages slot; a chart index would have to live inside the Docusaurus build and be regenerated on every release. OCI needs no index and reuses registries the project already publishes to.
- **Separate `charts` repository.** Reasonable once there are several charts; for one chart it only adds a second release process to keep in sync.
- **Gateway API implementation (GatewayClass/Gateway).** Controller work; operator scope. The chart only renders an `HTTPRoute` that targets the data-plane Service, so featherbit can sit behind a Gateway API implementation the cluster already runs.
- **etcd subchart.** An etcd lifecycle (persistence, quorum, upgrades) is a bigger problem than the gateway chart and is better delegated to the user's existing etcd or an etcd operator.

## 3. Layout

```
charts/featherbit-gateway/
  Chart.yaml                 # name, version/appVersion = gateway version, home, sources, maintainers
  values.yaml                # documented defaults (every key commented)
  values.schema.json         # JSON Schema for values; `helm lint` and `helm install` validate against it
  README.md                  # install, values table, upgrade notes
  templates/
    _helpers.tpl             # names, labels, selector labels, image ref, admin auth header
    deployment.yaml
    service.yaml             # data plane
    service-admin.yaml       # admin plane
    configmap.yaml           # system.yaml + gateway.yaml
    configmap-scripts.yaml   # Lua scripts (when any are given)
    secret.yaml              # admin credentials + MCP tokens (unless existingSecret)
    serviceaccount.yaml
    ingress.yaml             # data plane, optional
    ingress-admin.yaml       # admin plane, optional
    httproute.yaml           # Gateway API, optional
    hpa.yaml                 # optional
    pdb.yaml                 # optional
    servicemonitor.yaml      # optional (Prometheus Operator)
    NOTES.txt                # how to reach the data plane and admin UI after install
    tests/
      test-connection.yaml   # `helm test`: curl /healthz with the admin credentials
  ci/
    default-values.yaml      # what `ct install` exercises
    ingress-values.yaml
    etcd-values.yaml         # template-only (kubeconform), no etcd in CI
    all-options-values.yaml  # template-only: every optional resource on
.github/workflows/helm.yml
artifacthub-repo.yml         # ownership metadata, at the chart root (registration itself is a follow-up)
```

The chart is independent of the Rust crate: it does not affect `cargo build`, `cargo test`, or the Docker image.

## 4. Chart design

### 4.1 Workload

- **Deployment**, `replicas` default 1, `strategy: RollingUpdate`. Stateless by design; HA is etcd mode plus redis-backed stores, documented, not enforced.
- **Image:** `featherbit/featherbit`, tag defaults to `.Chart.AppVersion`. `image.headless: true` appends `-headless` to the tag. `image.digest` pins by digest when set.
- **Container ports:** `http` (data plane, default 8080) and `admin` (default 9090), plus one port per entry in `stream.ports` (see 4.5).
- **Security context** matching the `FROM scratch` image: `runAsNonRoot`, `runAsUser`/`runAsGroup` 65532, `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, drop all capabilities, `seccompProfile: RuntimeDefault`. An `emptyDir` at `/var/lib/featherbit` is mounted for anything the gateway writes (ACME filesystem storage is the only current writer).
- **Graceful shutdown:** `terminationGracePeriodSeconds` defaults to `timeouts.shutdown_timeout_seconds + 5` (computed in the template from `config.system.timeouts.shutdown_timeout_seconds`, default 30, so 35).
- **Probes** target the admin port. `/healthz` for liveness, `/readyz` for readiness and startup. Both admin endpoints sit behind Basic Auth, so every HTTP probe carries an `Authorization: Basic <base64>` header. The header value is built in `_helpers.tpl` from the admin credentials when the chart manages the Secret. When `admin.existingSecret` is used the chart cannot read the credentials, so the probes fall back to `tcpSocket` on the admin port unless `probes.authHeader` is given explicitly. The README documents this trade-off.
- **Config mounts:** the ConfigMap at `/etc/gateway` (read-only), the scripts ConfigMap at `/etc/gateway/plugins` when present. The binary's default CMD already points at `/etc/gateway/system.yaml` and `/etc/gateway/gateway.yaml`, so no `command`/`args` override is needed.
- **Hot reload on ConfigMap change:** the file watcher watches the config directory recursively for create/modify events, so the kubelet's symlink swap on ConfigMap update triggers a reload of `gateway.yaml` without a restart. `system.yaml` is not hot-reloaded by the gateway, so the Deployment carries a `checksum/config` annotation computed over the rendered **system** config only: changing `system.yaml` rolls the pods, changing `gateway.yaml` reloads in place. The README states this.

### 4.2 Configuration

Values expose the gateway's own config, not a parallel schema:

```yaml
config:
  source: file            # file | etcd
  system: {}              # merged over the chart's default system.yaml (map, rendered to YAML)
  gateway: {}             # the full gateway.yaml as a map (routes, policies, stores, ...)
  gatewayRaw: ""          # alternatively, the literal file (wins over `gateway` when set)
  etcd:
    endpoints: []
    prefix: /featherbit
    existingSecret: ""    # keys: user, password (optional)
  scripts: {}             # filename -> Lua source, mounted at /etc/gateway/plugins
```

- `config.system` is **deep-merged** over a chart-embedded default that mirrors `config/system.yaml` (bind `0.0.0.0`, ports from values, `${ADMIN_USER}`/`${ADMIN_PASSWORD}` placeholders, logging level from values). Users override individual keys (`timeouts`, `http2`, `debug`, `tls`, `acme`, `admin.mcp`, …) without restating the file.
- `config.gateway` defaults to the minimal example's one-route config (route `/*` to an upstream read from `${UPSTREAM_HOST}`/`${UPSTREAM_PORT}`), so a bare install serves something and `helm test` can prove the data plane works. The README's first real step is replacing it.
- `${ENV}` placeholders are preserved verbatim in the ConfigMap. Secrets arrive as environment variables (4.3), which is exactly how the gateway's interpolation already works, so the Admin API and UI never see resolved secrets.
- `config.source: etcd` renders the `config:` block of `system.yaml` from `config.etcd` and drops `gateway.yaml` from the ConfigMap (etcd seeds from nothing or from what is already there).

### 4.3 Secrets and environment

```yaml
admin:
  username: admin
  password: ""            # empty -> randomly generated once (lookup-preserving) and kept across upgrades
  existingSecret: ""      # keys: username, password
  users: []               # extra full-access accounts, rendered into system.yaml with ${ADMIN_USER_<N>} placeholders
  uiEnabled: true
mcp:
  enabled: false
  path: /mcp
  tokens: []              # [{name, scope: read|write, value}] or
  existingSecret: ""      # keys: one per token name
  allowedOrigins: []
extraEnv: []              # raw env entries
extraEnvFrom: []          # raw envFrom entries
```

The Deployment uses `envFrom` on the managed (or existing) Secret. A random admin password uses the `lookup`-then-`randAlphaNum` idiom so `helm upgrade` does not rotate it; `helm template` (no cluster) still renders deterministically because `lookup` returns empty there and the template falls back to a fresh value.

### 4.4 Networking

```yaml
service:
  type: ClusterIP
  port: 80                # Service port; targetPort is the container's http port
  annotations: {}
  externalTrafficPolicy: ""  # for LoadBalancer/NodePort
adminService:
  type: ClusterIP
  port: 9090
ingress:
  enabled: false
  className: ""
  annotations: {}
  hosts: [{host: gateway.example.com, paths: [{path: /, pathType: Prefix}]}]
  tls: []
adminIngress:
  enabled: false          # same shape; off by default on purpose
httpRoute:
  enabled: false
  parentRefs: []          # [{name, namespace, sectionName}]
  hostnames: []
  rules: []               # defaults to one rule matching PathPrefix / -> data-plane Service
tls:
  existingSecret: ""      # kubernetes.io/tls; mounted at /etc/gateway/tls, wired into system.yaml tls.cert_path/key_path
  adminInherit: false     # system.yaml admin.tls.inherit
```

- The two edge options are mutually exclusive in intent but not enforced; `values.schema.json` only validates shapes.
- Enabling `tls.existingSecret` sets `tls.cert_path`/`key_path` in the rendered system config; the gateway's cert watcher picks up Secret rotations because the kubelet updates the mounted files.
- ACME is configured purely through `config.system.acme` and `config.system.tls.acme`; the chart only guarantees the writable `emptyDir` for `storage.type: filesystem`. The README states that TLS-ALPN-01 requires the gateway to be reachable on 443 directly (LoadBalancer Service, not an Ingress) and that multiple replicas need the redis storage backend.

### 4.5 Optional resources (all off by default)

- `stream.ports: [{name, port, protocol: TCP|UDP}]` adds container ports and a `streamService` (own type/annotations) for L4 listeners declared in `config.system.stream`.
- `autoscaling` (HPA on CPU/memory), `podDisruptionBudget` (`minAvailable`/`maxUnavailable`).
- `serviceMonitor` for the Prometheus Operator, scraping the admin Service at `/metrics` with `basicAuth` referencing the admin Secret.
- `serviceAccount` (create/name/annotations), `podAnnotations`, `podLabels`, `nodeSelector`, `tolerations`, `affinity`, `topologySpreadConstraints`, `resources`, `priorityClassName`, `extraVolumes`, `extraVolumeMounts`, `extraContainers`, `initContainers`.

### 4.6 `NOTES.txt` and `helm test`

`NOTES.txt` prints the port-forward command for the admin UI, the admin username and how to read the password from the Secret, and how to reach the data plane for the chosen Service/Ingress/HTTPRoute mode.

`helm test` runs a `curlimages/curl` pod (pinned tag) that hits `/healthz` on the admin Service with the credentials from the Secret and then sends one request through the data-plane Service. It is what `ct install` executes in CI.

## 5. CI and publishing (`.github/workflows/helm.yml`)

Triggers: pull requests and pushes to `develop`/`main` that touch `charts/**` or the workflow; `vX.Y.Z` tags; `workflow_dispatch`.

**`lint` job** (every trigger):
1. `helm lint --strict` with each `ci/*-values.yaml`.
2. `chart-testing` `ct lint` (version-bump check disabled, since the chart version is bumped by the release process rather than per PR; `--validate-maintainers` off, the org has no GitHub-user maintainers).
3. `helm template` with each `ci/*-values.yaml` piped through `kubeconform -strict` with the Gateway API `HTTPRoute` schema and the Prometheus Operator `ServiceMonitor` schema fetched from the CRDs catalog, so the optional resources are schema-checked without a cluster.
4. `trivy config charts/` (same trivy the security workflow already pins), reporting misconfigurations at the thresholds in `trivy.yaml`.

**`install` job** (PRs and branch pushes): `helm/kind-action` creates a cluster, Gateway API CRDs are applied (so `httpRoute` renders cleanly in the all-options values; no implementation is needed for the test), `ct install` installs each `ci/*-values.yaml` that is cluster-capable (`default`, `ingress` — the etcd and all-options files are template-only), and `helm test` is run by `ct`. The image under test is `featherbit/featherbit:edge` on develop PRs, overridden to the computed release tag on `main`.

**`publish` job** (tags only, `needs: lint`): asserts `Chart.yaml` `version` and `appVersion` equal the tag; `helm package`; `helm push` to `oci://ghcr.io/featherbitplatform/charts` (login with `GITHUB_TOKEN`, `packages: write`) and to `oci://registry-1.docker.io/featherbit` (login with the existing `DOCKER_USERNAME`/`DOCKER_PASSWORD` secrets). The packaged `.tgz` is attached to the GitHub release alongside the SBOMs. Both registries create the repository on first push; the GHCR package must be switched to public once by hand in the org's package settings.

Not in this PR (follow-ups): cosign signing of the chart, Artifact Hub registration (the `artifacthub-repo.yml` is committed so registration is a one-time UI action), chart provenance files.

**Local SAST:** `dev/sast.ps1`/`dev/sast.sh` gain the `trivy config charts/` step so the local pipeline matches CI.

## 6. Release process changes

- `chore(release)` bumps `Cargo.toml`, `Cargo.lock`, and `charts/featherbit-gateway/Chart.yaml` (`version` and `appVersion`). A `cargo test` in `src/` is the wrong place to enforce this (the chart is outside the crate), so the `publish` job's tag assertion is the guard: a mismatched Chart.yaml fails the chart publish without affecting the image publish in `docker.yml`.
- The image for a release is pushed by `docker.yml` on the same tag; the chart's default tag is `appVersion`, so the two land together. The `helm.yml` publish job does not wait for `docker.yml`; a user who installs within the few minutes between them gets an `ImagePullBackOff` that resolves on its own. Documented, not engineered around.

## 7. Documentation

- `website/docs/guides/deployment.md`: a new **Kubernetes (Helm)** section after the Docker Hub section: install from either registry, the minimal `values.yaml` for the two exposure modes (LoadBalancer edge vs behind Ingress/HTTPRoute), supplying your own `gateway.yaml`, secrets via `existingSecret`, etcd mode, the system-vs-gateway reload distinction, ACME caveats, and the admin-plane exposure warning. The existing "Stateless multi-instance deployment" paragraph is revised to point at the chart instead of describing a hand-written ConfigMap mount.
- `charts/featherbit-gateway/README.md`: the values reference (generated by hand in this PR, tabular), kept in sync with `values.yaml` comments.
- `README.md` and `DOCKERHUB.md`: a Helm install snippet next to the Docker pull snippet.
- `website/docs/reference/roadmap.md`: a "Kubernetes: Helm chart" row marked implemented, with "Kubernetes operator (CRDs, Gateway API implementation)" listed as planned in a separate repository.
- `CLAUDE.md`: a line under build commands for `helm lint`/`ct lint` and the chart's location, and a note that the chart version is bumped with the crate version.

## 8. Testing

- **Template tests** are the `ci/*-values.yaml` matrix under `helm lint --strict`, `ct lint`, and kubeconform; any new optional resource adds itself to `all-options-values.yaml`.
- **Install tests** are `ct install` plus the chart's `helm test` hook in kind.
- **Locally**, before the PR: `helm lint`, `helm template` against every `ci/` file, kubeconform, and a kind install with `helm test` using the `edge` image. The user verifies on their own cluster before merging, per the usual workflow.

## 9. Out of scope

- The operator, CRDs, admission webhook, and any Gateway API *implementation*.
- An etcd subchart or any bundled redis.
- An `edge`/develop chart channel.
- Chart signing and Artifact Hub registration (follow-ups listed above).
