# Helm Chart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `charts/featherbit-gateway`, a Helm chart that installs the gateway on Kubernetes, lint/install-tested in CI and published as an OCI artifact to GHCR and Docker Hub on every `vX.Y.Z` tag.

**Architecture:** A conventional Deployment-based chart. The gateway's own `system.yaml` and `gateway.yaml` are rendered into a ConfigMap (system config deep-merged over a chart default, gateway config passed through verbatim), credentials arrive as env vars from a Secret so the gateway's `${ENV}` interpolation does the secret handling, and two Services split the data plane (8080) from the admin plane (9090). Optional resources (Ingress, HTTPRoute, HPA, PDB, ServiceMonitor, stream Service) are all off by default. A dedicated `helm.yml` workflow lints, kubeconform-checks, trivy-scans and kind-installs the chart on PRs, and packages + pushes it on release tags.

**Tech Stack:** Helm 3 (OCI, `lookup`, `mergeOverwrite`), chart-testing (`ct`), kubeconform, kind, trivy config scanning, GitHub Actions. No Rust changes except one drift test.

**Spec:** `docs/superpowers/specs/2026-10-07-helm-chart-design.md`

## Global Constraints

- Chart name is exactly `featherbit-gateway` (never `featherbit`): `helm push` names the OCI repository after the chart, and `featherbit/featherbit` on Docker Hub is the image.
- Chart `version` and `appVersion` always equal the crate version in `Cargo.toml` (`0.14.0` today). They are bumped only in `chore(release)` commits.
- Publishing happens on `vX.Y.Z` tags only. No `edge` chart.
- Image defaults: repository `featherbit/featherbit`, tag `.Chart.AppVersion`, `-headless` suffix when `image.headless: true`.
- Container runs as uid/gid `65532`, read-only root filesystem; writable `emptyDir` at `/var/lib/featherbit`.
- Admin plane is `ClusterIP` with no Ingress unless `adminIngress.enabled`.
- `${ENV}` placeholders in rendered config must survive verbatim (never resolved by the chart).
- Every optional resource ships off by default and is exercised by `ci/template-only/all-options-values.yaml`.
- Commits follow Conventional Commits (`feat(helm): …`, `ci(helm): …`, `docs(helm): …`, `test: …`). No `Co-Authored-By` trailer.
- Files are LF. Do not commit the untracked `*.png` files sitting in the repo root.
- The Rust crate is untouched except Task 10's drift test. Never run two `cargo` builds concurrently on this machine.

## Review Focus

1. **Gateway-side template syntax in values** — `gateway.yaml` bodies legitimately contain `{{error.code}}`-style text and `$var` references; the rendered ConfigMap must carry them verbatim. Pinned in Task 2, Step 5 (`grep -F '{{error.code}}'`).
2. **`admin.existingSecret` combined with `admin.users`** — the chart cannot append accounts to a Secret it does not own; rendering must fail with a message naming the fix, not silently drop accounts. Pinned in Task 3, Step 7.
3. **Lua script filenames** — keys like `auth.lua` must become `plugins/auth.lua` under `/etc/gateway`, the directory the gateway's script loader and hot-reload watcher expect. Pinned in Task 2, Step 8.
4. **`autoscaling.enabled` with `replicaCount`** — the Deployment must omit `replicas` when an HPA owns it, or every `helm upgrade` scales the fleet back down. Pinned in Task 7, Step 4.
5. **Random admin password across `helm upgrade`** — the lookup idiom must keep the generated password; rotating it on every upgrade would lock the UI, the probes (once HTTP) and the ServiceMonitor out. Pinned in Task 9, Step 6 (kind install → upgrade → Secret unchanged).

---

## File Structure

```
charts/featherbit-gateway/
  Chart.yaml                         Task 1
  values.yaml                        Tasks 1–8 (grows per task; every key commented)
  values.schema.json                 Task 8
  README.md                          Task 8
  .helmignore                        Task 1
  templates/
    _helpers.tpl                     Task 1 (names/labels/image) + Task 2 (config) + Task 3 (secret/probe helpers)
    configmap.yaml                   Task 2
    configmap-scripts.yaml           Task 2
    secret.yaml                      Task 3
    serviceaccount.yaml              Task 4
    deployment.yaml                  Task 4
    service.yaml                     Task 5
    service-admin.yaml               Task 5
    service-stream.yaml              Task 5
    ingress.yaml                     Task 6
    ingress-admin.yaml               Task 6
    httproute.yaml                   Task 6
    hpa.yaml                         Task 7
    pdb.yaml                         Task 7
    servicemonitor.yaml              Task 7
    NOTES.txt                        Task 8
    tests/test-connection.yaml       Task 8
  ci/
    default-values.yaml              Task 8 (ct install)
    ingress-values.yaml              Task 8 (ct install)
    template-only/
      etcd-values.yaml               Task 2
      all-options-values.yaml        Task 8 (lint/kubeconform only)
  artifacthub-repo.yml               Task 11
.github/workflows/helm.yml           Task 11
.github/ct-lintconf.yaml             Task 11
dev/sast.ps1, dev/sast.sh            Task 11 (new `helm` target)
src/admin/status.rs                  Task 10 (version drift test)
website/docs/guides/deployment.md    Task 12
website/docs/reference/roadmap.md    Task 12
README.md, DOCKERHUB.md, CLAUDE.md   Task 12
```

Rendering convention used by every task's test steps, from the repo root in Git Bash:

```bash
C=charts/featherbit-gateway
R=$(mktemp -d)            # scratch dir for rendered output
helm template fb "$C" > "$R/default.yaml"
```

`fb` is the release name used throughout, so `fullname` renders as `fb-featherbit-gateway`.

---

### Task 0: Local tooling

**Files:** none in the repo.

- [ ] **Step 1: Install helm, kind and kubectl**

```powershell
winget install --id Helm.Helm -e --accept-source-agreements --accept-package-agreements
winget install --id Kubernetes.kind -e
winget install --id Kubernetes.kubectl -e
```

Open a new shell afterwards (PATH changes do not reach the running one).

- [ ] **Step 2: Verify**

Run: `helm version --short && kind version && kubectl version --client`
Expected: three version lines, Helm ≥ 3.14.

- [ ] **Step 3: Pull the containerised linters (kubeconform and chart-testing are not installed natively)**

```bash
docker pull ghcr.io/yannh/kubeconform:v0.6.7
docker pull quay.io/helmpack/chart-testing:v3.12.0
```

Expected: both pulls succeed. They are invoked in Tasks 8 and 9 as:

```bash
# kubeconform over a rendered file
docker run --rm -i ghcr.io/yannh/kubeconform:v0.6.7 -strict -summary < "$R/default.yaml"
```

---

### Task 1: Chart skeleton

**Files:**
- Create: `charts/featherbit-gateway/Chart.yaml`
- Create: `charts/featherbit-gateway/.helmignore`
- Create: `charts/featherbit-gateway/values.yaml`
- Create: `charts/featherbit-gateway/templates/_helpers.tpl`

**Interfaces:**
- Produces template helpers used by every later task: `featherbit-gateway.name`, `featherbit-gateway.fullname`, `featherbit-gateway.chart`, `featherbit-gateway.labels`, `featherbit-gateway.selectorLabels`, `featherbit-gateway.image`. Produces values keys `nameOverride`, `fullnameOverride`, `image.{repository,tag,digest,headless,pullPolicy}`, `imagePullSecrets`, `containerPorts.{http,admin}`.

- [ ] **Step 1: Write Chart.yaml**

```yaml
apiVersion: v2
name: featherbit-gateway
description: featherbit — a single-binary API gateway with node-graph routing policies
type: application
# version and appVersion track the gateway version in Cargo.toml and are bumped
# in the same chore(release) commit; src/admin/status.rs has a test for it.
version: 0.14.0
appVersion: "0.14.0"
kubeVersion: ">=1.25.0-0"
home: https://featherbitplatform.github.io/gateway/
icon: https://raw.githubusercontent.com/featherbitplatform/gateway/main/banner.png
sources:
  - https://github.com/featherbitplatform/gateway
keywords:
  - api-gateway
  - gateway
  - proxy
  - rust
maintainers:
  - name: featherbit
    url: https://github.com/featherbitplatform
annotations:
  artifacthub.io/license: Apache-2.0
  artifacthub.io/links: |
    - name: Documentation
      url: https://featherbitplatform.github.io/gateway/
    - name: Docker Hub
      url: https://hub.docker.com/r/featherbit/featherbit
```

Check the repo `LICENSE` file's license and correct `artifacthub.io/license` if it is not Apache-2.0.

- [ ] **Step 2: Write .helmignore**

```
.DS_Store
.git/
.gitignore
*.swp
*.bak
*.tmp
*.orig
*~
ci/
README.md.gotmpl
```

- [ ] **Step 3: Write the initial values.yaml**

```yaml
# Default values for featherbit-gateway.
#
# Documentation: https://featherbitplatform.github.io/gateway/guides/deployment#kubernetes-helm

nameOverride: ""
fullnameOverride: ""

image:
  repository: featherbit/featherbit
  # Defaults to the chart's appVersion.
  tag: ""
  # Pin by digest instead of tag (sha256:...). Takes precedence over tag.
  digest: ""
  # Use the -headless image variant (no embedded web UI).
  headless: false
  pullPolicy: IfNotPresent
imagePullSecrets: []

# Ports the gateway listens on inside the container. Rendered into system.yaml.
containerPorts:
  http: 8080
  admin: 9090
```

- [ ] **Step 4: Write _helpers.tpl (names, labels, image)**

```
{{/*
Chart name, release-scoped full name, labels.
*/}}
{{- define "featherbit-gateway.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "featherbit-gateway.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "featherbit-gateway.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "featherbit-gateway.labels" -}}
helm.sh/chart: {{ include "featherbit-gateway.chart" . }}
{{ include "featherbit-gateway.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "featherbit-gateway.selectorLabels" -}}
app.kubernetes.io/name: {{ include "featherbit-gateway.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Image reference: repository@digest, or repository:tag with the optional
-headless suffix. The tag defaults to appVersion so chart and gateway
versions move together.
*/}}
{{- define "featherbit-gateway.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- if .Values.image.headless }}{{- $tag = printf "%s-headless" $tag }}{{- end -}}
{{- if .Values.image.digest -}}
{{ printf "%s@%s" .Values.image.repository .Values.image.digest }}
{{- else -}}
{{ printf "%s:%s" .Values.image.repository $tag }}
{{- end -}}
{{- end }}
```

- [ ] **Step 5: Lint the skeleton**

Run: `helm lint --strict charts/featherbit-gateway`
Expected: `1 chart(s) linted, 0 chart(s) failed` (an INFO about no templates is fine).

- [ ] **Step 6: Commit**

```bash
git add charts/featherbit-gateway
git commit -m "feat(helm): add featherbit-gateway chart skeleton"
```

---

### Task 2: Config rendering (ConfigMap, system/gateway config, scripts)

**Files:**
- Modify: `charts/featherbit-gateway/values.yaml` (append `logging`, `config`, `admin.uiEnabled`, `mcp`, `tls` placeholders used by the system config)
- Modify: `charts/featherbit-gateway/templates/_helpers.tpl` (append config helpers)
- Create: `charts/featherbit-gateway/templates/configmap.yaml`
- Create: `charts/featherbit-gateway/templates/configmap-scripts.yaml`
- Create: `charts/featherbit-gateway/ci/template-only/etcd-values.yaml`

**Interfaces:**
- Consumes `featherbit-gateway.fullname`, `.Values.containerPorts`.
- Produces helpers `featherbit-gateway.systemConfig` (YAML string), `featherbit-gateway.gatewayConfig` (YAML string), `featherbit-gateway.mcpTokenEnv` (env var name for a token name), `featherbit-gateway.configMapName`, `featherbit-gateway.scriptsConfigMapName`. Produces values `logging.{level,format}`, `config.{source,system,gateway,gatewayRaw,etcd.{endpoints,prefix,timeoutMs,existingSecret},scripts}`, `admin.{username,uiEnabled,users}`, `mcp.{enabled,path,tokens,allowedOrigins,existingSecret}`, `tls.{existingSecret,adminInherit}`.
- Env var contract the system config relies on (Task 3 must provide them): `ADMIN_USER`, `ADMIN_PASSWORD`, `ADMIN_USER_<n>`/`ADMIN_PASSWORD_<n>` for `admin.users`, `FEATHERBIT_MCP_TOKEN_<NAME>` per MCP token, `ETCD_USER`/`ETCD_PASSWORD` when `config.etcd.existingSecret` is set.

- [ ] **Step 1: Append values**

