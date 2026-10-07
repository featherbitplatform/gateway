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
