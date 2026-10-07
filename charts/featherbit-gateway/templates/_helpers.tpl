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