```yaml
logging:
  level: info          # error | warn | info | debug | trace
  format: text         # text | json

admin:
  # Primary full-access account. Rendered into system.yaml as ${ADMIN_USER} /
  # ${ADMIN_PASSWORD} and supplied through the Secret (see secret.yaml).
  username: admin
  # Empty -> generated once on first install and kept across upgrades.
  password: ""
  # Use your own Secret (keys: username, password) instead of a chart-managed one.
  # Not compatible with admin.users (add those accounts to config.system.admin.users).
  existingSecret: ""
  # Extra full-access accounts: [{username: ops, password: s3cret}]
  users: []
  # Serve the embedded web UI on the admin port.
  uiEnabled: true

# Model Context Protocol endpoint for AI agents (admin.mcp in system.yaml).
mcp:
  enabled: false
  path: /mcp
  # [{name: local-agent, scope: read, value: <token>}]. `value` may be omitted
  # when existingSecret is set; the Secret key is then the token name.
  tokens: []
  existingSecret: ""
  allowedOrigins: []

# TLS for the data plane from a kubernetes.io/tls Secret, mounted at
# /etc/gateway/tls and wired into system.yaml tls.cert_path/key_path.
tls:
  existingSecret: ""
  # Reuse the data-plane certificate for the admin listener (admin.tls.inherit).
  adminInherit: false

config:
  # file: config from the ConfigMap below (Admin API edits live in memory).
  # etcd: config lives in etcd; gateway.yaml only seeds an empty prefix.
  source: file
  # Merged over the chart's default system.yaml. Any key the gateway accepts:
  # timeouts, http2, debug, cache, acme, tls (for ACME), stream, admin.tls, ...
  system: {}
  # The full gateway.yaml as a map: routes, policies, supernodes, stores, ...
  # ${ENV} placeholders are passed through to the gateway untouched.
  gateway:
    routes:
      - name: hello
        match:
          path: /hello
        policy: hello
      - name: api
        match:
          path: /api/*
        policy: api
    policies:
      - name: hello
        nodes:
          - id: listener
            type: listener
          - id: mock
            type: mocking
            config:
              response_example: '{"gateway": "featherbit", "path": "$uri"}'
          - id: client
            type: client
        edges:
          - from: listener.out
            to: mock.in
          - from: mock.success
            to: client.in
      - name: api
        error_handler: on-error
        nodes:
          - id: listener
            type: listener
          - id: strip-prefix
            type: proxy-rewrite
            config:
              phase: request
              strip_path_prefix: /api
          - id: backend
            type: upstream
            config:
              targets:
                - host: ${UPSTREAM_HOST:-upstream}
                  port: ${UPSTREAM_PORT:-80}
          - id: on-error
            type: error-handler
            config:
              status_code: 502
              body_template: '{"error": "{{error.code}}", "message": "{{error.message}}"}'
          - id: client
            type: client
        edges:
          - from: listener.out
            to: strip-prefix.in
          - from: strip-prefix.success
            to: backend.in
          - from: backend.success
            to: client.in
          - from: backend.error
            to: on-error.in
          - from: on-error.success
            to: client.in
  # Alternatively the literal gateway.yaml text; wins over `gateway` when set.
  gatewayRaw: ""
  etcd:
    endpoints: []        # e.g. ["http://etcd:2379"]
    prefix: /featherbit
    timeoutMs: 3000
    # Secret with keys `user` and `password` for etcd auth.
    existingSecret: ""
  # Lua scripts, mounted at /etc/gateway/plugins/<name>: {auth.lua: "..."}
  scripts: {}
```

- [ ] **Step 2: Append config helpers to _helpers.tpl**

```
{{- define "featherbit-gateway.configMapName" -}}
{{ include "featherbit-gateway.fullname" . }}-config
{{- end }}

{{- define "featherbit-gateway.scriptsConfigMapName" -}}
{{ include "featherbit-gateway.fullname" . }}-scripts
{{- end }}

{{/* Env var carrying an MCP token: FEATHERBIT_MCP_TOKEN_<NAME>, env-safe. */}}
{{- define "featherbit-gateway.mcpTokenEnv" -}}
FEATHERBIT_MCP_TOKEN_{{ . | upper | replace "-" "_" | replace "." "_" }}
{{- end }}

{{/*
system.yaml: a chart default (listener, logging, admin, optional mcp/etcd/tls
blocks) deep-merged with .Values.config.system, which wins on conflicts.
Credentials are ${ENV} placeholders resolved by the gateway at load.
*/}}
{{- define "featherbit-gateway.systemConfig" -}}
{{- $admin := dict
      "bind" "0.0.0.0"
      "port" (int .Values.containerPorts.admin)
      "username" "${ADMIN_USER}"
      "password" "${ADMIN_PASSWORD}"
      "ui_enabled" .Values.admin.uiEnabled -}}
{{- if .Values.admin.users }}
  {{- $users := list }}
  {{- range $i, $u := .Values.admin.users }}
    {{- $n := int (add1 $i) }}
    {{- $users = append $users (dict "username" (printf "${ADMIN_USER_%d}" $n) "password" (printf "${ADMIN_PASSWORD_%d}" $n)) }}
  {{- end }}
  {{- $_ := set $admin "users" $users }}
{{- end }}
{{- if .Values.mcp.enabled }}
  {{- $tokens := list }}
  {{- range .Values.mcp.tokens }}
    {{- $tokens = append $tokens (dict "token" (printf "${%s}" (include "featherbit-gateway.mcpTokenEnv" .name)) "scope" .scope "name" .name) }}
  {{- end }}
  {{- $_ := set $admin "mcp" (dict "enabled" true "path" .Values.mcp.path "tokens" $tokens "allowed_origins" .Values.mcp.allowedOrigins) }}
{{- end }}
{{- if and .Values.tls.existingSecret .Values.tls.adminInherit }}
  {{- $_ := set $admin "tls" (dict "inherit" true) }}
{{- end }}
{{- $base := dict
      "listener" (dict "bind" "0.0.0.0" "port" (int .Values.containerPorts.http))
      "logging" (dict "level" .Values.logging.level "format" .Values.logging.format)
      "admin" $admin -}}
{{- if eq .Values.config.source "etcd" }}
  {{- $etcd := dict "endpoints" .Values.config.etcd.endpoints "prefix" .Values.config.etcd.prefix "timeout_ms" (int .Values.config.etcd.timeoutMs) }}
  {{- if .Values.config.etcd.existingSecret }}
    {{- $_ := set $etcd "user" "${ETCD_USER}" }}
    {{- $_ := set $etcd "password" "${ETCD_PASSWORD}" }}
  {{- end }}
  {{- $_ := set $base "config" (dict "source" "etcd" "etcd" $etcd) }}
{{- end }}
{{- if .Values.tls.existingSecret }}
  {{- $_ := set $base "tls" (dict "cert_path" "/etc/gateway/tls/tls.crt" "key_path" "/etc/gateway/tls/tls.key") }}
{{- end }}
{{- toYaml (mergeOverwrite $base (deepCopy .Values.config.system)) -}}
{{- end }}

{{/* gateway.yaml: literal text when gatewayRaw is set, else the map. */}}
{{- define "featherbit-gateway.gatewayConfig" -}}
{{- if .Values.config.gatewayRaw -}}
{{ .Values.config.gatewayRaw }}
{{- else -}}
{{ toYaml .Values.config.gateway }}
{{- end -}}
{{- end }}
```

- [ ] **Step 3: Write configmap.yaml**

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "featherbit-gateway.configMapName" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
data:
  system.yaml: |
    {{- include "featherbit-gateway.systemConfig" . | nindent 4 }}
  # In etcd mode this file only seeds an empty prefix on first boot.
  gateway.yaml: |
    {{- include "featherbit-gateway.gatewayConfig" . | nindent 4 }}
```

- [ ] **Step 4: Write configmap-scripts.yaml**

```yaml
{{- if .Values.config.scripts }}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "featherbit-gateway.scriptsConfigMapName" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
data:
  {{- range $name, $body := .Values.config.scripts }}
  {{ $name }}: |
    {{- $body | nindent 4 }}
  {{- end }}
{{- end }}
```

- [ ] **Step 5: Render and check the default config**

```bash
C=charts/featherbit-gateway; R=$(mktemp -d)
helm template fb "$C" > "$R/default.yaml"
grep -c 'kind: ConfigMap' "$R/default.yaml"            # 1
grep -F '${ADMIN_USER}' "$R/default.yaml"               # placeholder survives (quoted or not)
grep -F 'port: 8080' "$R/default.yaml"                  # listener port is an int
grep -F '{{error.code}}' "$R/default.yaml"              # gateway template syntax survives
grep -F '${UPSTREAM_HOST:-upstream}' "$R/default.yaml"
grep -c 'ui_enabled: true' "$R/default.yaml"            # 1
```

Expected: every grep prints a match (counts as noted). If `{{error.code}}` is missing, the gateway config was passed through a template context; it must stay a values string.

- [ ] **Step 6: Check deep merge and etcd mode**

Create `charts/featherbit-gateway/ci/template-only/etcd-values.yaml`:

```yaml
# Template-only (lint + kubeconform): no etcd runs in CI.
config:
  source: etcd
  etcd:
    endpoints: ["http://etcd.default.svc:2379"]
    prefix: /featherbit
    existingSecret: etcd-auth
  system:
    timeouts:
      shutdown_timeout_seconds: 10
    logging:
      level: debug
```

```bash
helm template fb "$C" -f "$C/ci/template-only/etcd-values.yaml" > "$R/etcd.yaml"
grep -F 'source: etcd' "$R/etcd.yaml"
grep -F '${ETCD_USER}' "$R/etcd.yaml"
grep -F 'shutdown_timeout_seconds: 10' "$R/etcd.yaml"
grep -F 'level: debug' "$R/etcd.yaml"       # user override wins over chart default
grep -F 'format: text' "$R/etcd.yaml"       # sibling default preserved by deep merge
grep -c 'gateway.yaml: |' "$R/etcd.yaml"    # 1 (seed file still shipped)
```

Expected: all match.

- [ ] **Step 7: Check MCP, extra users and TLS wiring**

```bash
helm template fb "$C" \
  --set mcp.enabled=true --set 'mcp.tokens[0].name=local-agent' --set 'mcp.tokens[0].scope=read' --set 'mcp.tokens[0].value=abc' \
  --set 'admin.users[0].username=ops' --set 'admin.users[0].password=pw' \
  --set tls.existingSecret=gw-tls --set tls.adminInherit=true > "$R/opts.yaml"
grep -F '${FEATHERBIT_MCP_TOKEN_LOCAL_AGENT}' "$R/opts.yaml"
grep -F '${ADMIN_USER_1}' "$R/opts.yaml"
grep -F 'cert_path: /etc/gateway/tls/tls.crt' "$R/opts.yaml"
grep -F 'inherit: true' "$R/opts.yaml"
```

Expected: all match.

- [ ] **Step 8: Check scripts and gatewayRaw**

```bash
helm template fb "$C" --set-string 'config.scripts.auth\.lua=return 1' \
  --set-string 'config.gatewayRaw=routes: []' > "$R/scripts.yaml"
grep -c 'kind: ConfigMap' "$R/scripts.yaml"     # 2
grep -F 'auth.lua: |' "$R/scripts.yaml"
grep -A1 -F 'gateway.yaml: |' "$R/scripts.yaml" | grep -F 'routes: []'
```

Expected: 2 ConfigMaps, the script key present, the raw gateway text used.

- [ ] **Step 9: Lint and commit**

Run: `helm lint --strict "$C" && helm lint --strict "$C" -f "$C/ci/template-only/etcd-values.yaml"`
Expected: both pass.

```bash
git add charts/featherbit-gateway
git commit -m "feat(helm): render system.yaml and gateway.yaml into a ConfigMap"
```

---

### Task 3: Secret and credential helpers

**Files:**
- Modify: `charts/featherbit-gateway/templates/_helpers.tpl` (append)
- Create: `charts/featherbit-gateway/templates/secret.yaml`
- Modify: `charts/featherbit-gateway/values.yaml` (append `probes`)

**Interfaces:**
- Produces helpers `featherbit-gateway.secretName` (managed or existing), `featherbit-gateway.adminPassword` (explicit value, else the password already stored in the cluster via `lookup`, else empty), `featherbit-gateway.probeAuthHeader` (full `Basic …` header value or empty), `featherbit-gateway.env` (the container `env:` list). Produces values `probes.authHeader`.
- Secret key contract: `username`, `password`, `username-<n>`/`password-<n>`, `mcp-<name>`. An `admin.existingSecret` must provide `username` and `password`.

- [ ] **Step 1: Append values**

```yaml
probes:
  # /healthz and /readyz sit behind Basic Auth. HTTP probes are used when the
  # chart knows the password (admin.password set, or already stored from a
  # previous install); otherwise probes fall back to a TCP check on the admin
  # port. Set this to "Basic <base64 user:pass>" to force HTTP probes with
  # admin.existingSecret.
  authHeader: ""
  liveness:
    initialDelaySeconds: 5
    periodSeconds: 10
    timeoutSeconds: 2
    failureThreshold: 3
  readiness:
    initialDelaySeconds: 2
    periodSeconds: 5
    timeoutSeconds: 2
    failureThreshold: 3
  startup:
    periodSeconds: 2
    failureThreshold: 30
```

- [ ] **Step 2: Append helpers**

```
{{- define "featherbit-gateway.secretName" -}}
{{- if .Values.admin.existingSecret -}}
{{ .Values.admin.existingSecret }}
{{- else -}}
{{ include "featherbit-gateway.fullname" . }}-admin
{{- end -}}
{{- end }}

{{/*
The admin password the chart can know at render time: the explicit value, or
the one a previous install stored (lookup is empty under `helm template`).
Empty means "generate on first install" — see secret.yaml.
*/}}
{{- define "featherbit-gateway.adminPassword" -}}
{{- if .Values.admin.password -}}
{{ .Values.admin.password }}
{{- else if not .Values.admin.existingSecret -}}
{{- $existing := lookup "v1" "Secret" .Release.Namespace (include "featherbit-gateway.secretName" .) -}}
{{- if and $existing $existing.data (hasKey $existing.data "password") -}}
{{ index $existing.data "password" | b64dec }}
{{- end -}}
{{- end -}}
{{- end }}

{{/* "Basic <b64>" for the HTTP probes, or empty when the password is unknown. */}}
{{- define "featherbit-gateway.probeAuthHeader" -}}
{{- if .Values.probes.authHeader -}}
{{ .Values.probes.authHeader }}
{{- else -}}
{{- $pw := include "featherbit-gateway.adminPassword" . -}}
{{- if $pw -}}
Basic {{ printf "%s:%s" .Values.admin.username $pw | b64enc }}
{{- end -}}
{{- end -}}
{{- end }}

{{/* Container env: every ${ENV} the rendered system.yaml references. */}}
{{- define "featherbit-gateway.env" -}}
- name: ADMIN_USER
  valueFrom:
    secretKeyRef:
      name: {{ include "featherbit-gateway.secretName" . }}
      key: username
- name: ADMIN_PASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ include "featherbit-gateway.secretName" . }}
      key: password
{{- range $i, $u := .Values.admin.users }}
{{- $n := int (add1 $i) }}
- name: ADMIN_USER_{{ $n }}
  valueFrom:
    secretKeyRef:
      name: {{ include "featherbit-gateway.secretName" $ }}
      key: username-{{ $n }}
- name: ADMIN_PASSWORD_{{ $n }}
  valueFrom:
    secretKeyRef:
      name: {{ include "featherbit-gateway.secretName" $ }}
      key: password-{{ $n }}
{{- end }}
{{- if .Values.mcp.enabled }}
{{- range .Values.mcp.tokens }}
- name: {{ include "featherbit-gateway.mcpTokenEnv" .name }}
  valueFrom:
    secretKeyRef:
      {{- if $.Values.mcp.existingSecret }}
      name: {{ $.Values.mcp.existingSecret }}
      key: {{ .name }}
      {{- else }}
      name: {{ include "featherbit-gateway.secretName" $ }}
      key: mcp-{{ .name }}
      {{- end }}
{{- end }}
{{- end }}
{{- if and (eq .Values.config.source "etcd") .Values.config.etcd.existingSecret }}
- name: ETCD_USER
  valueFrom:
    secretKeyRef:
      name: {{ .Values.config.etcd.existingSecret }}
      key: user
- name: ETCD_PASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ .Values.config.etcd.existingSecret }}
      key: password
{{- end }}
{{- with .Values.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end }}
```

- [ ] **Step 3: Write secret.yaml**

```yaml
{{- if and .Values.admin.existingSecret .Values.admin.users }}
{{- fail "admin.users requires the chart-managed Secret; with admin.existingSecret, put extra accounts in that Secret and declare them under config.system.admin.users" }}
{{- end }}
{{- if and .Values.mcp.enabled (not .Values.mcp.existingSecret) }}
{{- range .Values.mcp.tokens }}
{{- if not .value }}
{{- fail (printf "mcp.tokens[%s].value is required unless mcp.existingSecret is set" .name) }}
{{- end }}
{{- end }}
{{- end }}
{{- if not .Values.admin.existingSecret }}
apiVersion: v1
kind: Secret
metadata:
  name: {{ include "featherbit-gateway.secretName" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
type: Opaque
data:
  username: {{ .Values.admin.username | b64enc }}
  # Explicit value, else the password a previous install stored, else a fresh one.
  password: {{ include "featherbit-gateway.adminPassword" . | default (randAlphaNum 24) | b64enc }}
  {{- range $i, $u := .Values.admin.users }}
  {{- $n := int (add1 $i) }}
  username-{{ $n }}: {{ required (printf "admin.users[%d].username is required" $i) $u.username | b64enc }}
  password-{{ $n }}: {{ required (printf "admin.users[%d].password is required" $i) $u.password | b64enc }}
  {{- end }}
  {{- if and .Values.mcp.enabled (not .Values.mcp.existingSecret) }}
  {{- range .Values.mcp.tokens }}
  mcp-{{ .name }}: {{ .value | b64enc }}
  {{- end }}
  {{- end }}
{{- end }}
```

- [ ] **Step 4: Render defaults**

```bash
C=charts/featherbit-gateway; R=$(mktemp -d)
helm template fb "$C" > "$R/default.yaml"
grep -c 'kind: Secret' "$R/default.yaml"               # 1
grep -F 'name: fb-featherbit-gateway-admin' "$R/default.yaml"
grep -E '^  password: [A-Za-z0-9+/=]{20,}$' "$R/default.yaml"   # random, base64
```

Expected: all match.

- [ ] **Step 5: Explicit password is used and stable**

```bash
helm template fb "$C" --set admin.password=hunter2 > "$R/pw.yaml"
grep -F "password: $(printf hunter2 | base64)" "$R/pw.yaml"
```

Expected: match.

- [ ] **Step 6: existingSecret suppresses the managed Secret**

```bash
helm template fb "$C" --set admin.existingSecret=my-admin > "$R/ext.yaml"
grep -c 'kind: Secret' "$R/ext.yaml"   # 0
```

Expected: `0`.

- [ ] **Step 7: existingSecret + users fails loudly (Review Focus 2)**

```bash
helm template fb "$C" --set admin.existingSecret=my-admin \
  --set 'admin.users[0].username=ops' --set 'admin.users[0].password=pw' 2>&1 | grep -F 'admin.users requires the chart-managed Secret'
```

Expected: the failure message is printed and helm exits non-zero.

- [ ] **Step 8: MCP token without value fails unless existingSecret**

```bash
helm template fb "$C" --set mcp.enabled=true --set 'mcp.tokens[0].name=agent' --set 'mcp.tokens[0].scope=read' 2>&1 | grep -F 'mcp.tokens[agent].value is required'
helm template fb "$C" --set mcp.enabled=true --set 'mcp.tokens[0].name=agent' --set 'mcp.tokens[0].scope=read' --set mcp.existingSecret=mcp-tokens > "$R/mcp.yaml"
grep -c 'kind: Secret' "$R/mcp.yaml"   # 1 (admin secret only, no mcp- key)
grep -c 'mcp-agent' "$R/mcp.yaml"      # 0
```

Expected: first command prints the message; second renders; counts 1 and 0.

- [ ] **Step 9: Lint and commit**

Run: `helm lint --strict "$C"`
Expected: pass.

```bash
git add charts/featherbit-gateway
git commit -m "feat(helm): manage admin, MCP and etcd credentials through a Secret"
```

---

### Task 4: Deployment and ServiceAccount

**Files:**
- Create: `charts/featherbit-gateway/templates/serviceaccount.yaml`
- Create: `charts/featherbit-gateway/templates/deployment.yaml`
- Modify: `charts/featherbit-gateway/values.yaml` (append workload values)
- Modify: `charts/featherbit-gateway/templates/_helpers.tpl` (append `serviceAccountName`)

**Interfaces:**
- Consumes `featherbit-gateway.{image,env,probeAuthHeader,systemConfig,configMapName,scriptsConfigMapName}`, `.Values.containerPorts`, `.Values.config.scripts`, `.Values.tls.existingSecret`.
- Produces container port names `http`, `admin`, and one named port per `stream.ports[]` entry (`.name`). Produces values `replicaCount`, `stream.ports`, `terminationGracePeriodSeconds`, `podAnnotations`, `podLabels`, `podSecurityContext`, `securityContext`, `resources`, `nodeSelector`, `tolerations`, `affinity`, `topologySpreadConstraints`, `priorityClassName`, `extraEnv`, `extraEnvFrom`, `extraVolumes`, `extraVolumeMounts`, `extraContainers`, `initContainers`, `serviceAccount.{create,name,annotations,automount}`, `autoscaling.enabled` (read here, defined fully in Task 7).

- [ ] **Step 1: Append values**

```yaml
replicaCount: 1

# L4 stream listeners declared in config.system.stream need container ports
# and a Service; list them here: [{name: mqtt, port: 1883, protocol: TCP}]
stream:
  ports: []
  service:
    type: ClusterIP
    annotations: {}

# Defaults to config.system.timeouts.shutdown_timeout_seconds (30) + 5 so the
# gateway's drain finishes before the kubelet kills the pod.
terminationGracePeriodSeconds: null

serviceAccount:
  create: true
  name: ""
  annotations: {}
  automount: false

podAnnotations: {}
podLabels: {}

# The image is FROM scratch and runs as the distroless "nonroot" uid.
podSecurityContext:
  runAsNonRoot: true
  runAsUser: 65532
  runAsGroup: 65532
  fsGroup: 65532
  seccompProfile:
    type: RuntimeDefault
securityContext:
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop: ["ALL"]

resources: {}
#  limits:
#    cpu: "1"
#    memory: 256Mi
#  requests:
#    cpu: 100m
#    memory: 64Mi

nodeSelector: {}
tolerations: []
affinity: {}
topologySpreadConstraints: []
priorityClassName: ""

# Raw additions to the gateway container / pod.
extraEnv: []            # e.g. [{name: UPSTREAM_HOST, value: my-api.default.svc}]
extraEnvFrom: []
extraVolumes: []
extraVolumeMounts: []
extraContainers: []
initContainers: []

autoscaling:
  enabled: false
```

- [ ] **Step 2: Append the serviceAccountName helper**

```
{{- define "featherbit-gateway.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "featherbit-gateway.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}
```

- [ ] **Step 3: Write serviceaccount.yaml**

```yaml
{{- if .Values.serviceAccount.create -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ include "featherbit-gateway.serviceAccountName" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
  {{- with .Values.serviceAccount.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
automountServiceAccountToken: {{ .Values.serviceAccount.automount }}
{{- end }}
```

- [ ] **Step 4: Write deployment.yaml**

```yaml
{{- $auth := include "featherbit-gateway.probeAuthHeader" . -}}
{{- $drain := dig "timeouts" "shutdown_timeout_seconds" 30 .Values.config.system -}}
{{- $grace := .Values.terminationGracePeriodSeconds -}}
{{- if not $grace }}{{ if kindIs "string" $drain }}{{ $grace = 35 }}{{ else }}{{ $grace = add $drain 5 }}{{ end }}{{ end -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
spec:
  {{- if not .Values.autoscaling.enabled }}
  replicas: {{ .Values.replicaCount }}
  {{- end }}
  selector:
    matchLabels:
      {{- include "featherbit-gateway.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      annotations:
        # system.yaml is not hot-reloaded by the gateway: a change rolls the pods.
        # gateway.yaml is, so it is deliberately left out of the checksum.
        checksum/system-config: {{ include "featherbit-gateway.systemConfig" . | sha256sum }}
        {{- with .Values.podAnnotations }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      labels:
        {{- include "featherbit-gateway.selectorLabels" . | nindent 8 }}
        {{- with .Values.podLabels }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
    spec:
      {{- with .Values.imagePullSecrets }}
      imagePullSecrets:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      serviceAccountName: {{ include "featherbit-gateway.serviceAccountName" . }}
      automountServiceAccountToken: {{ .Values.serviceAccount.automount }}
      terminationGracePeriodSeconds: {{ $grace }}
      {{- with .Values.priorityClassName }}
      priorityClassName: {{ . }}
      {{- end }}
      {{- with .Values.podSecurityContext }}
      securityContext:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.initContainers }}
      initContainers:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      containers:
        - name: gateway
          image: {{ include "featherbit-gateway.image" . | quote }}
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          {{- with .Values.securityContext }}
          securityContext:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          ports:
            - name: http
              containerPort: {{ .Values.containerPorts.http }}
              protocol: TCP
            - name: admin
              containerPort: {{ .Values.containerPorts.admin }}
              protocol: TCP
            {{- range .Values.stream.ports }}
            - name: {{ .name }}
              containerPort: {{ .port }}
              protocol: {{ default "TCP" .protocol }}
            {{- end }}
          env:
            {{- include "featherbit-gateway.env" . | nindent 12 }}
          {{- with .Values.extraEnvFrom }}
          envFrom:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          livenessProbe:
            {{- if $auth }}
            httpGet:
              path: /healthz
              port: admin
              httpHeaders:
                - name: Authorization
                  value: {{ $auth | quote }}
            {{- else }}
            tcpSocket:
              port: admin
            {{- end }}
            {{- toYaml .Values.probes.liveness | nindent 12 }}
          readinessProbe:
            {{- if $auth }}
            httpGet:
              path: /readyz
              port: admin
              httpHeaders:
                - name: Authorization
                  value: {{ $auth | quote }}
            {{- else }}
            tcpSocket:
              port: admin
            {{- end }}
            {{- toYaml .Values.probes.readiness | nindent 12 }}
          startupProbe:
            {{- if $auth }}
            httpGet:
              path: /readyz
              port: admin
              httpHeaders:
                - name: Authorization
                  value: {{ $auth | quote }}
            {{- else }}
            tcpSocket:
              port: admin
            {{- end }}
            {{- toYaml .Values.probes.startup | nindent 12 }}
          {{- with .Values.resources }}
          resources:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          volumeMounts:
            - name: config
              mountPath: /etc/gateway
              readOnly: true
            - name: data
              mountPath: /var/lib/featherbit
            {{- with .Values.extraVolumeMounts }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
        {{- with .Values.extraContainers }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      volumes:
        # One projected volume so config, scripts and TLS material all live
        # under /etc/gateway, the directory the gateway's watchers cover.
        - name: config
          projected:
            sources:
              - configMap:
                  name: {{ include "featherbit-gateway.configMapName" . }}
              {{- if .Values.config.scripts }}
              - configMap:
                  name: {{ include "featherbit-gateway.scriptsConfigMapName" . }}
                  items:
                    {{- range $name, $_ := .Values.config.scripts }}
                    - key: {{ $name }}
                      path: plugins/{{ $name }}
                    {{- end }}
              {{- end }}
              {{- if .Values.tls.existingSecret }}
              - secret:
                  name: {{ .Values.tls.existingSecret }}
                  items:
                    - key: tls.crt
                      path: tls/tls.crt
                    - key: tls.key
                      path: tls/tls.key
              {{- end }}
        - name: data
          emptyDir: {}
        {{- with .Values.extraVolumes }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- with .Values.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.topologySpreadConstraints }}
      topologySpreadConstraints:
        {{- toYaml . | nindent 8 }}
      {{- end }}
```

- [ ] **Step 5: Render defaults and check the security/probe shape**

```bash
C=charts/featherbit-gateway; R=$(mktemp -d)
helm template fb "$C" > "$R/default.yaml"
grep -F 'image: "featherbit/featherbit:0.14.0"' "$R/default.yaml"
grep -F 'runAsUser: 65532' "$R/default.yaml"
grep -F 'readOnlyRootFilesystem: true' "$R/default.yaml"
grep -F 'terminationGracePeriodSeconds: 35' "$R/default.yaml"
grep -c 'tcpSocket:' "$R/default.yaml"          # 3: password unknown at first render -> TCP probes
grep -F 'mountPath: /etc/gateway' "$R/default.yaml"
grep -F 'checksum/system-config:' "$R/default.yaml"
grep -F 'replicas: 1' "$R/default.yaml"
```

Expected: all match; tcpSocket count 3.

- [ ] **Step 6: Known password switches to HTTP probes; image variants; grace period**

```bash
helm template fb "$C" --set admin.password=hunter2 > "$R/pw.yaml"
grep -c 'path: /readyz' "$R/pw.yaml"             # 2 (readiness + startup)
grep -c 'path: /healthz' "$R/pw.yaml"            # 1
grep -F "value: \"Basic $(printf 'admin:hunter2' | base64)\"" "$R/pw.yaml"
helm template fb "$C" --set image.headless=true --set image.tag=edge | grep -F 'image: "featherbit/featherbit:edge-headless"'
helm template fb "$C" --set image.digest=sha256:0000000000000000000000000000000000000000000000000000000000000000 | grep -F 'featherbit/featherbit@sha256:0000'
helm template fb "$C" --set config.system.timeouts.shutdown_timeout_seconds=10 | grep -F 'terminationGracePeriodSeconds: 15'
helm template fb "$C" --set terminationGracePeriodSeconds=90 | grep -F 'terminationGracePeriodSeconds: 90'
```

Expected: all match with the counts shown.

- [ ] **Step 7: Scripts and TLS land in the projected volume (Review Focus 3)**

```bash
helm template fb "$C" --set-string 'config.scripts.auth\.lua=return 1' --set tls.existingSecret=gw-tls > "$R/vol.yaml"
grep -F 'path: plugins/auth.lua' "$R/vol.yaml"
grep -F 'path: tls/tls.key' "$R/vol.yaml"
grep -F 'name: gw-tls' "$R/vol.yaml"
helm template fb "$C" --set 'stream.ports[0].name=mqtt' --set 'stream.ports[0].port=1883' | grep -B1 -F 'containerPort: 1883' | grep -F 'name: mqtt'
```

Expected: all match.

- [ ] **Step 8: Lint and commit**

Run: `helm lint --strict "$C"`
Expected: pass.

```bash
git add charts/featherbit-gateway
git commit -m "feat(helm): add the gateway Deployment and ServiceAccount"
```

---

### Task 5: Services

**Files:**
- Create: `charts/featherbit-gateway/templates/service.yaml`
- Create: `charts/featherbit-gateway/templates/service-admin.yaml`
- Create: `charts/featherbit-gateway/templates/service-stream.yaml`
- Modify: `charts/featherbit-gateway/values.yaml` (append `service`, `adminService`)

**Interfaces:**
- Produces Services named `<fullname>` (data plane, port `http`), `<fullname>-admin` (port `admin`, label `app.kubernetes.io/component: admin`), `<fullname>-stream`. Later tasks (Ingress, HTTPRoute, ServiceMonitor, NOTES, helm test) reference these names and ports.
- Produces values `service.{type,port,nodePort,annotations,labels,externalTrafficPolicy,loadBalancerIP,loadBalancerSourceRanges,clusterIP}`, `adminService.{type,port,annotations,labels,clusterIP}`.

- [ ] **Step 1: Append values**

```yaml
# Data plane. For an edge deployment set type: LoadBalancer (or NodePort);
# behind an existing controller leave ClusterIP and enable ingress/httpRoute.
service:
  type: ClusterIP
  port: 80
  nodePort: null
  clusterIP: ""
  annotations: {}
  labels: {}
  externalTrafficPolicy: ""
  loadBalancerIP: ""
  loadBalancerSourceRanges: []

# Admin API + web UI. Deliberately never exposed by default: Basic Auth
# credentials travel on every request and it has full write access.
adminService:
  type: ClusterIP
  port: 9090
  clusterIP: ""
  annotations: {}
  labels: {}
```

- [ ] **Step 2: Write service.yaml**

```yaml
apiVersion: v1
kind: Service
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
    app.kubernetes.io/component: data-plane
    {{- with .Values.service.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- with .Values.service.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  type: {{ .Values.service.type }}
  {{- with .Values.service.clusterIP }}
  clusterIP: {{ . }}
  {{- end }}
  {{- with .Values.service.externalTrafficPolicy }}
  externalTrafficPolicy: {{ . }}
  {{- end }}
  {{- with .Values.service.loadBalancerIP }}
  loadBalancerIP: {{ . }}
  {{- end }}
  {{- with .Values.service.loadBalancerSourceRanges }}
  loadBalancerSourceRanges:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  ports:
    - name: http
      port: {{ .Values.service.port }}
      targetPort: http
      protocol: TCP
      {{- if and (eq .Values.service.type "NodePort") .Values.service.nodePort }}
      nodePort: {{ .Values.service.nodePort }}
      {{- end }}
  selector:
    {{- include "featherbit-gateway.selectorLabels" . | nindent 4 }}
```

- [ ] **Step 3: Write service-admin.yaml**

```yaml
apiVersion: v1
kind: Service
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}-admin
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
    app.kubernetes.io/component: admin
    {{- with .Values.adminService.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- with .Values.adminService.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  type: {{ .Values.adminService.type }}
  {{- with .Values.adminService.clusterIP }}
  clusterIP: {{ . }}
  {{- end }}
  ports:
    - name: admin
      port: {{ .Values.adminService.port }}
      targetPort: admin
      protocol: TCP
  selector:
    {{- include "featherbit-gateway.selectorLabels" . | nindent 4 }}
```

- [ ] **Step 4: Write service-stream.yaml**

```yaml
{{- if .Values.stream.ports }}
apiVersion: v1
kind: Service
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}-stream
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
    app.kubernetes.io/component: stream
  {{- with .Values.stream.service.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  type: {{ .Values.stream.service.type }}
  ports:
    {{- range .Values.stream.ports }}
    - name: {{ .name }}
      port: {{ .port }}
      targetPort: {{ .name }}
      protocol: {{ default "TCP" .protocol }}
    {{- end }}
  selector:
    {{- include "featherbit-gateway.selectorLabels" . | nindent 4 }}
{{- end }}
```

- [ ] **Step 5: Render and check**

```bash
C=charts/featherbit-gateway; R=$(mktemp -d)
helm template fb "$C" > "$R/default.yaml"
grep -c 'kind: Service$' "$R/default.yaml"                 # 2
grep -F 'name: fb-featherbit-gateway-admin' "$R/default.yaml"
grep -c 'type: ClusterIP' "$R/default.yaml"                # 2
helm template fb "$C" --set service.type=LoadBalancer --set service.externalTrafficPolicy=Local | grep -F 'externalTrafficPolicy: Local'
helm template fb "$C" --set 'stream.ports[0].name=mqtt' --set 'stream.ports[0].port=1883' --set 'stream.ports[0].protocol=UDP' > "$R/stream.yaml"
grep -c 'kind: Service$' "$R/stream.yaml"                  # 3
grep -F 'protocol: UDP' "$R/stream.yaml"
```

Expected: counts 2, 2, 3 and the greps match.

- [ ] **Step 6: Lint and commit**

Run: `helm lint --strict "$C"`

```bash
git add charts/featherbit-gateway
git commit -m "feat(helm): add data-plane, admin and stream Services"
```

---

### Task 6: Edge exposure — Ingress, admin Ingress, HTTPRoute

**Files:**
- Create: `charts/featherbit-gateway/templates/ingress.yaml`
- Create: `charts/featherbit-gateway/templates/ingress-admin.yaml`
- Create: `charts/featherbit-gateway/templates/httproute.yaml`
- Modify: `charts/featherbit-gateway/values.yaml` (append `ingress`, `adminIngress`, `httpRoute`)

**Interfaces:**
- Consumes Service names/ports from Task 5.
- Produces values `ingress.{enabled,className,annotations,hosts[].{host,paths[].{path,pathType}},tls}`, `adminIngress` (same shape), `httpRoute.{enabled,annotations,parentRefs,hostnames,rules}`.

- [ ] **Step 1: Append values**

```yaml
# Expose the data plane through an existing Ingress controller.
ingress:
  enabled: false
  className: ""
  annotations: {}
  hosts:
    - host: gateway.example.com
      paths:
        - path: /
          pathType: Prefix
  tls: []
  #  - secretName: gateway-tls
  #    hosts: [gateway.example.com]

# Expose the admin API/UI. Off by default on purpose; put it behind your own
# network policy / auth proxy if you turn it on.
adminIngress:
  enabled: false
  className: ""
  annotations: {}
  hosts:
    - host: gateway-admin.example.com
      paths:
        - path: /
          pathType: Prefix
  tls: []

# Expose the data plane through a Gateway API implementation (HTTPRoute).
# featherbit does not implement Gateway API itself; this attaches it as a
# backend of a Gateway you already run.
httpRoute:
  enabled: false
  annotations: {}
  parentRefs: []
  #  - name: my-gateway
  #    namespace: gateway-system
  #    sectionName: https
  hostnames: []
  # Defaults to one PathPrefix / rule to the data-plane Service when empty.
  rules: []
```

- [ ] **Step 2: Write ingress.yaml**

```yaml
{{- if .Values.ingress.enabled -}}
{{- $svcName := include "featherbit-gateway.fullname" . -}}
{{- $svcPort := .Values.service.port -}}
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ $svcName }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
  {{- with .Values.ingress.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  {{- with .Values.ingress.className }}
  ingressClassName: {{ . }}
  {{- end }}
  {{- with .Values.ingress.tls }}
  tls:
    {{- range . }}
    - hosts:
        {{- range .hosts }}
        - {{ . | quote }}
        {{- end }}
      secretName: {{ .secretName }}
    {{- end }}
  {{- end }}
  rules:
    {{- range .Values.ingress.hosts }}
    - host: {{ .host | quote }}
      http:
        paths:
          {{- range .paths }}
          - path: {{ .path }}
            pathType: {{ .pathType }}
            backend:
              service:
                name: {{ $svcName }}
                port:
                  number: {{ $svcPort }}
          {{- end }}
    {{- end }}
{{- end }}
```

- [ ] **Step 3: Write ingress-admin.yaml** — identical structure, driven by `.Values.adminIngress`, name `{{ $svcName }}-admin`, backend service `{{ $svcName }}-admin` port `.Values.adminService.port`:

```yaml
{{- if .Values.adminIngress.enabled -}}
{{- $svcName := printf "%s-admin" (include "featherbit-gateway.fullname" .) -}}
{{- $svcPort := .Values.adminService.port -}}
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ $svcName }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
    app.kubernetes.io/component: admin
  {{- with .Values.adminIngress.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  {{- with .Values.adminIngress.className }}
  ingressClassName: {{ . }}
  {{- end }}
  {{- with .Values.adminIngress.tls }}
  tls:
    {{- range . }}
    - hosts:
        {{- range .hosts }}
        - {{ . | quote }}
        {{- end }}
      secretName: {{ .secretName }}
    {{- end }}
  {{- end }}
  rules:
    {{- range .Values.adminIngress.hosts }}
    - host: {{ .host | quote }}
      http:
        paths:
          {{- range .paths }}
          - path: {{ .path }}
            pathType: {{ .pathType }}
            backend:
              service:
                name: {{ $svcName }}
                port:
                  number: {{ $svcPort }}
          {{- end }}
    {{- end }}
{{- end }}
```

- [ ] **Step 4: Write httproute.yaml**

```yaml
{{- if .Values.httpRoute.enabled -}}
{{- if not .Values.httpRoute.parentRefs }}
{{- fail "httpRoute.enabled requires at least one entry in httpRoute.parentRefs" }}
{{- end }}
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
  {{- with .Values.httpRoute.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  parentRefs:
    {{- toYaml .Values.httpRoute.parentRefs | nindent 4 }}
  {{- with .Values.httpRoute.hostnames }}
  hostnames:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  rules:
    {{- if .Values.httpRoute.rules }}
    {{- toYaml .Values.httpRoute.rules | nindent 4 }}
    {{- else }}
    - matches:
        - path:
            type: PathPrefix
            value: /
      backendRefs:
        - name: {{ include "featherbit-gateway.fullname" . }}
          port: {{ .Values.service.port }}
    {{- end }}
{{- end }}
```

- [ ] **Step 5: Render and check**

```bash
C=charts/featherbit-gateway; R=$(mktemp -d)
helm template fb "$C" > "$R/default.yaml"
grep -c 'kind: Ingress' "$R/default.yaml"      # 0
grep -c 'kind: HTTPRoute' "$R/default.yaml"    # 0
helm template fb "$C" --set ingress.enabled=true --set ingress.className=nginx > "$R/ing.yaml"
grep -c 'kind: Ingress' "$R/ing.yaml"          # 1
grep -F 'ingressClassName: nginx' "$R/ing.yaml"
grep -F 'number: 80' "$R/ing.yaml"
helm template fb "$C" --set adminIngress.enabled=true | grep -F 'name: fb-featherbit-gateway-admin' | head -1
helm template fb "$C" --set httpRoute.enabled=true 2>&1 | grep -F 'requires at least one entry in httpRoute.parentRefs'
helm template fb "$C" --set httpRoute.enabled=true --set 'httpRoute.parentRefs[0].name=edge' > "$R/hr.yaml"
grep -F 'kind: HTTPRoute' "$R/hr.yaml"
grep -F 'type: PathPrefix' "$R/hr.yaml"
grep -F 'name: fb-featherbit-gateway' "$R/hr.yaml"
```

Expected: counts and matches as noted.

- [ ] **Step 6: Lint and commit**

Run: `helm lint --strict "$C" --set ingress.enabled=true --set httpRoute.enabled=true --set 'httpRoute.parentRefs[0].name=edge'`

```bash
git add charts/featherbit-gateway
git commit -m "feat(helm): optional Ingress, admin Ingress and Gateway API HTTPRoute"
```

---

### Task 7: HPA, PodDisruptionBudget, ServiceMonitor

**Files:**
- Create: `charts/featherbit-gateway/templates/hpa.yaml`
- Create: `charts/featherbit-gateway/templates/pdb.yaml`
- Create: `charts/featherbit-gateway/templates/servicemonitor.yaml`
- Modify: `charts/featherbit-gateway/values.yaml` (replace the `autoscaling` stub, append `podDisruptionBudget`, `serviceMonitor`)

**Interfaces:**
- Consumes Secret key names `username`/`password` (Task 3), admin Service label `app.kubernetes.io/component: admin` and port name `admin` (Task 5).
- Produces values `autoscaling.{enabled,minReplicas,maxReplicas,targetCPUUtilizationPercentage,targetMemoryUtilizationPercentage,behavior}`, `podDisruptionBudget.{enabled,minAvailable,maxUnavailable}`, `serviceMonitor.{enabled,namespace,interval,scrapeTimeout,labels,relabelings,metricRelabelings}`.

- [ ] **Step 1: Replace the `autoscaling` stub and append values**

```yaml
autoscaling:
  enabled: false
  minReplicas: 2
  maxReplicas: 10
  targetCPUUtilizationPercentage: 75
  targetMemoryUtilizationPercentage: null
  behavior: {}

podDisruptionBudget:
  enabled: false
  minAvailable: 1
  maxUnavailable: null

# Prometheus Operator ServiceMonitor scraping /metrics on the admin port with
# the admin credentials (Secret keys username/password).
serviceMonitor:
  enabled: false
  namespace: ""
  interval: 30s
  scrapeTimeout: 10s
  labels: {}
  relabelings: []
  metricRelabelings: []
```

- [ ] **Step 2: Write hpa.yaml**

```yaml
{{- if .Values.autoscaling.enabled }}
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: {{ include "featherbit-gateway.fullname" . }}
  minReplicas: {{ .Values.autoscaling.minReplicas }}
  maxReplicas: {{ .Values.autoscaling.maxReplicas }}
  metrics:
    {{- with .Values.autoscaling.targetCPUUtilizationPercentage }}
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: {{ . }}
    {{- end }}
    {{- with .Values.autoscaling.targetMemoryUtilizationPercentage }}
    - type: Resource
      resource:
        name: memory
        target:
          type: Utilization
          averageUtilization: {{ . }}
    {{- end }}
  {{- with .Values.autoscaling.behavior }}
  behavior:
    {{- toYaml . | nindent 4 }}
  {{- end }}
{{- end }}
```

- [ ] **Step 3: Write pdb.yaml**

```yaml
{{- if .Values.podDisruptionBudget.enabled }}
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
spec:
  {{- if .Values.podDisruptionBudget.maxUnavailable }}
  maxUnavailable: {{ .Values.podDisruptionBudget.maxUnavailable }}
  {{- else }}
  minAvailable: {{ .Values.podDisruptionBudget.minAvailable }}
  {{- end }}
  selector:
    matchLabels:
      {{- include "featherbit-gateway.selectorLabels" . | nindent 6 }}
{{- end }}
```

- [ ] **Step 4: Write servicemonitor.yaml, then check the HPA/replicas interaction (Review Focus 4)**

```yaml
{{- if .Values.serviceMonitor.enabled }}
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}
  {{- with .Values.serviceMonitor.namespace }}
  namespace: {{ . }}
  {{- end }}
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
    {{- with .Values.serviceMonitor.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
spec:
  namespaceSelector:
    matchNames:
      - {{ .Release.Namespace }}
  selector:
    matchLabels:
      {{- include "featherbit-gateway.selectorLabels" . | nindent 6 }}
      app.kubernetes.io/component: admin
  endpoints:
    - port: admin
      path: /metrics
      interval: {{ .Values.serviceMonitor.interval }}
      scrapeTimeout: {{ .Values.serviceMonitor.scrapeTimeout }}
      basicAuth:
        username:
          name: {{ include "featherbit-gateway.secretName" . }}
          key: username
        password:
          name: {{ include "featherbit-gateway.secretName" . }}
          key: password
      {{- with .Values.serviceMonitor.relabelings }}
      relabelings:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.serviceMonitor.metricRelabelings }}
      metricRelabelings:
        {{- toYaml . | nindent 8 }}
      {{- end }}
{{- end }}
```

```bash
C=charts/featherbit-gateway; R=$(mktemp -d)
helm template fb "$C" --set autoscaling.enabled=true > "$R/hpa.yaml"
grep -c 'kind: HorizontalPodAutoscaler' "$R/hpa.yaml"   # 1
grep -c '^  replicas:' "$R/hpa.yaml"                     # 0  <- Deployment must not pin replicas
helm template fb "$C" --set podDisruptionBudget.enabled=true | grep -F 'minAvailable: 1'
helm template fb "$C" --set podDisruptionBudget.enabled=true --set podDisruptionBudget.maxUnavailable=1 | grep -F 'maxUnavailable: 1'
helm template fb "$C" --set serviceMonitor.enabled=true > "$R/sm.yaml"
grep -F 'kind: ServiceMonitor' "$R/sm.yaml"
grep -F 'app.kubernetes.io/component: admin' "$R/sm.yaml"
grep -B1 -F 'key: password' "$R/sm.yaml" | grep -F 'name: fb-featherbit-gateway-admin'
```

Expected: counts 1 and 0; all greps match.

- [ ] **Step 5: Lint and commit**

Run: `helm lint --strict "$C" --set autoscaling.enabled=true --set podDisruptionBudget.enabled=true --set serviceMonitor.enabled=true`

```bash
git add charts/featherbit-gateway
git commit -m "feat(helm): optional HPA, PodDisruptionBudget and ServiceMonitor"
```

---

### Task 8: NOTES, helm test, values schema, CI value files, README

**Files:**
- Create: `charts/featherbit-gateway/templates/NOTES.txt`
- Create: `charts/featherbit-gateway/templates/tests/test-connection.yaml`
- Create: `charts/featherbit-gateway/values.schema.json`
- Create: `charts/featherbit-gateway/ci/default-values.yaml`
- Create: `charts/featherbit-gateway/ci/ingress-values.yaml`
- Create: `charts/featherbit-gateway/ci/template-only/all-options-values.yaml`
- Create: `charts/featherbit-gateway/README.md`
- Modify: `charts/featherbit-gateway/values.yaml` (append `tests`)

**Interfaces:**
- Produces values `tests.{enabled,image,dataPlanePath,expect}`.
- `ci/*-values.yaml` (top level only) are what `ct install` runs; `ci/template-only/*` are lint/kubeconform-only (ct ignores subdirectories).

- [ ] **Step 1: Append values**

```yaml
# `helm test`: checks /healthz with the admin credentials, then sends one
# request through the data plane. Set dataPlanePath to "" to skip the second
# check when you replace the default gateway config.
tests:
  enabled: true
  image: curlimages/curl:8.11.1
  dataPlanePath: /hello
  expect: featherbit
```

- [ ] **Step 2: Write NOTES.txt**

```
featherbit {{ .Chart.AppVersion }} is being deployed as release {{ .Release.Name }}.

Admin API + web UI (not exposed by default):

  kubectl -n {{ .Release.Namespace }} port-forward svc/{{ include "featherbit-gateway.fullname" . }}-admin {{ .Values.adminService.port }}:{{ .Values.adminService.port }}
  open http://localhost:{{ .Values.adminService.port }}

  username: {{ .Values.admin.username }}
{{- if .Values.admin.existingSecret }}
  password: from Secret {{ .Values.admin.existingSecret }} (key: password)
{{- else }}
  password: kubectl -n {{ .Release.Namespace }} get secret {{ include "featherbit-gateway.secretName" . }} -o jsonpath='{.data.password}' | base64 -d
{{- end }}

Data plane:
{{- if .Values.ingress.enabled }}
{{- range .Values.ingress.hosts }}
  http{{ if $.Values.ingress.tls }}s{{ end }}://{{ .host }}{{ (index .paths 0).path }}
{{- end }}
{{- else if .Values.httpRoute.enabled }}
  attached via HTTPRoute {{ include "featherbit-gateway.fullname" . }} to {{ range .Values.httpRoute.parentRefs }}{{ .name }} {{ end }}
{{- else if eq .Values.service.type "LoadBalancer" }}
  kubectl -n {{ .Release.Namespace }} get svc {{ include "featherbit-gateway.fullname" . }} -w   # wait for EXTERNAL-IP
{{- else if eq .Values.service.type "NodePort" }}
  kubectl -n {{ .Release.Namespace }} get svc {{ include "featherbit-gateway.fullname" . }}      # see the NodePort
{{- else }}
  kubectl -n {{ .Release.Namespace }} port-forward svc/{{ include "featherbit-gateway.fullname" . }} 8080:{{ .Values.service.port }}
  curl http://localhost:8080{{ .Values.tests.dataPlanePath | default "/" }}
{{- end }}

{{- if not .Values.admin.password }}
{{- if not .Values.admin.existingSecret }}

Note: health probes use a TCP check until the admin password is known to the
chart. Run `helm upgrade` once (same values) to switch them to HTTP /readyz,
or set admin.password / probes.authHeader.
{{- end }}
{{- end }}

Docs: https://featherbitplatform.github.io/gateway/guides/deployment#kubernetes-helm
```

- [ ] **Step 3: Write tests/test-connection.yaml**

```yaml
{{- if .Values.tests.enabled }}
apiVersion: v1
kind: Pod
metadata:
  name: {{ include "featherbit-gateway.fullname" . }}-test
  labels:
    {{- include "featherbit-gateway.labels" . | nindent 4 }}
  annotations:
    helm.sh/hook: test
    helm.sh/hook-delete-policy: before-hook-creation,hook-succeeded
spec:
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 65532
    seccompProfile:
      type: RuntimeDefault
  containers:
    - name: curl
      image: {{ .Values.tests.image }}
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities:
          drop: ["ALL"]
      env:
        - name: ADMIN_USER
          valueFrom:
            secretKeyRef:
              name: {{ include "featherbit-gateway.secretName" . }}
              key: username
        - name: ADMIN_PASSWORD
          valueFrom:
            secretKeyRef:
              name: {{ include "featherbit-gateway.secretName" . }}
              key: password
      command: ["sh", "-ec"]
      args:
        - |
          curl -fsS -u "$ADMIN_USER:$ADMIN_PASSWORD" \
            "http://{{ include "featherbit-gateway.fullname" . }}-admin:{{ .Values.adminService.port }}/healthz"
          {{- if .Values.tests.dataPlanePath }}
          curl -fsS "http://{{ include "featherbit-gateway.fullname" . }}:{{ .Values.service.port }}{{ .Values.tests.dataPlanePath }}" \
            | grep -q {{ .Values.tests.expect | quote }}
          {{- end }}
{{- end }}
```

- [ ] **Step 4: Write values.schema.json**

```json
{
  "$schema": "https://json-schema.org/draft-07/schema#",
  "title": "featherbit-gateway values",
  "type": "object",
  "properties": {
    "replicaCount": { "type": "integer", "minimum": 0 },
    "image": {
      "type": "object",
      "properties": {
        "repository": { "type": "string", "minLength": 1 },
        "tag": { "type": "string" },
        "digest": { "type": "string", "pattern": "^$|^sha256:[a-f0-9]{64}$" },
        "headless": { "type": "boolean" },
        "pullPolicy": { "type": "string", "enum": ["Always", "IfNotPresent", "Never"] }
      },
      "required": ["repository"]
    },
    "containerPorts": {
      "type": "object",
      "properties": {
        "http": { "type": "integer", "minimum": 1, "maximum": 65535 },
        "admin": { "type": "integer", "minimum": 1, "maximum": 65535 }
      },
      "required": ["http", "admin"]
    },
    "logging": {
      "type": "object",
      "properties": {
        "level": { "type": "string", "enum": ["error", "warn", "info", "debug", "trace"] },
        "format": { "type": "string", "enum": ["text", "json"] }
      }
    },
    "admin": {
      "type": "object",
      "properties": {
        "username": { "type": "string", "minLength": 1 },
        "password": { "type": "string" },
        "existingSecret": { "type": "string" },
        "uiEnabled": { "type": "boolean" },
        "users": {
          "type": "array",
          "items": {
            "type": "object",
            "properties": {
              "username": { "type": "string", "minLength": 1 },
              "password": { "type": "string", "minLength": 1 }
            },
            "required": ["username", "password"]
          }
        }
      },
      "required": ["username"]
    },
    "mcp": {
      "type": "object",
      "properties": {
        "enabled": { "type": "boolean" },
        "path": { "type": "string", "pattern": "^/" },
        "existingSecret": { "type": "string" },
        "allowedOrigins": { "type": "array", "items": { "type": "string" } },
        "tokens": {
          "type": "array",
          "items": {
            "type": "object",
            "properties": {
              "name": { "type": "string", "pattern": "^[A-Za-z0-9._-]+$" },
              "scope": { "type": "string", "enum": ["read", "write"] },
              "value": { "type": "string" }
            },
            "required": ["name", "scope"]
          }
        }
      }
    },
    "tls": {
      "type": "object",
      "properties": {
        "existingSecret": { "type": "string" },
        "adminInherit": { "type": "boolean" }
      }
    },
    "config": {
      "type": "object",
      "properties": {
        "source": { "type": "string", "enum": ["file", "etcd"] },
        "system": { "type": "object" },
        "gateway": { "type": "object" },
        "gatewayRaw": { "type": "string" },
        "etcd": {
          "type": "object",
          "properties": {
            "endpoints": { "type": "array", "items": { "type": "string" } },
            "prefix": { "type": "string" },
            "timeoutMs": { "type": "integer", "minimum": 1 },
            "existingSecret": { "type": "string" }
          }
        },
        "scripts": { "type": "object", "additionalProperties": { "type": "string" } }
      },
      "required": ["source"]
    },
    "probes": {
      "type": "object",
      "properties": { "authHeader": { "type": "string" } }
    },
    "stream": {
      "type": "object",
      "properties": {
        "ports": {
          "type": "array",
          "items": {
            "type": "object",
            "properties": {
              "name": { "type": "string", "pattern": "^[a-z0-9-]{1,15}$" },
              "port": { "type": "integer", "minimum": 1, "maximum": 65535 },
              "protocol": { "type": "string", "enum": ["TCP", "UDP"] }
            },
            "required": ["name", "port"]
          }
        }
      }
    },
    "service": {
      "type": "object",
      "properties": {
        "type": { "type": "string", "enum": ["ClusterIP", "NodePort", "LoadBalancer"] },
        "port": { "type": "integer", "minimum": 1, "maximum": 65535 }
      }
    },
    "adminService": {
      "type": "object",
      "properties": {
        "type": { "type": "string", "enum": ["ClusterIP", "NodePort", "LoadBalancer"] },
        "port": { "type": "integer", "minimum": 1, "maximum": 65535 }
      }
    },
    "ingress": { "type": "object", "properties": { "enabled": { "type": "boolean" } } },
    "adminIngress": { "type": "object", "properties": { "enabled": { "type": "boolean" } } },
    "httpRoute": {
      "type": "object",
      "properties": {
        "enabled": { "type": "boolean" },
        "parentRefs": { "type": "array" },
        "hostnames": { "type": "array", "items": { "type": "string" } },
        "rules": { "type": "array" }
      }
    },
    "autoscaling": {
      "type": "object",
      "properties": {
        "enabled": { "type": "boolean" },
        "minReplicas": { "type": "integer", "minimum": 1 },
        "maxReplicas": { "type": "integer", "minimum": 1 }
      }
    },
    "podDisruptionBudget": { "type": "object", "properties": { "enabled": { "type": "boolean" } } },
    "serviceMonitor": { "type": "object", "properties": { "enabled": { "type": "boolean" } } },
    "tests": {
      "type": "object",
      "properties": {
        "enabled": { "type": "boolean" },
        "image": { "type": "string" },
        "dataPlanePath": { "type": "string" },
        "expect": { "type": "string" }
      }
    }
  }
}
```

- [ ] **Step 5: Write the CI value files**

`ci/default-values.yaml`:

```yaml
# Exercised by `ct install` in CI (image.tag is overridden to a published tag
# on the ct command line). Default values; the file exists so ct runs the
# baseline install.
replicaCount: 1
```

`ci/ingress-values.yaml`:

```yaml
# `ct install`: Ingress object creation (no controller runs in kind, so the
# object is only created, not routed).
ingress:
  enabled: true
  hosts:
    - host: gateway.ci.local
      paths:
        - path: /
          pathType: Prefix
admin:
  password: ci-password
  users:
    - username: ops
      password: ci-ops-password
podDisruptionBudget:
  enabled: true
  minAvailable: 0
```

`ci/template-only/all-options-values.yaml`:

```yaml
# Lint + kubeconform only: every optional resource on at once.
replicaCount: 2
image:
  headless: true
admin:
  password: all-options
  users:
    - username: ops
      password: ops-password
mcp:
  enabled: true
  tokens:
    - name: local-agent
      scope: read
      value: read-token
    - name: ci.writer
      scope: write
      value: write-token
tls:
  existingSecret: gateway-tls
  adminInherit: true
config:
  system:
    timeouts:
      shutdown_timeout_seconds: 20
    http2:
      enabled: true
    stream:
      - protocol: tcp
        bind: 0.0.0.0
        port: 1883
        upstream:
          targets:
            - host: mqtt.default.svc
              port: 1883
  scripts:
    hello.lua: |
      return function(ctx) return ctx end
stream:
  ports:
    - name: mqtt
      port: 1883
      protocol: TCP
  service:
    type: LoadBalancer
service:
  type: LoadBalancer
  externalTrafficPolicy: Local
ingress:
  enabled: true
  className: nginx
  tls:
    - secretName: gateway-tls
      hosts: [gateway.example.com]
adminIngress:
  enabled: true
httpRoute:
  enabled: true
  parentRefs:
    - name: edge
      namespace: gateway-system
  hostnames: [gateway.example.com]
autoscaling:
  enabled: true
podDisruptionBudget:
  enabled: true
serviceMonitor:
  enabled: true
resources:
  requests:
    cpu: 100m
    memory: 64Mi
  limits:
    cpu: "1"
    memory: 256Mi
extraEnv:
  - name: UPSTREAM_HOST
    value: my-api.default.svc
```

- [ ] **Step 6: Lint every values file and kubeconform the renders**

```bash
C=charts/featherbit-gateway; R=$(mktemp -d)
for f in "$C"/ci/*-values.yaml "$C"/ci/template-only/*-values.yaml; do
  helm lint --strict "$C" -f "$f" || exit 1
done
helm template fb "$C" > "$R/default.yaml"
helm template fb "$C" -f "$C/ci/template-only/all-options-values.yaml" > "$R/all.yaml"
# HTTPRoute and ServiceMonitor are CRDs: give kubeconform their schemas.
SCHEMAS='-schema-location default -schema-location https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json'
docker run --rm -i ghcr.io/yannh/kubeconform:v0.6.7 -strict -summary $SCHEMAS < "$R/default.yaml"
docker run --rm -i ghcr.io/yannh/kubeconform:v0.6.7 -strict -summary $SCHEMAS < "$R/all.yaml"
grep -c 'helm.sh/hook: test' "$R/default.yaml"   # 1
```

Expected: every lint passes, kubeconform reports 0 invalid and 0 errors for both, test hook present. A values-schema violation (e.g. `--set service.type=External`) must fail: `helm template fb "$C" --set service.type=External 2>&1 | grep -F 'service.type'`.

- [ ] **Step 7: Write README.md**

```markdown
# featherbit-gateway

Helm chart for [featherbit](https://featherbitplatform.github.io/gateway/), a single-binary API gateway with node-graph routing policies.

## Install

```bash
helm install featherbit oci://ghcr.io/featherbitplatform/charts/featherbit-gateway --version <chart version>
# or, the same chart from Docker Hub
helm install featherbit oci://registry-1.docker.io/featherbit/featherbit-gateway --version <chart version>
```

The chart version equals the gateway version it installs. A bare install serves a `/hello` mock route and an `/api/*` route to `${UPSTREAM_HOST}`; replace `config.gateway` with your own routes and policies.

```bash
kubectl port-forward svc/featherbit-featherbit-gateway-admin 9090:9090   # admin UI, user admin
kubectl get secret featherbit-featherbit-gateway-admin -o jsonpath='{.data.password}' | base64 -d
```

## Exposure

| Setup | Values |
|---|---|
| featherbit *is* the edge | `service.type=LoadBalancer` (plus `tls.existingSecret` or ACME under `config.system`) |
| behind an Ingress controller | `ingress.enabled=true`, `ingress.hosts[0].host=…` |
| behind a Gateway API Gateway | `httpRoute.enabled=true`, `httpRoute.parentRefs[0].name=…` |

The admin Service is `ClusterIP` and has no Ingress unless `adminIngress.enabled=true`. It carries Basic Auth credentials on every request and has full write access to the configuration; keep it internal.

## Configuration

- `config.system` is deep-merged over the chart's default `system.yaml`. Any gateway key works: `timeouts`, `http2`, `debug`, `cache`, `acme`, `stream`, `admin.tls`.
- `config.gateway` is the full `gateway.yaml` as a map (or `config.gatewayRaw` as text). `${ENV}` placeholders pass through to the gateway and resolve from the pod environment (`extraEnv`, `extraEnvFrom`).
- `config.scripts` mounts Lua files under `/etc/gateway/plugins`.
- Changing `system.yaml` rolls the pods (checksum annotation). Changing `gateway.yaml` or a script hot-reloads in place once the kubelet refreshes the ConfigMap mount.
- `config.source=etcd` with `config.etcd.endpoints` moves routes/policies into etcd (shared by every replica, Admin API edits persist). The chart does not run etcd for you.

## Credentials

The chart creates a Secret with `username`/`password`; an empty `admin.password` is generated once and kept across upgrades. `admin.existingSecret` (keys `username`, `password`) replaces it. MCP tokens go in the same Secret (`mcp.tokens[].value`) or in `mcp.existingSecret` keyed by token name.

Health probes hit `/healthz` and `/readyz`, which are behind Basic Auth. The chart sends the header when it knows the password (explicit `admin.password`, or a previously stored one on `helm upgrade`); otherwise probes fall back to a TCP check. Set `probes.authHeader` to force HTTP probes with an existing Secret.

## TLS and ACME

`tls.existingSecret` mounts a `kubernetes.io/tls` Secret and wires `tls.cert_path`/`key_path`; rotations are picked up by the gateway's cert watcher. ACME (TLS-ALPN-01) is configured under `config.system.acme` and `config.system.tls.acme`; the gateway must be reachable on 443 directly (a `LoadBalancer` Service, not an Ingress), and multiple replicas need the redis storage backend.

## Multiple replicas

The gateway is stateless. For cluster-accurate rate limits, sessions and ACME storage, declare redis `stores` in `config.gateway` and use etcd mode so config converges. `autoscaling` and `podDisruptionBudget` are available; `terminationGracePeriodSeconds` defaults to the gateway's drain timeout plus five seconds.

## Values

See the commented [`values.yaml`](./values.yaml) for the full reference; `values.schema.json` validates types and enums at install time.
```

(Fence nesting: the outer file is Markdown, write the inner code fences as regular triple backticks in the file.)

- [ ] **Step 8: Commit**

```bash
git add charts/featherbit-gateway
git commit -m "feat(helm): notes, helm test hook, values schema, CI value files and README"
```

---

### Task 9: Local kind install test

**Files:** none committed (verification only). Fix anything it uncovers in the chart files and commit with `fix(helm): …`.

**Interfaces:** consumes the whole chart. Uses the published `featherbit/featherbit:edge` image (tip of develop); `0.14.0` also works.

- [ ] **Step 1: Create a cluster**

Run: `kind create cluster --name fb-chart && kubectl cluster-info --context kind-fb-chart`
Expected: cluster up.

- [ ] **Step 2: Install with the default CI values**

```bash
C=charts/featherbit-gateway
helm install fb "$C" -f "$C/ci/default-values.yaml" --set image.tag=edge --wait --timeout 3m
kubectl get pods -l app.kubernetes.io/instance=fb
```

Expected: one pod `Running`, `1/1` ready. If the pod never becomes ready, `kubectl logs deploy/fb-featherbit-gateway` and `kubectl describe pod` — the usual culprits are a rendered `system.yaml` the gateway rejects (check the ConfigMap with `kubectl get cm fb-featherbit-gateway-config -o yaml`) or the read-only root filesystem.

- [ ] **Step 3: Run the chart test**

Run: `helm test fb`
Expected: `Phase: Succeeded`. The hook curls `/healthz` with credentials and `/hello` through the data-plane Service.

- [ ] **Step 4: Confirm hot reload of gateway.yaml without a restart**

```bash
helm upgrade fb "$C" -f "$C/ci/default-values.yaml" --set image.tag=edge --set tests.expect=changed \
  --set 'config.gateway.policies[0].nodes[1].config.response_example={"gateway": "changed"}' --wait
sleep 90   # kubelet ConfigMap sync (up to ~60s) + the gateway's debounce
kubectl get pod -l app.kubernetes.io/instance=fb -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}{"\n"}'   # 0
helm test fb
```

Expected: restart count `0`, test succeeds against the new body — the pod was not rolled (the checksum covers system.yaml only) and the gateway reloaded the file.

- [ ] **Step 5: Confirm a system.yaml change rolls the pod**

```bash
BEFORE=$(kubectl get pod -l app.kubernetes.io/instance=fb -o jsonpath='{.items[0].metadata.name}')
helm upgrade fb "$C" -f "$C/ci/default-values.yaml" --set image.tag=edge --set logging.level=debug --wait
AFTER=$(kubectl get pod -l app.kubernetes.io/instance=fb -o jsonpath='{.items[0].metadata.name}')
[ "$BEFORE" != "$AFTER" ] && echo rolled
```

Expected: `rolled`.

- [ ] **Step 6: Random password survives upgrade and probes switch to HTTP (Review Focus 5)**

```bash
P1=$(kubectl get secret fb-featherbit-gateway-admin -o jsonpath='{.data.password}')
helm upgrade fb "$C" -f "$C/ci/default-values.yaml" --set image.tag=edge --wait
P2=$(kubectl get secret fb-featherbit-gateway-admin -o jsonpath='{.data.password}')
[ "$P1" = "$P2" ] && echo stable
kubectl get deploy fb-featherbit-gateway -o jsonpath='{.spec.template.spec.containers[0].readinessProbe.httpGet.path}{"\n"}'   # /readyz
kubectl get pods -l app.kubernetes.io/instance=fb   # still 1/1 Ready with HTTP probes
```

Expected: `stable`, `/readyz`, pod ready.

- [ ] **Step 7: Ingress values install and the second ct file**

```bash
helm uninstall fb --wait
helm install fb "$C" -f "$C/ci/ingress-values.yaml" --set image.tag=edge --wait --timeout 3m
helm test fb
kubectl get ingress,pdb -l app.kubernetes.io/instance=fb
helm uninstall fb --wait
```

Expected: install + test succeed; one Ingress and one PDB listed.

- [ ] **Step 8: Tear down**

Run: `kind delete cluster --name fb-chart`

- [ ] **Step 9: Commit any fixes**

```bash
git add charts/featherbit-gateway
git commit -m "fix(helm): <what the kind run uncovered>"   # only if something changed
```

---

### Task 10: Chart/crate version drift test

**Files:**
- Modify: `src/admin/status.rs` (append to the existing `#[cfg(test)] mod tests`)

**Interfaces:** reads `charts/featherbit-gateway/Chart.yaml` relative to `CARGO_MANIFEST_DIR`.

- [ ] **Step 1: Write the failing test**

Append inside the existing tests module in `src/admin/status.rs`:

```rust
    /// The Helm chart is versioned in lockstep with the crate: Chart.yaml's
    /// `version` and `appVersion` must equal CARGO_PKG_VERSION, and the
    /// release chore commit bumps all three (helm.yml refuses to publish a
    /// chart whose version differs from the tag).
    #[test]
    fn helm_chart_version_tracks_the_crate_version() {
        let chart = include_str!("../../charts/featherbit-gateway/Chart.yaml");
        let version = env!("CARGO_PKG_VERSION");
        let has = |line: &str| chart.lines().any(|l| l.trim_end() == line);
        assert!(
            has(&format!("version: {version}")),
            "charts/featherbit-gateway/Chart.yaml `version` must be {version}"
        );
        assert!(
            has(&format!("appVersion: \"{version}\"")),
            "charts/featherbit-gateway/Chart.yaml `appVersion` must be \"{version}\""
        );
    }
```

- [ ] **Step 2: Prove it detects drift**

Temporarily edit `Chart.yaml` to `version: 0.0.0`, then:

Run: `cargo test helm_chart_version_tracks_the_crate_version -- --exact`
Expected: FAIL with the `version` must be 0.14.0 message. Restore `version: 0.14.0`.

- [ ] **Step 3: Run it green**

Run: `cargo test helm_chart_version_tracks_the_crate_version -- --exact`
Expected: `test result: ok. 1 passed`.

- [ ] **Step 4: Commit**

```bash
git add src/admin/status.rs
git commit -m "test: pin the helm chart version to the crate version"
```

---

### Task 11: CI workflow, Artifact Hub metadata, local SAST target

**Files:**
- Create: `.github/workflows/helm.yml`
- Create: `.github/ct-lintconf.yaml`
- Create: `charts/featherbit-gateway/artifacthub-repo.yml`
- Modify: `dev/sast.ps1` (new `helm` target, included in `all`)
- Modify: `dev/sast.sh` (same)
- Modify: `trivy.yaml` (comment only: it now also drives `trivy config charts/`)

**Interfaces:** consumes the existing secrets `DOCKER_USERNAME`/`DOCKER_PASSWORD` and `GITHUB_TOKEN`; the GHCR namespace is `ghcr.io/<owner lowercased>/charts`.

- [ ] **Step 1: Write .github/ct-lintconf.yaml** (chart-testing's upstream yamllint config with `line-length` disabled so commented values files are not rejected)

```yaml
---
rules:
  braces:
    min-spaces-inside: 0
    max-spaces-inside: 0
    min-spaces-inside-empty: -1
    max-spaces-inside-empty: -1
  brackets:
    min-spaces-inside: 0
    max-spaces-inside: 0
    min-spaces-inside-empty: -1
    max-spaces-inside-empty: -1
  colons:
    max-spaces-before: 0
    max-spaces-after: 1
  commas:
    max-spaces-before: 0
    min-spaces-after: 1
    max-spaces-after: 1
  comments:
    require-starting-space: true
    min-spaces-from-content: 1
  document-end: disable
  document-start: disable
  empty-lines:
    max: 2
    max-start: 0
    max-end: 0
  hyphens:
    max-spaces-after: 1
  indentation:
    spaces: consistent
    indent-sequences: whatever
    check-multi-line-strings: false
  key-duplicates: enable
  line-length: disable
  new-line-at-end-of-file: enable
  new-lines:
    type: unix
  trailing-spaces: enable
  truthy:
    level: warning
```

- [ ] **Step 2: Write .github/workflows/helm.yml**

```yaml
# Helm chart: lint + kubeconform + trivy config on every change, a kind
# install on PRs/branches, and an OCI publish to GHCR + Docker Hub on
# vX.Y.Z tags (the chart version must equal the tag — it tracks Cargo.toml).
name: helm

on:
  push:
    branches: [main, develop]
    paths: ['charts/**', '.github/workflows/helm.yml', '.github/ct-lintconf.yaml']
    tags: ['v*.*.*']
  pull_request:
    paths: ['charts/**', '.github/workflows/helm.yml', '.github/ct-lintconf.yaml']
  workflow_dispatch:

concurrency:
  group: helm-${{ github.ref }}
  cancel-in-progress: true

permissions:
  contents: read

env:
  CHART_DIR: charts/featherbit-gateway
  KUBECONFORM_VERSION: v0.6.7
  GATEWAY_API_VERSION: v1.2.1

jobs:
  lint:
    name: lint + schema + trivy
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      security-events: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0   # ct compares against the target branch

      - uses: azure/setup-helm@v4

      - name: helm lint (every values file)
        run: |
          helm lint --strict "$CHART_DIR"
          for f in "$CHART_DIR"/ci/*-values.yaml "$CHART_DIR"/ci/template-only/*-values.yaml; do
            echo "== $f"
            helm lint --strict "$CHART_DIR" -f "$f"
          done

      - uses: helm/chart-testing-action@v2

      # Version increments are made by the release chore commit, not per PR.
      - name: ct lint
        run: |
          ct lint --charts "$CHART_DIR" \
            --lint-conf .github/ct-lintconf.yaml \
            --check-version-increment=false \
            --validate-maintainers=false

      - name: Install kubeconform
        run: |
          curl -fsSL "https://github.com/yannh/kubeconform/releases/download/${KUBECONFORM_VERSION}/kubeconform-linux-amd64.tar.gz" \
            | sudo tar -xz -C /usr/local/bin kubeconform

      # CRD schemas (HTTPRoute, ServiceMonitor) come from the CRDs catalog.
      - name: kubeconform
        run: |
          schemas='-schema-location default -schema-location https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json'
          helm template fb "$CHART_DIR" | kubeconform -strict -summary $schemas
          for f in "$CHART_DIR"/ci/*-values.yaml "$CHART_DIR"/ci/template-only/*-values.yaml; do
            echo "== $f"
            helm template fb "$CHART_DIR" -f "$f" | kubeconform -strict -summary $schemas
          done

      - uses: aquasecurity/trivy-action@v0.36.0
        with:
          scan-type: config
          scan-ref: ${{ env.CHART_DIR }}
          trivy-config: trivy.yaml
          exit-code: '1'
          format: sarif
          output: trivy-helm.sarif

      - uses: github/codeql-action/upload-sarif@v3
        if: always()
        with:
          sarif_file: trivy-helm.sarif
          category: trivy-helm

  install:
    name: kind install
    if: ${{ !startsWith(github.ref, 'refs/tags/') }}
    runs-on: ubuntu-24.04
    needs: lint
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: azure/setup-helm@v4
      - uses: helm/chart-testing-action@v2
      - uses: helm/kind-action@v1

      # HTTPRoute CRDs so the all-options render would apply; no controller.
      - name: Install Gateway API CRDs
        run: kubectl apply -f "https://github.com/kubernetes-sigs/gateway-api/releases/download/${GATEWAY_API_VERSION}/standard-install.yaml"

      # develop/PRs test against the edge image; main against latest.
      - name: Pick image tag
        run: |
          if [ "${GITHUB_REF}" = "refs/heads/main" ]; then echo "IMAGE_TAG=latest" >> "$GITHUB_ENV"; else echo "IMAGE_TAG=edge" >> "$GITHUB_ENV"; fi

      - name: ct install
        run: |
          ct install --charts "$CHART_DIR" \
            --helm-extra-set-args "--set image.tag=${IMAGE_TAG}" \
            --helm-extra-args "--timeout 3m"

  publish:
    name: publish (GHCR + Docker Hub)
    if: startsWith(github.ref, 'refs/tags/v')
    runs-on: ubuntu-24.04
    needs: lint
    permissions:
      contents: write
      packages: write
    steps:
      - uses: actions/checkout@v4

      - uses: azure/setup-helm@v4

      - name: Chart version must equal the tag
        env:
          TAG: ${{ github.ref_name }}
        run: |
          version="${TAG#v}"
          chart_version=$(sed -n 's/^version: *//p' "$CHART_DIR/Chart.yaml")
          app_version=$(sed -n 's/^appVersion: *"\(.*\)"/\1/p' "$CHART_DIR/Chart.yaml")
          if [ "$chart_version" != "$version" ] || [ "$app_version" != "$version" ]; then
            echo "::error::Chart.yaml version=$chart_version appVersion=$app_version, tag is $version — bump the chart in the release commit."
            exit 1
          fi
          echo "VERSION=$version" >> "$GITHUB_ENV"

      - name: helm package
        run: helm package "$CHART_DIR" --destination dist

      - name: Push to GHCR
        env:
          OWNER: ${{ github.repository_owner }}
        run: |
          echo "${{ secrets.GITHUB_TOKEN }}" | helm registry login ghcr.io -u "${{ github.actor }}" --password-stdin
          helm push "dist/featherbit-gateway-${VERSION}.tgz" "oci://ghcr.io/${OWNER,,}/charts"

      - name: Push to Docker Hub
        env:
          DOCKER_USERNAME: ${{ secrets.DOCKER_USERNAME }}
        run: |
          echo "${{ secrets.DOCKER_PASSWORD }}" | helm registry login registry-1.docker.io -u "$DOCKER_USERNAME" --password-stdin
          helm push "dist/featherbit-gateway-${VERSION}.tgz" "oci://registry-1.docker.io/${DOCKER_USERNAME}"

      # Same release the SBOMs attach to (docker.yml); created if missing.
      - uses: softprops/action-gh-release@v2
        with:
          files: dist/featherbit-gateway-*.tgz
```

- [ ] **Step 3: Write artifacthub-repo.yml**

```yaml
# Artifact Hub repository metadata (registration is a one-time step in the
# Artifact Hub UI pointing at oci://ghcr.io/featherbitplatform/charts/featherbit-gateway).
repositoryID: ""
owners:
  - name: featherbit
```

- [ ] **Step 4: Add the `helm` target to dev/sast.ps1**

In the tool image list add:

```powershell
$HelmImage     = 'alpine/helm:3.16.3'
```

Change the `all` expansion to include it:

```powershell
if ($Targets -contains 'all') {
    $Targets = @('semgrep', 'deny', 'grype', 'hadolint', 'gitleaks', 'npm-audit', 'helm', 'sbom')
}
```

Add a case before `default` (mirror the `hadolint` block's style):

```powershell
        'helm' {
            # Same two checks as the lint job in .github/workflows/helm.yml:
            # strict helm lint over every values file, then trivy's
            # misconfiguration rules over the rendered chart.
            Invoke-Scan 'helm lint' {
                docker run --rm -v "${RepoRoot}:/src:ro" -w /src --entrypoint sh $HelmImage -c @'
set -e
helm lint --strict charts/featherbit-gateway
for f in charts/featherbit-gateway/ci/*-values.yaml charts/featherbit-gateway/ci/template-only/*-values.yaml; do
  echo "== $f"; helm lint --strict charts/featherbit-gateway -f "$f"
done
'@
            }
            Invoke-Scan 'trivy (helm config)' {
                docker run --rm -v "${RepoRoot}:/src:ro" `
                    -v featherbit-trivy-cache:/root/.cache/trivy `
                    $TrivyImage config --config /src/trivy.yaml --exit-code 1 /src/charts
            }
        }
```

Update the `default` branch's target list string and the `.SYNOPSIS` targets line to include `helm`.

- [ ] **Step 5: Add the same target to dev/sast.sh**

Add `HELM_IMAGE='alpine/helm:3.16.3'` next to the other images, `helm` to the `all` list, and a case:

```bash
    helm)
        run_scan "helm lint" docker run --rm -v "$REPO_ROOT:/src:ro" -w /src --entrypoint sh "$HELM_IMAGE" -c '
set -e
helm lint --strict charts/featherbit-gateway
for f in charts/featherbit-gateway/ci/*-values.yaml charts/featherbit-gateway/ci/template-only/*-values.yaml; do
  echo "== $f"; helm lint --strict charts/featherbit-gateway -f "$f"
done'
        run_scan "trivy (helm config)" docker run --rm -v "$REPO_ROOT:/src:ro" \
            -v featherbit-trivy-cache:/root/.cache/trivy \
            "$TRIVY_IMAGE" config --config /src/trivy.yaml --exit-code 1 /src/charts
        ;;
```

Update the usage text's target list.

- [ ] **Step 6: Update the trivy.yaml header comment**

Replace the first comment line with:

```yaml
# trivy configuration — container image scanning (OS packages + embedded deps)
# and Helm chart misconfiguration scanning (`trivy config charts/`).
```

- [ ] **Step 7: Run the local target and fix findings**

Run: `pwsh ./dev/sast.ps1 helm`
Expected: `PASS helm lint` and `PASS trivy (helm config)`. If trivy reports HIGH/CRITICAL misconfigurations in the rendered chart, fix the template (typical: a missing `seccompProfile` on the test pod, `automountServiceAccountToken`); do not suppress with `.trivyignore` unless the finding is wrong for a `FROM scratch` image, in which case add the check ID with a comment explaining why.

- [ ] **Step 8: Validate the workflow YAML**

Run: `docker run --rm -v "$PWD:/src:ro" -w /src rhysd/actionlint:latest .github/workflows/helm.yml`
Expected: no output (clean). Fix any shellcheck warnings it raises.

- [ ] **Step 9: Commit**

```bash
git add .github/workflows/helm.yml .github/ct-lintconf.yaml charts/featherbit-gateway/artifacthub-repo.yml dev/sast.ps1 dev/sast.sh trivy.yaml
git commit -m "ci(helm): lint, kind-install and publish the chart to GHCR and Docker Hub"
```

---

### Task 12: Documentation

**Files:**
- Modify: `website/docs/guides/deployment.md` (new section after "Container image", revise "Stateless multi-instance deployment")
- Modify: `website/docs/reference/roadmap.md` (new row)
- Modify: `README.md` (line after the Docker Hub paragraph)
- Modify: `DOCKERHUB.md` (new "Kubernetes" subsection after "Quick start")
- Modify: `CLAUDE.md` (build commands + a line in the project description)

- [ ] **Step 1: Add the Kubernetes section to deployment.md**

Insert after the "Container image: static binary, `FROM scratch`" section (before "## Graceful shutdown"):

```markdown
## Kubernetes (Helm)

The chart `featherbit-gateway` is published as an OCI artifact on every release, to GHCR and to Docker Hub (same chart, pick either). Its version equals the gateway version it installs.

```bash
helm install featherbit oci://ghcr.io/featherbitplatform/charts/featherbit-gateway --version 0.14.0
# or
helm install featherbit oci://registry-1.docker.io/featherbit/featherbit-gateway --version 0.14.0
```

A bare install runs one replica with a `/hello` mock route and an `/api/*` route to `${UPSTREAM_HOST}`, the admin API and UI on an internal Service, and a generated admin password:

```bash
kubectl port-forward svc/featherbit-featherbit-gateway-admin 9090:9090
kubectl get secret featherbit-featherbit-gateway-admin -o jsonpath='{.data.password}' | base64 -d
```

### Exposing the data plane

featherbit is itself a gateway, so pick one of two shapes:

- **featherbit is the edge** — expose the data-plane Service directly and let featherbit terminate TLS (a mounted Secret or ACME):

  ```yaml
  service:
    type: LoadBalancer
  tls:
    existingSecret: gateway-tls        # kubernetes.io/tls
  ```

- **Behind an existing controller** — keep the Service internal and attach it to an Ingress or a Gateway API `Gateway` (featherbit does not implement Gateway API; it attaches as a backend):

  ```yaml
  ingress:
    enabled: true
    className: nginx
    hosts:
      - host: api.example.com
        paths: [{ path: /, pathType: Prefix }]
  # or
  httpRoute:
    enabled: true
    parentRefs: [{ name: edge, namespace: gateway-system }]
    hostnames: [api.example.com]
  ```

The admin Service is `ClusterIP` and gets no Ingress unless `adminIngress.enabled: true`. It carries Basic Auth credentials on every request and has full write access, so keep it internal or put your own auth proxy in front.

### Your configuration

The chart renders the gateway's own files into a ConfigMap. `config.system` is deep-merged over a chart default `system.yaml`; `config.gateway` is the full `gateway.yaml` as a map (`config.gatewayRaw` takes the literal text). `${ENV}` placeholders are passed through untouched and resolve from the pod environment:

```yaml
config:
  system:
    timeouts:
      shutdown_timeout_seconds: 20
  gateway:
    routes:
      - name: users
        match: { path: /users/* }
        policy: users
    policies:
      - name: users
        nodes:
          - { id: listener, type: listener }
          - id: backend
            type: upstream
            config:
              targets: [{ host: ${USERS_HOST}, port: 8080 }]
          - { id: client, type: client }
        edges:
          - { from: listener.out, to: backend.in }
          - { from: backend.success, to: client.in }
  scripts:
    audit.lua: |
      return function(ctx) return ctx end
extraEnv:
  - { name: USERS_HOST, value: users.default.svc }
```

Changing `system.yaml` rolls the pods (it is not hot-reloaded). Changing `gateway.yaml` or a script hot-reloads in place once the kubelet refreshes the mount, within about a minute.

### Secrets

Admin credentials live in a chart-managed Secret (an empty `admin.password` is generated once and kept across upgrades) or in `admin.existingSecret` with keys `username` and `password`. MCP tokens and etcd credentials follow the same pattern (`mcp.existingSecret`, `config.etcd.existingSecret`). Health probes use `/healthz` and `/readyz`, which sit behind Basic Auth: the chart sends the header when it knows the password and falls back to a TCP check otherwise (`probes.authHeader` forces HTTP probes with an existing Secret).

### Multiple replicas

Scale with `replicaCount` or `autoscaling`, and add `podDisruptionBudget` for voluntary disruptions. For a coordinated cluster, set `config.source: etcd` with `config.etcd.endpoints` (see [HA clustering with etcd](#ha-clustering-with-etcd)); the chart does not run etcd. Cluster-accurate rate limits, sessions and ACME storage need redis `stores` declared in `config.gateway`.

ACME works through `config.system.acme` and `config.system.tls.acme`; TLS-ALPN-01 requires the gateway itself to answer on port 443, so use a `LoadBalancer` Service rather than an Ingress, and the redis storage backend when running more than one replica.

Every value is documented in the chart's [`values.yaml`](https://github.com/featherbitplatform/gateway/blob/main/charts/featherbit-gateway/values.yaml).
```

Then in "Stateless multi-instance deployment", replace the sentence beginning "This fits simple deployments" with:

```markdown
This fits simple deployments, Docker Compose, single-node setups, and Kubernetes — where the [Helm chart](#kubernetes-helm) mounts both files from a ConfigMap and the kubelet's refresh triggers the same hot-reload.
```

Also update the front-matter `description` to: `Docker Compose development setup, the scratch container image, the Helm chart, and multi-instance deployment.`

- [ ] **Step 2: Add the roadmap row**

Append after the "Graceful shutdown" row:

```markdown
| Kubernetes | **Helm chart implemented** — `featherbit-gateway`, published as an OCI artifact to `oci://ghcr.io/featherbitplatform/charts` and `oci://registry-1.docker.io/featherbit` on every release, versioned in lockstep with the gateway: Deployment with the `FROM scratch` security profile, split data-plane/admin Services, config rendered from the gateway's own `system.yaml`/`gateway.yaml`, optional Ingress / Gateway API `HTTPRoute` / HPA / PDB / ServiceMonitor, lint + kind-install tested in CI (see [Deployment → Kubernetes (Helm)](../guides/deployment.md#kubernetes-helm)). Planned, in a separate repository: a Kubernetes **operator** (CRDs for routes/policies with the gateway's own validation in an admission webhook, Gateway API implementation). |
```

- [ ] **Step 3: README.md**

After the "Container images are on Docker Hub…" paragraph add:

```markdown
A Helm chart is published alongside: `helm install featherbit oci://ghcr.io/featherbitplatform/charts/featherbit-gateway --version <release>` (also on Docker Hub as `oci://registry-1.docker.io/featherbit/featherbit-gateway`). Source in [`charts/featherbit-gateway/`](charts/featherbit-gateway/), published by `.github/workflows/helm.yml`.
```

- [ ] **Step 4: DOCKERHUB.md**

After the "Quick start" compose example add:

```markdown
## Kubernetes

The Helm chart is published to this registry too (as an OCI artifact, not an image — `docker pull` will not work on it):

```console
helm install featherbit oci://registry-1.docker.io/featherbit/featherbit-gateway --version <release>
```

Chart versions equal gateway versions. Values and the deployment guide: https://featherbitplatform.github.io/gateway/guides/deployment#kubernetes-helm
```

- [ ] **Step 5: CLAUDE.md**

Under "Build Commands", after the SAST block, add:

```markdown
Helm chart (`charts/featherbit-gateway/`; version + appVersion track `Cargo.toml` and are bumped
in the release commit — `src/admin/status.rs` has a test for it; published by `.github/workflows/helm.yml`):
```bash
helm lint --strict charts/featherbit-gateway
./dev/sast.ps1 helm           # strict lint over every ci/ values file + trivy config scan
kind create cluster && helm install fb charts/featherbit-gateway --set image.tag=edge --wait && helm test fb
```
```

And in "What This Project Is", append a bullet:

```markdown
- **Helm chart** — `charts/featherbit-gateway`, OCI-published to GHCR and Docker Hub on release tags; the gateway's `system.yaml`/`gateway.yaml` rendered from values into a ConfigMap, credentials via Secret + `${ENV}` interpolation, split data-plane/admin Services, optional Ingress/HTTPRoute/HPA/PDB/ServiceMonitor. A Kubernetes operator is planned as a separate repository.
```

- [ ] **Step 6: Build the docs site**

Run: `cd website && npm run build`
Expected: build succeeds; no broken-anchor warnings for `#kubernetes-helm`. (Docusaurus v3 admonition syntax is `:::type[Title]` if you add one.)

- [ ] **Step 7: Commit**

```bash
git add website/docs/guides/deployment.md website/docs/reference/roadmap.md README.md DOCKERHUB.md CLAUDE.md
git commit -m "docs(helm): document the Kubernetes Helm chart"
```

---

### Task 13: Final verification

**Files:** none.

- [ ] **Step 1: Full chart matrix once more**

```bash
C=charts/featherbit-gateway
for f in "$C"/ci/*-values.yaml "$C"/ci/template-only/*-values.yaml; do helm lint --strict "$C" -f "$f" || exit 1; done
pwsh ./dev/sast.ps1 helm gitleaks
```

Expected: all PASS. gitleaks must not flag the CI passwords (`ci-password`, `all-options`); if it does, add `charts/featherbit-gateway/ci/` to the inline-fixtures allowlist in `.gitleaks.toml` with a comment.

- [ ] **Step 2: Rust suite (the drift test lives in it)**

Run: `cargo test`
Expected: all green, including `helm_chart_version_tracks_the_crate_version`.

- [ ] **Step 3: Knowledge graph**

Run: `graphify update .`

- [ ] **Step 4: Report**

Summarize to the user: chart location, what the kind run proved, the CI jobs, the two install commands, and the two manual one-offs that remain (make the GHCR package public after the first publish; register on Artifact Hub). Wait for their go-ahead before pushing or opening the PR against `develop`.
