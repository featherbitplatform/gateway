/**
 * Config schema per plugin type — the declarative field list consumed by
 * SchemaForm so a node's configuration is edited through real controls
 * (text, number, radio, select, switch, and repeatable list/object rows)
 * instead of a raw JSON blob. Mirrors the featherbit design system's
 * pluginConfig. `listener`/`client` take no config.
 *
 * @module pluginConfig
 */

/**
 * Control kinds SchemaForm can render for a config field.
 *
 * `list` is a repeatable row of scalar items; `objects` is a repeatable
 * row of sub-fields that serializes as an array of records (e.g.
 * `add_headers` → `[{name, value}]`), which the Rust plugins accept
 * alongside the plain map form.
 */
export type FieldType =
  | 'text'
  | 'number'
  | 'radio'
  | 'select'
  | 'switch'
  | 'textarea'
  | 'list'
  | 'objects';

/**
 * A radio/select option whose stored value differs from its display label.
 */
export interface FieldOption {
  /** Value written into the node config. */
  value: string;
  /** Text shown in the control. */
  label: string;
}

/**
 * Declarative description of a single config field rendered by SchemaForm.
 */
export interface FieldSchema {
  /** Property name in the node's `config` object. */
  key: string;
  /** Label shown next to the control. */
  label: string;
  /** Which control to render. */
  type: FieldType;
  /** Choices for `radio`/`select`; plain strings double as value and label. */
  options?: (string | FieldOption)[];
  /** Initial value when the field is unset in the node config. */
  default?: unknown;
  /** Placeholder text for text-like inputs. */
  placeholder?: string;
  /** Short helper text rendered under the control. */
  hint?: string;
  /** Visible rows for `textarea` fields. */
  rows?: number;
  /** Inline label rendered beside a `switch` control. */
  switchLabel?: string;
  /** Label for the "add item" button on `list`/`objects` fields. */
  addLabel?: string;
  /** Singular noun used to title each `objects` row. */
  itemLabel?: string;
  /** Scalar item control for `list` fields. */
  item?: { type: 'text' | 'number'; placeholder?: string };
  /** Sub-fields of each record for `objects` fields. */
  fields?: FieldSchema[];
}

/**
 * Form schema registry keyed by plugin type name.
 *
 * Field keys and defaults track the config each Rust plugin deserializes.
 * `objects` fields serialize as an array of records (e.g. `add_headers` →
 * `[{name, value}]`), a form the Rust plugins accept alongside the map form.
 *
 * @remarks
 * Plugin types and their config parsing live under src/plugins/ (factory in
 * src/plugins/mod.rs); keys here must match the serde field names there.
 */
export const pluginConfig: Record<string, FieldSchema[]> = {
  'proxy-rewrite': [
    { key: 'phase', label: 'Phase', type: 'radio', options: ['request', 'response'], default: 'request' },
    { key: 'strip_path_prefix', label: 'Strip path prefix', type: 'text', placeholder: '/api', hint: 'removed from the matched path' },
    { key: 'add_headers', label: 'Add headers', type: 'objects', addLabel: 'Header', itemLabel: 'Header',
      fields: [
        { key: 'name', label: 'Name', type: 'text', placeholder: 'x-request-id' },
        { key: 'value', label: 'Value', type: 'text', placeholder: '$uuid' },
      ] },
    { key: 'remove_headers', label: 'Remove headers', type: 'list', addLabel: 'Header', item: { type: 'text', placeholder: 'x-powered-by' } },
  ],
  upstream: [
    { key: 'targets', label: 'Targets', type: 'objects', addLabel: 'Target', itemLabel: 'Target',
      fields: [
        { key: 'host', label: 'Host', type: 'text', placeholder: 'localhost' },
        { key: 'port', label: 'Port', type: 'number', default: 3000 },
      ] },
    { key: 'load_balancing', label: 'Load balancing', type: 'select', options: ['round_robin', 'least_connections', 'ip_hash'], default: 'round_robin' },
    { key: 'timeout_ms', label: 'Timeout (ms)', type: 'number', default: 60000, hint: 'whole-call deadline per proxied request' },
  ],
  'error-handler': [
    { key: 'status_code', label: 'Status code', type: 'number', default: 502 },
    { key: 'content_type', label: 'Content type', type: 'select', options: ['application/json', 'text/plain', 'text/html'], default: 'application/json' },
    { key: 'body_template', label: 'Body template', type: 'textarea', rows: 4, default: '{"error": "{{error.code}}"}' },
  ],
  cors: [
    { key: 'allowed_origins', label: 'Allowed origins', type: 'list', addLabel: 'Origin', item: { type: 'text', placeholder: 'https://app.example.com' }, hint: 'defaults to * when empty' },
    { key: 'allowed_methods', label: 'Allowed methods', type: 'list', addLabel: 'Method', item: { type: 'text', placeholder: 'GET' }, hint: 'defaults to GET, POST, PUT, DELETE, OPTIONS' },
    { key: 'allowed_headers', label: 'Allowed headers', type: 'list', addLabel: 'Header', item: { type: 'text', placeholder: 'content-type' }, hint: 'defaults to * when empty' },
    { key: 'allow_credentials', label: 'Credentials', type: 'switch', switchLabel: 'Allow credentials', default: false },
    { key: 'max_age', label: 'Max age (s)', type: 'number', default: 3600 },
  ],
  'rate-limit': [
    { key: 'requests_per_second', label: 'Requests per second', type: 'number', default: 100 },
    { key: 'burst', label: 'Burst', type: 'number', hint: 'bucket capacity; defaults to requests per second' },
    { key: 'key_from', label: 'Key from', type: 'text', placeholder: 'header:x-api-key', hint: 'header:<name> keys per header value; empty keys per client address' },
  ],
  'ip-restriction': [
    { key: 'allow', label: 'Allow list', type: 'list', addLabel: 'CIDR', item: { type: 'text', placeholder: '10.0.0.0/8' }, hint: 'when set, only these IPs/CIDRs pass' },
    { key: 'deny', label: 'Deny list', type: 'list', addLabel: 'CIDR', item: { type: 'text', placeholder: '192.168.1.50' }, hint: 'checked before the allow list' },
  ],
  'request-size-limit': [
    { key: 'max_bytes', label: 'Max body size (bytes)', type: 'number', default: 1048576 },
    { key: 'reject_status', label: 'Reject status', type: 'number', default: 413 },
  ],
  'key-auth': [
    { key: 'header_name', label: 'Header name', type: 'text', default: 'x-api-key' },
    { key: 'keys', label: 'Valid keys', type: 'list', addLabel: 'Key', item: { type: 'text', placeholder: 'sk_live_…' } },
  ],
  'basic-auth': [
    { key: 'use_consumers', label: 'Use consumers', type: 'switch', switchLabel: 'Resolve credentials against consumers', default: false },
    { key: 'realm', label: 'Realm', type: 'text', default: 'restricted' },
    { key: 'users', label: 'Users', type: 'objects', addLabel: 'User', itemLabel: 'User',
      fields: [
        { key: 'username', label: 'Username', type: 'text' },
        { key: 'password', label: 'Password', type: 'text' },
      ] },
    { key: 'anonymous_consumer', label: 'Anonymous consumer', type: 'text', placeholder: 'guest' },
    { key: 'hide_credentials', label: 'Hide credentials', type: 'switch', switchLabel: 'Strip Authorization header', default: false },
  ],
  'jwt-auth': [
    { key: 'use_consumers', label: 'Use consumers', type: 'switch', switchLabel: 'Verify with per-consumer secrets', default: false },
    { key: 'secret', label: 'Secret', type: 'text', placeholder: '${JWT_SECRET}', hint: 'inline HMAC secret; optional when using consumers' },
    { key: 'algorithm', label: 'Algorithm', type: 'select', options: ['HS256', 'HS384', 'HS512'], default: 'HS256' },
    { key: 'header_name', label: 'Header name', type: 'text', default: 'authorization' },
  ],
  'hmac-auth': [
    { key: 'use_consumers', label: 'Use consumers', type: 'switch', switchLabel: 'Resolve access key against consumers', default: false },
    { key: 'access_key', label: 'Access key', type: 'text', placeholder: 'my-key-id', hint: 'inline single credential; requires secret key' },
    { key: 'secret_key', label: 'Secret key', type: 'text', placeholder: '${HMAC_SECRET}', hint: 'paired with access key' },
    { key: 'algorithm', label: 'Algorithm', type: 'select', options: ['hmac-sha1', 'hmac-sha256', 'hmac-sha512'], default: 'hmac-sha256' },
    { key: 'clock_skew', label: 'Clock skew (s)', type: 'number', default: 300, hint: '0 disables the Date check' },
    { key: 'signed_headers', label: 'Signed headers', type: 'list', addLabel: 'Header', item: { type: 'text', placeholder: 'date' } },
    { key: 'keep_headers', label: 'Keep headers', type: 'switch', switchLabel: 'Forward X-HMAC-* headers', default: false },
    { key: 'hide_credentials', label: 'Hide credentials', type: 'switch', switchLabel: 'Strip Authorization header', default: false },
    { key: 'anonymous_consumer', label: 'Anonymous consumer', type: 'text', placeholder: 'guest' },
    { key: 'realm', label: 'Realm', type: 'text', default: 'hmac' },
  ],
  'multi-auth': [
    { key: 'auth_plugins', label: 'Auth plugins', type: 'list', addLabel: 'Plugin', item: { type: 'text', placeholder: 'key-auth' }, hint: 'YAML-only: each entry is a single-key map {plugin-type: {config}} — edit as YAML' },
  ],
  'jwe-decrypt': [
    { key: 'header', label: 'Header', type: 'text', default: 'Authorization' },
    { key: 'forward_header', label: 'Forward header', type: 'text', default: 'Authorization' },
    { key: 'strict', label: 'Strict', type: 'switch', switchLabel: 'Reject requests with no token', default: true },
    { key: 'key', label: 'Inline key (base64, 32 bytes)', type: 'text', placeholder: 'base64 AES-256 key', hint: 'set this or enable Use consumers' },
    { key: 'use_consumers', label: 'Use consumers', type: 'switch', switchLabel: 'Resolve key from consumer store via kid', default: false },
  ],
  'consumer-restriction': [
    { key: 'type', label: 'Match by', type: 'select', options: ['consumer_name', 'consumer_group_id'], default: 'consumer_name' },
    { key: 'whitelist', label: 'Whitelist', type: 'list', addLabel: 'Value', item: { type: 'text', placeholder: 'alice' }, hint: 'allowed values; mutually exclusive with blacklist' },
    { key: 'blacklist', label: 'Blacklist', type: 'list', addLabel: 'Value', item: { type: 'text', placeholder: 'mallory' } },
    { key: 'allowed_by_methods', label: 'Method restrictions', type: 'objects', addLabel: 'Rule', itemLabel: 'Consumer',
      fields: [
        { key: 'user', label: 'Consumer name', type: 'text' },
        { key: 'methods', label: 'Methods', type: 'list', addLabel: 'Method', item: { type: 'text', placeholder: 'GET' } },
      ] },
    { key: 'rejected_code', label: 'Rejected code', type: 'number', default: 403 },
    { key: 'rejected_msg', label: 'Rejected message', type: 'text' },
  ],
  acl: [
    { key: 'allowed_by', label: 'Allowed groups', type: 'list', addLabel: 'Group', item: { type: 'text', placeholder: 'partners' }, hint: 'when set, only these consumer groups pass' },
    { key: 'denied_by', label: 'Denied groups', type: 'list', addLabel: 'Group', item: { type: 'text', placeholder: 'banned' }, hint: 'checked first — deny wins' },
    { key: 'rejected_code', label: 'Rejected code', type: 'number', default: 403 },
    { key: 'rejected_msg', label: 'Rejected message', type: 'text' },
  ],
  'attach-consumer-label': [
    { key: 'header_prefix', label: 'Header prefix', type: 'text', default: 'X-Consumer-', hint: 'prepended to each consumer label key to form the upstream header name' },
  ],
  // ---- Plugin catalog (Wave 7: serverless & FaaS) -------------------
  'serverless-pre-function': [
    { key: 'functions', label: 'Functions', type: 'list', addLabel: 'Function', item: { type: 'text', placeholder: 'function execute(ctx) ... end' }, hint: 'Lua source; each defines execute(ctx), threaded in order' },
    { key: 'timeout_ms', label: 'Timeout (ms)', type: 'number', default: 5000 },
    { key: 'phase', label: 'Phase', type: 'text', hint: 'accepted for config compatibility; inert (phase = graph position)' },
  ],
  'serverless-post-function': [
    { key: 'functions', label: 'Functions', type: 'list', addLabel: 'Function', item: { type: 'text', placeholder: 'function execute(ctx) ... end' }, hint: 'Lua source; each defines execute(ctx), threaded in order' },
    { key: 'timeout_ms', label: 'Timeout (ms)', type: 'number', default: 5000 },
    { key: 'phase', label: 'Phase', type: 'text', hint: 'accepted for config compatibility; inert (phase = graph position)' },
  ],
  'oas-validator': [
    { key: 'spec', label: 'OpenAPI spec (JSON)', type: 'textarea', rows: 12, placeholder: '{ "openapi": "3.0.0", "paths": { } }', hint: 'inline OpenAPI 3 document (JSON object)' },
    { key: 'rejected_code', label: 'Rejected code', type: 'number', default: 400 },
    { key: 'rejected_msg', label: 'Rejected message', type: 'text', placeholder: 'optional fixed message' },
  ],
  'aws-lambda': [
    { key: 'function_uri', label: 'Function URI', type: 'text', placeholder: 'https://xyz.lambda-url.us-east-1.on.aws/' },
    { key: 'authorization.apikey', label: 'API key', type: 'text', hint: 'sent as x-api-key; leave empty to use IAM' },
    { key: 'authorization.iam.accesskey', label: 'IAM access key', type: 'text' },
    { key: 'authorization.iam.secretkey', label: 'IAM secret key', type: 'text' },
    { key: 'authorization.iam.aws_region', label: 'AWS region', type: 'text', default: 'us-east-1' },
    { key: 'authorization.iam.aws_service', label: 'AWS service', type: 'text', default: 'lambda' },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', switchLabel: 'ssl_verify', default: false },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
  ],
  'azure-functions': [
    { key: 'function_uri', label: 'Function URI', type: 'text', placeholder: 'https://app.azurewebsites.net/api/HttpTrigger' },
    { key: 'authorization.apikey', label: 'API key (x-functions-key)', type: 'text' },
    { key: 'authorization.clientid', label: 'Client id (x-functions-clientid)', type: 'text' },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', switchLabel: 'ssl_verify', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
  ],
  openwhisk: [
    { key: 'api_host', label: 'API host', type: 'text', placeholder: 'https://ow.example.com' },
    { key: 'service_token', label: 'Service token (user:pass)', type: 'text' },
    { key: 'namespace', label: 'Namespace', type: 'text', default: '_' },
    { key: 'package', label: 'Package', type: 'text' },
    { key: 'action', label: 'Action', type: 'text' },
    { key: 'result', label: 'Inline result', type: 'switch', switchLabel: 'result', default: true },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', switchLabel: 'ssl_verify', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
  ],
  openfunction: [
    { key: 'function_uri', label: 'Function URI', type: 'text', placeholder: 'http://openfunction.svc/default/hello' },
    { key: 'authorization.service_token', label: 'Service token', type: 'text', hint: 'sent as Authorization: Basic base64(token)' },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', switchLabel: 'ssl_verify', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
  ],
  // ---- Plugin catalog (Wave 6: tracing & metrics) -------------------
  // Tracers are start/end node pairs: place a `start` node after listener and
  // an `end` node after upstream; both share the span via context state.
  opentelemetry: [
    { key: 'phase', label: 'Phase', type: 'radio', options: ['start', 'end'], default: 'start' },
    { key: 'endpoint', label: 'Collector endpoint', type: 'text', placeholder: 'http://localhost:4318', hint: 'OTLP/HTTP base; traces POST to /v1/traces' },
    { key: 'service_name', label: 'Service name', type: 'text', default: 'featherbit' },
    { key: 'sampler', label: 'Sampler', type: 'select', options: ['always_on', 'always_off', 'trace_id_ratio'], default: 'always_on', hint: 'trace_id_ratio uses sampler.options.fraction' },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', switchLabel: 'ssl_verify', default: true },
    { key: 'timeout', label: 'Export timeout (s)', type: 'number', default: 3 },
  ],
  zipkin: [
    { key: 'phase', label: 'Phase', type: 'radio', options: ['start', 'end'], default: 'start' },
    { key: 'endpoint', label: 'Collector endpoint', type: 'text', placeholder: 'http://localhost:9411/api/v2/spans', hint: 'full Zipkin v2 spans URL (required)' },
    { key: 'service_name', label: 'Service name', type: 'text', default: 'featherbit' },
    { key: 'server_addr', label: 'Server addr (ipv4)', type: 'text', placeholder: '10.0.0.5' },
    { key: 'sample_ratio', label: 'Sample ratio', type: 'number', default: 1.0, hint: '0.0-1.0 of new traces' },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', switchLabel: 'ssl_verify', default: true },
    { key: 'timeout', label: 'Export timeout (s)', type: 'number', default: 3 },
  ],
  skywalking: [
    { key: 'phase', label: 'Phase', type: 'radio', options: ['start', 'end'], default: 'start' },
    { key: 'endpoint_addr', label: 'OAP endpoint', type: 'text', default: 'http://127.0.0.1:12800', hint: 'segments POST to /v3/segments' },
    { key: 'service_name', label: 'Service name', type: 'text', default: 'featherbit' },
    { key: 'service_instance_name', label: 'Instance name', type: 'text', default: 'featherbit Instance Name' },
    { key: 'sample_ratio', label: 'Sample ratio', type: 'number', default: 1, hint: '0.0-1.0 of new traces' },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 3 },
  ],
  prometheus: [
    { key: 'prefer_name', label: 'Prefer name', type: 'switch', switchLabel: 'Accepted for compatibility (inert)', default: false },
  ],
  // ---- Plugin catalog (Wave 5: loggers) -----------------------------
  // All loggers are fire-and-forget: place after upstream. They share the
  // batch keys (batch_max_size, inactive_timeout, buffer_duration,
  // max_retry_count, retry_delay) and log_format (YAML-only nested map).
  'http-logger': [
    { key: 'uri', label: 'Endpoint URI', type: 'text', placeholder: 'http://collector:3000/logs' },
    { key: 'method', label: 'Method', type: 'select', options: ['POST', 'PUT', 'PATCH'], default: 'POST' },
    { key: 'auth_header', label: 'Authorization header', type: 'text' },
    { key: 'concat_method', label: 'Body format', type: 'radio', options: ['json', 'new_line'], default: 'json' },
    { key: 'ssl_verify', label: 'TLS verify', type: 'switch', switchLabel: 'Verify certificates', default: false },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 3 },
    { key: 'include_req_body', label: 'Request body', type: 'switch', switchLabel: 'Include request body', default: false },
    { key: 'include_resp_body', label: 'Response body', type: 'switch', switchLabel: 'Include response body', default: false },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  'loki-logger': [
    { key: 'endpoint_addrs', label: 'Endpoint addresses', type: 'list', addLabel: 'Add address', item: { type: 'text', placeholder: 'http://loki:3100' } },
    { key: 'endpoint_uri', label: 'Push path', type: 'text', default: '/loki/api/v1/push' },
    { key: 'tenant_id', label: 'Tenant ID', type: 'text', default: 'fake' },
    { key: 'ssl_verify', label: 'TLS verify', type: 'switch', switchLabel: 'Verify certificates', default: false },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  'splunk-hec-logging': [
    { key: 'endpoint.uri', label: 'HEC URI', type: 'text', placeholder: 'https://splunk:8088/services/collector' },
    { key: 'endpoint.token', label: 'HEC token', type: 'text' },
    { key: 'endpoint.channel', label: 'Channel', type: 'text' },
    { key: 'source', label: 'Source', type: 'text', default: 'featherbit-splunk-hec-logging' },
    { key: 'ssl_verify', label: 'TLS verify', type: 'switch', switchLabel: 'Verify certificates', default: true },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  datadog: [
    { key: 'host', label: 'Agent host', type: 'text', default: '127.0.0.1' },
    { key: 'port', label: 'DogStatsD port', type: 'number', default: 8125 },
    { key: 'namespace', label: 'Namespace', type: 'text', default: 'featherbit' },
    { key: 'constant_tags', label: 'Constant tags', type: 'list', addLabel: 'Add tag', item: { type: 'text', placeholder: 'source:featherbit' } },
    { key: 'include_path', label: 'Path tag', type: 'switch', switchLabel: 'Include path tag', default: false },
    { key: 'include_method', label: 'Method tag', type: 'switch', switchLabel: 'Include method tag', default: false },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  loggly: [
    { key: 'customer_token', label: 'Customer token', type: 'text' },
    { key: 'tags', label: 'Tags', type: 'list', addLabel: 'Add tag', item: { type: 'text', placeholder: 'featherbit' } },
    { key: 'host', label: 'Host', type: 'text', default: 'logs-01.loggly.com' },
    { key: 'ssl_verify', label: 'TLS verify', type: 'switch', switchLabel: 'Verify certificates', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 5000 },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  'elasticsearch-logger': [
    { key: 'endpoint_addr', label: 'Endpoint', type: 'text', placeholder: 'http://es:9200' },
    { key: 'field.index', label: 'Index', type: 'text', placeholder: 'services' },
    { key: 'auth.username', label: 'Username', type: 'text' },
    { key: 'auth.password', label: 'Password', type: 'text', placeholder: '${ES_PASSWORD}' },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', switchLabel: 'Verify TLS certificates', default: true },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 10 },
    { key: 'include_req_body', label: 'Request body', type: 'switch', switchLabel: 'Include request body', default: false },
    { key: 'include_resp_body', label: 'Response body', type: 'switch', switchLabel: 'Include response body', default: false },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  'clickhouse-logger': [
    { key: 'endpoint_addr', label: 'Endpoint', type: 'text', placeholder: 'http://clickhouse:8123' },
    { key: 'database', label: 'Database', type: 'text', placeholder: 'default' },
    { key: 'logtable', label: 'Log table', type: 'text', placeholder: 'gateway_logs' },
    { key: 'user', label: 'User', type: 'text', default: '' },
    { key: 'password', label: 'Password', type: 'text', placeholder: '${CLICKHOUSE_PASSWORD}' },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', switchLabel: 'Verify TLS certificates', default: true },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 3 },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  'sls-logger': [
    { key: 'host', label: 'Host', type: 'text', placeholder: 'cn-hangzhou.log.aliyuncs.com' },
    { key: 'port', label: 'Port', type: 'number', default: 443 },
    { key: 'project', label: 'Project', type: 'text', placeholder: 'my-project' },
    { key: 'logstore', label: 'Logstore', type: 'text', placeholder: 'gateway' },
    { key: 'access_key_id', label: 'Access key id', type: 'text', placeholder: '${SLS_KEY_ID}' },
    { key: 'access_key_secret', label: 'Access key secret', type: 'text', placeholder: '${SLS_KEY_SECRET}' },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 5000 },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  'tencent-cloud-cls': [
    { key: 'cls_host', label: 'CLS host', type: 'text', placeholder: 'ap-guangzhou.cls.tencentcs.com' },
    { key: 'cls_topic', label: 'CLS topic', type: 'text', placeholder: 'xxxxxxxx-xxxx-xxxx' },
    { key: 'secret_id', label: 'Secret id', type: 'text', placeholder: '${CLS_SECRET_ID}' },
    { key: 'secret_key', label: 'Secret key', type: 'text', placeholder: '${CLS_SECRET_KEY}' },
    { key: 'scheme', label: 'Scheme', type: 'select', options: ['http', 'https'], default: 'https' },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', switchLabel: 'Verify TLS certificates', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 10000 },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  'tcp-logger': [
    { key: 'host', label: 'Host', type: 'text', placeholder: '127.0.0.1' },
    { key: 'port', label: 'Port', type: 'number', default: 5044 },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 1000 },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
    { key: 'include_req_body', label: 'Request body', type: 'switch', switchLabel: 'Include request body', default: false },
    { key: 'include_resp_body', label: 'Response body', type: 'switch', switchLabel: 'Include response body', default: false },
  ],
  'udp-logger': [
    { key: 'host', label: 'Host', type: 'text', placeholder: '127.0.0.1' },
    { key: 'port', label: 'Port', type: 'number', default: 5140 },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 3 },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
    { key: 'include_req_body', label: 'Request body', type: 'switch', switchLabel: 'Include request body', default: false },
    { key: 'include_resp_body', label: 'Response body', type: 'switch', switchLabel: 'Include response body', default: false },
  ],
  syslog: [
    { key: 'host', label: 'Host', type: 'text', placeholder: '127.0.0.1' },
    { key: 'port', label: 'Port', type: 'number', default: 5140 },
    { key: 'sock_type', label: 'Transport', type: 'select', options: ['tcp', 'udp'], default: 'tcp' },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
    { key: 'include_req_body', label: 'Request body', type: 'switch', switchLabel: 'Include request body', default: false },
  ],
  'file-logger': [
    { key: 'path', label: 'File path', type: 'text', placeholder: '/var/log/featherbit/access.log' },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000, hint: 'set 1 for immediate per-request writes' },
    { key: 'include_req_body', label: 'Request body', type: 'switch', switchLabel: 'Include request body', default: false },
    { key: 'include_resp_body', label: 'Response body', type: 'switch', switchLabel: 'Include response body', default: false },
  ],
  'error-log-logger': [
    { key: 'host', label: 'Host', type: 'text', placeholder: '127.0.0.1' },
    { key: 'port', label: 'Port', type: 'number', default: 5044 },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 3 },
    { key: 'level', label: 'Level', type: 'select', options: ['STDERR','EMERG','ALERT','CRIT','ERR','ERROR','WARN','NOTICE','INFO','DEBUG'], default: 'WARN', hint: 'accepted for compatibility; not enforced' },
    { key: 'batch_max_size', label: 'Batch max size', type: 'number', default: 1000 },
  ],
  'google-cloud-logging': [
    { key: 'auth_file', label: 'Auth file', type: 'text', placeholder: '/etc/gateway/gcp-sa.json', hint: 'path to service-account JSON; or set auth_config in YAML' },
    { key: 'log_id', label: 'Log ID', type: 'text', default: 'featherbit%2Flogs' },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 10 },
  ],
  'skywalking-logger': [
    { key: 'endpoint_addr', label: 'OAP endpoint', type: 'text', placeholder: 'http://127.0.0.1:12800', hint: 'required' },
    { key: 'service_name', label: 'Service name', type: 'text', default: 'featherbit' },
    { key: 'service_instance_name', label: 'Instance name', type: 'text', default: 'featherbit Instance Name' },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 3 },
  ],
  lago: [
    { key: 'endpoint', label: 'Lago endpoint', type: 'text', placeholder: 'http://127.0.0.1:3000', hint: 'required' },
    { key: 'token', label: 'API token', type: 'text', hint: 'required - Lago API key' },
    { key: 'event_transaction_id', label: 'Transaction ID', type: 'text', placeholder: 'req_$request_uri', hint: 'required - $var template' },
    { key: 'subscription_id', label: 'Subscription ID', type: 'text', placeholder: 'cus_$consumer_name', hint: 'required - $var template' },
    { key: 'event_code', label: 'Event code', type: 'text', hint: 'required - billable metric code' },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
  ],
  // ---- Plugin catalog (Wave 4: traffic control) ---------------------
  'limit-count': [
    { key: 'count', label: 'Count', type: 'number', hint: 'requests allowed per window' },
    { key: 'time_window', label: 'Time window (s)', type: 'number', hint: 'window length in seconds' },
    { key: 'key', label: 'Key', type: 'text', default: '$remote_addr', placeholder: '$remote_addr', hint: '$var template; empty falls back to client address' },
    { key: 'policy', label: 'Policy', type: 'select', options: ['local'], default: 'local' },
    { key: 'group', label: 'Group', type: 'text', placeholder: 'shared-counter', hint: 'prefixes the key so nodes share a counter' },
    { key: 'rejected_code', label: 'Rejected code', type: 'number', default: 503 },
    { key: 'rejected_msg', label: 'Rejected message', type: 'text', placeholder: 'Requests over the limit' },
    { key: 'show_limit_quota_header', label: 'Show quota headers', type: 'switch', switchLabel: 'Emit X-RateLimit-* headers', default: true },
    { key: 'allow_degradation', label: 'Allow degradation', type: 'switch', switchLabel: 'Pass through on counter error', default: false },
  ],
  'proxy-mirror': [
    { key: 'host', label: 'Shadow host', type: 'text', placeholder: 'http://shadow:8080', hint: 'scheme + host + optional port; no trailing path' },
    { key: 'path', label: 'Path override', type: 'text', placeholder: '/mirror', hint: 'optional; original query string is always appended' },
    { key: 'sample_ratio', label: 'Sample ratio', type: 'number', default: 1, hint: '0.0-1.0 fraction of requests mirrored (pseudo-random)' },
  ],
  'traffic-split': [
    { key: 'timeout_ms', label: 'Timeout (ms)', type: 'number', default: 60000, hint: 'whole-call deadline for split-proxied requests' },
    { key: 'rules', label: 'Rules', type: 'objects', addLabel: 'Rule', itemLabel: 'Rule', hint: 'first rule whose match passes is used; edit match / weighted_upstreams as YAML',
      fields: [
        { key: 'match', label: 'Match (vars)', type: 'textarea', rows: 3, placeholder: '[["arg_canary", "==", "1"]]', hint: 'triple-array match expression; empty = match all' },
        { key: 'weighted_upstreams', label: 'Weighted upstreams', type: 'objects', addLabel: 'Slot', itemLabel: 'Slot',
          fields: [
            { key: 'weight', label: 'Weight', type: 'number', default: 1 },
            { key: 'upstream', label: 'Upstream (targets)', type: 'textarea', rows: 3, placeholder: '{ "targets": [{ "host": "canary", "port": 8080 }] }', hint: 'omit for the default slot (falls through to the route upstream)' },
          ] },
      ] },
  ],
  'limit-conn': [
    { key: 'phase', label: 'Phase', type: 'radio', options: ['acquire', 'release'], default: 'acquire', hint: 'acquire before upstream, release after (on success and error)' },
    { key: 'conn', label: 'Max concurrent', type: 'number', default: 20 },
    { key: 'burst', label: 'Burst', type: 'number', default: 0, hint: 'extra concurrency above conn; ceiling is conn + burst' },
    { key: 'key', label: 'Key', type: 'text', default: '$remote_addr', hint: 'both nodes of the pair must share this key' },
    { key: 'key_type', label: 'Key type', type: 'select', options: ['var', 'constant'], default: 'var' },
    { key: 'rejected_code', label: 'Rejected code', type: 'number', default: 503 },
    { key: 'rejected_msg', label: 'Rejected message', type: 'text' },
  ],
  'api-breaker': [
    { key: 'phase', label: 'Phase', type: 'radio', options: ['check', 'observe'], default: 'check', hint: 'check before upstream, observe after' },
    { key: 'id', label: 'Breaker id', type: 'text', placeholder: 'orders-api', hint: 'check and observe nodes must share this id' },
    { key: 'break_response_code', label: 'Break response code', type: 'number', default: 502 },
    { key: 'break_response_body', label: 'Break response body', type: 'textarea', rows: 2 },
    { key: 'break_base_sec', label: 'Base cooldown (s)', type: 'number', default: 2, hint: 'grows as base * 2^trip' },
    { key: 'max_breaker_sec', label: 'Max cooldown (s)', type: 'number', default: 300 },
  ],
  'proxy-cache': [
    { key: 'phase', label: 'Phase', type: 'radio', options: ['lookup', 'store'], default: 'lookup', hint: 'lookup before upstream, store after' },
    { key: 'id', label: 'Cache id', type: 'text', placeholder: 'catalog', hint: 'lookup and store nodes must share this id' },
    { key: 'cache_key', label: 'Cache key', type: 'list', addLabel: 'Component', item: { type: 'text', placeholder: '$uri' }, hint: 'defaults to $request_method + $host + $uri' },
    { key: 'cache_ttl', label: 'Cache TTL (s)', type: 'number', default: 300 },
    { key: 'cache_http_statuses', label: 'Cacheable statuses', type: 'list', addLabel: 'Status', item: { type: 'number' }, hint: 'defaults to 200, 301, 404' },
    { key: 'cache_method', label: 'Cacheable methods', type: 'list', addLabel: 'Method', item: { type: 'text', placeholder: 'GET' }, hint: 'defaults to GET, HEAD' },
    { key: 'hide_cache_headers', label: 'Hide cache headers', type: 'switch', switchLabel: 'Strip cache-control/expires on hits', default: false },
  ],
  // ---- Plugin catalog (Wave 3: callout auth & authz) -----------------
  'forward-auth': [
    { key: 'uri', label: 'Auth service URI', type: 'text', placeholder: 'http://auth-service:8080/verify' },
    { key: 'request_method', label: 'Request method', type: 'select', options: ['GET', 'POST'], default: 'GET' },
    { key: 'request_headers', label: 'Request headers', type: 'list', addLabel: 'Header', item: { type: 'text', placeholder: 'authorization' } },
    { key: 'upstream_headers', label: 'Upstream headers', type: 'list', addLabel: 'Header', item: { type: 'text', placeholder: 'x-user-id' } },
    { key: 'client_headers', label: 'Client headers', type: 'list', addLabel: 'Header', item: { type: 'text', placeholder: 'www-authenticate' } },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', switchLabel: 'Verify TLS certificates', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
    { key: 'status_on_error', label: 'Status on error', type: 'number', default: 403 },
    { key: 'allow_degradation', label: 'Allow degradation', type: 'switch', switchLabel: 'Fail open on callout error', default: false },
  ],
  opa: [
    { key: 'host', label: 'OPA host', type: 'text', placeholder: 'http://opa:8181' },
    { key: 'policy', label: 'Policy path', type: 'text', placeholder: 'example/allow' },
    { key: 'with_consumer', label: 'With consumer', type: 'switch', switchLabel: 'Include consumer object in input', default: false },
    { key: 'send_headers_upstream', label: 'Send headers upstream', type: 'list', addLabel: 'Header', item: { type: 'text', placeholder: 'x-user-id' } },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', switchLabel: 'Verify TLS certificates', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
  ],
  'authz-casbin': [
    { key: 'model_path', label: 'Model file path', type: 'text', placeholder: '/etc/featherbit/model.conf', hint: 'with policy_path; host file mode' },
    { key: 'policy_path', label: 'Policy file path', type: 'text', placeholder: '/etc/featherbit/policy.csv' },
    { key: 'model', label: 'Model (inline)', type: 'text', hint: 'inline Casbin model config; with policy' },
    { key: 'policy', label: 'Policy (inline)', type: 'text', hint: 'inline CSV policy' },
    { key: 'username_header', label: 'Username header', type: 'text', default: 'x-user', hint: 'subject fallback when no consumer' },
  ],
  'authz-keycloak': [
    { key: 'token_endpoint', label: 'Token endpoint', type: 'text', placeholder: 'https://kc/realms/r/protocol/openid-connect/token' },
    { key: 'client_id', label: 'Client ID', type: 'text' },
    { key: 'permissions', label: 'Permissions', type: 'list', addLabel: 'Permission', item: { type: 'text', placeholder: 'Default Resource#read' } },
    { key: 'policy_enforcement_mode', label: 'Enforcement mode', type: 'select', options: ['ENFORCING', 'PERMISSIVE'], default: 'ENFORCING' },
    { key: 'http_method_as_scope', label: 'Method as scope', type: 'switch', switchLabel: 'Append request method as scope', default: false },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
  ],
  'authz-casdoor': [
    { key: 'endpoint_addr', label: 'Casdoor endpoint', type: 'text', placeholder: 'https://casdoor.example.com' },
    { key: 'client_id', label: 'Client ID', type: 'text' },
    { key: 'client_secret', label: 'Client secret', type: 'text', placeholder: '${CASDOOR_CLIENT_SECRET}' },
    { key: 'callback_url', label: 'Callback URL', type: 'text', hint: 'required for interactive mode; the OAuth redirect path' },
    { key: 'ssl_verify', label: 'Verify TLS', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 3000 },
    { key: 'session_secret', label: 'Session secret', type: 'text', placeholder: '${CASDOOR_SESSION_SECRET}', hint: 'set to enable interactive OAuth SSO login (encrypted cookie session); leave blank for stateless bearer-token introspection' },
    { key: 'scope', label: 'OAuth scope', type: 'text', default: 'read', hint: 'interactive mode only' },
    { key: 'session_cookie_name', label: 'Session cookie name', type: 'text', default: 'casdoor_session', hint: 'interactive mode only; flow cookie is <name>_flow' },
    { key: 'session_cookie_path', label: 'Session cookie path', type: 'text', default: '/', hint: 'interactive mode only; scope to a subpath (e.g. /app_a) for independent per-app sessions; must cover the callback URL path' },
    { key: 'session_cookie_lifetime', label: 'Session lifetime (s)', type: 'number', default: 3600, hint: 'interactive mode only' },
    { key: 'logout_path', label: 'Logout path', type: 'text', placeholder: '/logout', hint: 'optional; clears the session cookie and redirects to /' },
  ],
  'ldap-auth': [
    { key: 'ldap_uri', label: 'LDAP URI', type: 'text', placeholder: 'ldap://ldap.example.org:389' },
    { key: 'base_dn', label: 'Base DN', type: 'text', placeholder: 'ou=users,dc=example,dc=org' },
    { key: 'uid', label: 'UID attribute', type: 'text', default: 'cn' },
    { key: 'use_tls', label: 'Use TLS', type: 'switch', switchLabel: 'Negotiate StartTLS', default: false },
    { key: 'tls_verify', label: 'Verify TLS cert', type: 'switch', default: false },
    { key: 'realm', label: 'Realm', type: 'text', default: 'ldap' },
  ],
  'wolf-rbac': [
    { key: 'server', label: 'Wolf server', type: 'text', default: 'http://127.0.0.1:12180' },
    { key: 'appid', label: 'App id', type: 'text', default: 'unset' },
    { key: 'header_prefix', label: 'Header prefix', type: 'text', default: 'X-' },
    { key: 'ssl_verify', label: 'Verify TLS cert', type: 'switch', default: false },
  ],
  'cas-auth': [
    { key: 'idp_uri', label: 'CAS server URI', type: 'text', placeholder: 'https://cas.example.org/cas' },
    { key: 'service', label: 'Service URL', type: 'text', placeholder: 'https://app.example.org/', hint: 'defaults to request scheme/host/path' },
    { key: 'ticket_param', label: 'Ticket param', type: 'text', default: 'ticket' },
    { key: 'ssl_verify', label: 'Verify TLS cert', type: 'switch', default: true },
    { key: 'session_secret', label: 'Session secret', type: 'text', placeholder: '${CAS_SESSION_SECRET}', hint: 'set to enable interactive SSO login (encrypted cookie session); leave blank for stateless ticket validation' },
    { key: 'session_cookie_name', label: 'Session cookie name', type: 'text', default: 'cas_session', hint: 'interactive mode only' },
    { key: 'session_cookie_path', label: 'Session cookie path', type: 'text', default: '/', hint: 'interactive mode only; scope to a subpath (e.g. /app_a) for independent per-app sessions' },
    { key: 'session_cookie_lifetime', label: 'Session lifetime (s)', type: 'number', default: 3600, hint: 'interactive mode only' },
    { key: 'logout_path', label: 'Logout path', type: 'text', placeholder: '/logout', hint: 'optional; clears the session cookie and redirects to /' },
  ],
  'openid-connect': [
    { key: 'discovery', label: 'Discovery URL', type: 'text', placeholder: 'https://idp.example.com/.well-known/openid-configuration', hint: 'resolves jwks_uri; or set jwks_uri directly' },
    { key: 'jwks_uri', label: 'JWKS URI', type: 'text', placeholder: 'https://idp.example.com/jwks' },
    { key: 'introspection_endpoint', label: 'Introspection endpoint', type: 'text', hint: 'used only when no JWKS source is set' },
    { key: 'client_id', label: 'Client ID', type: 'text' },
    { key: 'client_secret', label: 'Client secret', type: 'text', placeholder: '${OIDC_CLIENT_SECRET}', hint: 'required for introspection' },
    { key: 'bearer_only', label: 'Bearer only', type: 'switch', switchLabel: 'Off = interactive login flow', default: true },
    { key: 'token_signing_alg_values_expected', label: 'Allowed algorithms', type: 'text', default: 'RS256', placeholder: 'RS256, ES256' },
    { key: 'set_userinfo_header', label: 'Set X-Userinfo header', type: 'switch', default: true },
    { key: 'set_access_token_header', label: 'Forward access token', type: 'switch', default: true },
    { key: 'access_token_in_authorization_header', label: 'Token in Authorization', type: 'switch', switchLabel: 'Keep in Authorization instead of X-Access-Token', default: false },
    { key: 'redirect_uri', label: 'Redirect URI', type: 'text', placeholder: 'https://app.example.com/oidc/callback', hint: 'interactive mode: callback URL; its path must be on this route' },
    { key: 'session_secret', label: 'Session secret', type: 'text', placeholder: '${SESSION_SECRET}', hint: 'interactive mode: seals the session cookie; same on every instance' },
    { key: 'session_cookie_name', label: 'Session cookie name', type: 'text', default: 'oidc_session', hint: 'interactive mode: session cookie name (flow cookie is <name>_flow); give two apps distinct names for independent sessions' },
    { key: 'session_cookie_path', label: 'Session cookie path', type: 'text', default: '/', hint: 'interactive mode: cookie Path; scope to a subpath (e.g. /app_a) so nodes on different subpaths keep separate sessions; must cover the redirect URI path' },
    { key: 'session_cookie_lifetime', label: 'Session lifetime (s)', type: 'number', default: 3600, hint: 'interactive mode: session cookie lifetime' },
    { key: 'scope', label: 'Scope', type: 'text', default: 'openid', hint: 'interactive mode: OAuth scopes' },
    { key: 'logout_path', label: 'Logout path', type: 'text', placeholder: '/logout', hint: 'interactive mode: clears the session' },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (s)', type: 'number', default: 3 },
  ],
  'dingtalk-auth': [
    { key: 'app_key', label: 'App key', type: 'text', placeholder: '${DINGTALK_APP_KEY}' },
    { key: 'app_secret', label: 'App secret', type: 'text', placeholder: '${DINGTALK_APP_SECRET}' },
    { key: 'code_header', label: 'Code header', type: 'text', default: 'X-DingTalk-Code' },
    { key: 'code_query', label: 'Code query param', type: 'text', default: 'code' },
    { key: 'access_token_url', label: 'Access token URL', type: 'text', default: 'https://api.dingtalk.com/v1.0/oauth2/accessToken' },
    { key: 'userinfo_url', label: 'Userinfo URL', type: 'text', default: 'https://oapi.dingtalk.com/topapi/v2/user/getuserinfo' },
    { key: 'set_userinfo_header', label: 'Set X-Userinfo header', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 6000 },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', default: true },
  ],
  'feishu-auth': [
    { key: 'app_id', label: 'App ID', type: 'text', placeholder: '${FEISHU_APP_ID}' },
    { key: 'app_secret', label: 'App secret', type: 'text', placeholder: '${FEISHU_APP_SECRET}' },
    { key: 'auth_redirect_uri', label: 'Auth redirect URI', type: 'text', placeholder: 'https://app.example.com/callback', hint: 'part of the token-exchange body' },
    { key: 'code_header', label: 'Code header', type: 'text', default: 'X-Feishu-Code' },
    { key: 'code_query', label: 'Code query param', type: 'text', default: 'code' },
    { key: 'access_token_url', label: 'Access token URL', type: 'text', default: 'https://open.feishu.cn/open-apis/authen/v2/oauth/token' },
    { key: 'userinfo_url', label: 'Userinfo URL', type: 'text', default: 'https://open.feishu.cn/open-apis/authen/v1/user_info' },
    { key: 'set_userinfo_header', label: 'Set X-Userinfo header', type: 'switch', default: true },
    { key: 'timeout', label: 'Timeout (ms)', type: 'number', default: 6000 },
    { key: 'ssl_verify', label: 'SSL verify', type: 'switch', default: true },
  ],
  logging: [
    { key: 'level', label: 'Level', type: 'radio', options: ['debug', 'info', 'warn', 'error'], default: 'info' },
    { key: 'format', label: 'Format', type: 'select', options: ['json', 'text'], default: 'json' },
    { key: 'include_headers', label: 'Headers', type: 'switch', switchLabel: 'Include request headers', default: false },
  ],
  script: [
    { key: 'runtime', label: 'Runtime', type: 'select', options: ['lua', 'python'], default: 'lua' },
    { key: 'source', label: 'Source file', type: 'text', placeholder: '/etc/gateway/plugins/custom.lua', hint: 'path on the gateway host' },
    { key: 'inline', label: 'Inline script', type: 'textarea', rows: 8, placeholder: 'function execute(ctx) … end', hint: 'used when no source file is set' },
  ],
  // ---- Plugin catalog (Wave 1) --------------------------------------
  // Some plugins (error-page, response-rewrite headers, exit-transformer
  // status_map) carry nested-map config the SchemaForm cannot render; those
  // keys are documented as YAML-only on their reference pages.
  'request-id': [
    { key: 'header_name', label: 'Header name', type: 'text', default: 'X-Request-Id' },
    { key: 'include_in_response', label: 'Response', type: 'switch', switchLabel: 'Echo id on the response', default: true },
    { key: 'algorithm', label: 'Algorithm', type: 'select', options: ['uuid'], default: 'uuid', hint: 'only uuid (v4) is supported' },
  ],
  'real-ip': [
    { key: 'source', label: 'Source', type: 'text', placeholder: 'http_x_forwarded_for', hint: 'required — variable holding the real address, e.g. http_x_real_ip' },
    { key: 'trusted_addresses', label: 'Trusted addresses', type: 'list', addLabel: 'CIDR', item: { type: 'text', placeholder: '10.0.0.0/8' }, hint: 'rewrite only when the direct peer matches; empty means always' },
    { key: 'recursive', label: 'Recursive', type: 'switch', switchLabel: 'Walk X-Forwarded-For skipping trusted hops', default: false },
  ],
  redirect: [
    { key: 'uri', label: 'Target URI', type: 'text', placeholder: 'https://$host/new$uri', hint: 'template with $vars; set this or HTTP to HTTPS, not both' },
    { key: 'http_to_https', label: 'HTTP to HTTPS', type: 'switch', switchLabel: 'Redirect plain HTTP to https', default: false },
    { key: 'ret_code', label: 'Status code', type: 'number', default: 302, hint: 'ignored by http_to_https (301 GET/HEAD, 308 otherwise)' },
    { key: 'append_query_string', label: 'Query string', type: 'switch', switchLabel: 'Append original query string', default: false },
  ],
  echo: [
    { key: 'body', label: 'Body', type: 'textarea', rows: 4, hint: 'replaces the upstream response body' },
    { key: 'before_body', label: 'Before body', type: 'text', hint: 'prepended to the body' },
    { key: 'after_body', label: 'After body', type: 'text', hint: 'appended to the body' },
    { key: 'headers', label: 'Headers', type: 'objects', addLabel: 'Header', itemLabel: 'Header',
      fields: [
        { key: 'name', label: 'Name', type: 'text', placeholder: 'x-served-by' },
        { key: 'value', label: 'Value', type: 'text' },
      ] },
  ],
  'ua-restriction': [
    { key: 'allowlist', label: 'Allowlist', type: 'list', addLabel: 'Regex', item: { type: 'text', placeholder: 'Mozilla.*' }, hint: 'only matching User-Agents pass; set exactly one list' },
    { key: 'denylist', label: 'Denylist', type: 'list', addLabel: 'Regex', item: { type: 'text', placeholder: 'curl/.*' }, hint: 'matching User-Agents are rejected' },
    { key: 'bypass_missing', label: 'Missing User-Agent', type: 'switch', switchLabel: 'Bypass when header absent', default: false },
    { key: 'rejected_code', label: 'Rejected code', type: 'number', default: 403 },
    { key: 'rejected_msg', label: 'Rejected message', type: 'text', placeholder: 'Not allowed' },
  ],
  'referer-restriction': [
    { key: 'whitelist', label: 'Whitelist', type: 'list', addLabel: 'Host', item: { type: 'text', placeholder: '*.example.com' }, hint: 'only these Referer hosts pass; set exactly one list' },
    { key: 'blacklist', label: 'Blacklist', type: 'list', addLabel: 'Host', item: { type: 'text', placeholder: 'evil.com' }, hint: 'these Referer hosts are rejected' },
    { key: 'bypass_missing', label: 'Missing Referer', type: 'switch', switchLabel: 'Bypass when missing or malformed', default: false },
    { key: 'message', label: 'Rejected message', type: 'text', placeholder: 'Your referer host is not allowed' },
  ],
  'uri-blocker': [
    { key: 'block_rules', label: 'Block rules', type: 'list', addLabel: 'Regex', item: { type: 'text', placeholder: '^/admin/' }, hint: 'required; matched against path + query string' },
    { key: 'rejected_code', label: 'Rejected code', type: 'number', default: 403 },
    { key: 'rejected_msg', label: 'Rejected message', type: 'text', hint: 'when set, body is {"error_msg": ...}; empty body otherwise' },
    { key: 'case_insensitive', label: 'Case', type: 'switch', switchLabel: 'Case-insensitive matching', default: false },
  ],
  csrf: [
    { key: 'key', label: 'Secret key', type: 'text', placeholder: '${CSRF_SECRET}', hint: 'required; HMAC secret for signing tokens' },
    { key: 'expires', label: 'Expires (s)', type: 'number', default: 7200, hint: '0 disables the expiry check' },
    { key: 'name', label: 'Token name', type: 'text', default: 'featherbit-csrf-token', hint: 'cookie and request-header name' },
    { key: 'phase', label: 'Phase', type: 'radio', options: ['request', 'response'], default: 'request', hint: 'request validates unsafe methods; response issues the cookie (place after upstream)' },
  ],
  'response-rewrite': [
    { key: 'status_code', label: 'Status code', type: 'number', hint: '200-598; empty keeps the current status' },
    { key: 'body', label: 'Body', type: 'textarea', rows: 4, hint: 'replaces the body; mutually exclusive with filters' },
    { key: 'body_base64', label: 'Base64', type: 'switch', switchLabel: 'Decode body from base64', default: false },
    { key: 'filters', label: 'Body filters', type: 'objects', addLabel: 'Filter', itemLabel: 'Filter',
      fields: [
        { key: 'regex', label: 'Regex', type: 'text' },
        { key: 'replace', label: 'Replace', type: 'text' },
        { key: 'scope', label: 'Scope', type: 'radio', options: ['once', 'global'], default: 'once' },
        { key: 'options', label: 'Options', type: 'select', options: [{ value: '', label: 'none' }, { value: 'i', label: 'i (case-insensitive)' }], default: '' },
      ] },
  ],
  gzip: [
    { key: 'types', label: 'Content types', type: 'list', addLabel: 'Type', item: { type: 'text', placeholder: 'text/html' }, hint: 'defaults to text/html; "*" (YAML) matches any' },
    { key: 'min_length', label: 'Min length (bytes)', type: 'number', default: 20 },
    { key: 'comp_level', label: 'Compression level', type: 'number', default: 1, hint: '1-9' },
    { key: 'vary', label: 'Vary', type: 'switch', switchLabel: 'Add Vary: Accept-Encoding', default: false },
  ],
  brotli: [
    { key: 'types', label: 'Content types', type: 'list', addLabel: 'Type', item: { type: 'text', placeholder: 'text/html' }, hint: 'defaults to text/html; "*" (YAML) matches any' },
    { key: 'min_length', label: 'Min length (bytes)', type: 'number', default: 20 },
    { key: 'comp_level', label: 'Quality', type: 'number', default: 6, hint: '0-11' },
    { key: 'vary', label: 'Vary', type: 'switch', switchLabel: 'Add Vary: Accept-Encoding', default: false },
  ],
  'exit-transformer': [
    { key: 'body', label: 'Body template', type: 'textarea', rows: 4, placeholder: '{"status": $status, "path": "$uri"}', hint: '$var interpolation; $status is the remapped status. status_map is YAML-only.' },
    { key: 'always', label: 'Scope', type: 'switch', switchLabel: 'Apply to all responses, not just gateway exits', default: false },
  ],
  'fault-injection': [
    { key: 'abort', label: 'Abort', type: 'textarea', rows: 6, placeholder: '{"http_status": 503, "body": "injected", "percentage": 10}', hint: 'JSON object: http_status (required), body, headers, percentage 0-100, vars' },
    { key: 'delay', label: 'Delay', type: 'textarea', rows: 4, placeholder: '{"duration": 0.5, "percentage": 30}', hint: 'JSON object: duration seconds (required), percentage, vars' },
  ],
  workflow: [
    { key: 'rules', label: 'Rules', type: 'textarea', rows: 10, placeholder: '[{"case": [["uri", "~~", "^/admin"]], "actions": [["return", {"code": 403}]]}]', hint: 'JSON array; actions: ["return", {code}] or ["limit-count", {count, time_window, key, rejected_code}]' },
  ],
  'traffic-label': [
    { key: 'rules', label: 'Rules', type: 'textarea', rows: 10, placeholder: '[{"match": [["arg_channel", "==", "beta"]], "actions": [{"set_headers": {"x-server-id": "beta"}, "set_labels": {"tier": "beta"}}]}]', hint: 'JSON array; set_headers → request headers, set_labels → message label.<key>' },
  ],
  mocking: [
    { key: 'response_status', label: 'Status', type: 'number', default: 200 },
    { key: 'content_type', label: 'Content type', type: 'select', options: ['application/json;charset=utf8', 'application/json', 'text/plain', 'text/html', 'application/xml', 'text/xml'], default: 'application/json;charset=utf8' },
    { key: 'response_example', label: 'Response body', type: 'textarea', rows: 6, placeholder: '{"user": "$arg_name"}', hint: 'required; supports $var interpolation' },
    { key: 'response_headers', label: 'Response headers', type: 'objects', addLabel: 'Header', itemLabel: 'Header',
      fields: [
        { key: 'name', label: 'Name', type: 'text' },
        { key: 'value', label: 'Value', type: 'text' },
      ] },
    { key: 'with_mock_header', label: 'Mock header', type: 'switch', switchLabel: 'Add x-mock-by header', default: true },
    { key: 'delay', label: 'Delay (s)', type: 'number', default: 0 },
  ],
  'data-mask': [
    { key: 'request', label: 'Masking rules', type: 'objects', addLabel: 'Rule', itemLabel: 'Rule',
      fields: [
        { key: 'type', label: 'Target', type: 'select', options: ['query', 'header', 'body'], default: 'query' },
        { key: 'name', label: 'Name / body path', type: 'text', placeholder: 'user.card_number', hint: 'dotted path for body rules; numeric segments index arrays' },
        { key: 'action', label: 'Action', type: 'select', options: ['remove', 'replace', 'regex'], default: 'remove' },
        { key: 'regex', label: 'Regex', type: 'text', placeholder: '^(\\d{4})\\d+(\\d{4})$', hint: 'required for action regex' },
        { key: 'value', label: 'Value', type: 'text', placeholder: '***', hint: 'required for replace/regex; $1 captures allowed' },
        { key: 'body_format', label: 'Body format', type: 'select', options: ['json'], default: 'json', hint: 'required for body rules' },
      ] },
    { key: 'max_body_size', label: 'Max body size (bytes)', type: 'number', default: 1048576, hint: 'larger bodies skip body rules' },
  ],
  'request-validation': [
    { key: 'header_schema', label: 'Header schema (JSON)', type: 'textarea', rows: 6, placeholder: '{"type":"object","required":["x-api-version"]}', hint: 'JSON Schema; headers seen as {name: first_value}, lowercase names' },
    { key: 'body_schema', label: 'Body schema (JSON)', type: 'textarea', rows: 8, placeholder: '{"type":"object","required":["name"]}', hint: 'JSON Schema; at least one schema is required' },
    { key: 'rejected_code', label: 'Rejected status', type: 'number', default: 400 },
    { key: 'rejected_msg', label: 'Rejected message', type: 'text', placeholder: 'invalid payload', hint: 'fixed message instead of validator detail' },
  ],
  'body-transformer': [
    { key: 'request', label: 'Request transform', type: 'objects', addLabel: 'Transform', itemLabel: 'Request',
      fields: [
        { key: 'template', label: 'Template', type: 'textarea', rows: 5, placeholder: '{"name":"{{body.user.name}}","trace":"{{$http_x_request_id}}"}' },
        { key: 'input_format', label: 'Input format', type: 'select', options: ['json'], default: 'json' },
      ] },
    { key: 'response', label: 'Response transform', type: 'objects', addLabel: 'Transform', itemLabel: 'Response',
      fields: [
        { key: 'template', label: 'Template', type: 'textarea', rows: 5, placeholder: '{"status":"{{body.result}}","code":$status}' },
        { key: 'input_format', label: 'Input format', type: 'select', options: ['json'], default: 'json' },
      ] },
  ],
  degraphql: [
    { key: 'query', label: 'GraphQL query', type: 'textarea', rows: 6, placeholder: 'query ($name: String!) { persons(filter: { name: $name }) { id name } }', hint: 'required; sent upstream as the query document' },
    { key: 'variables', label: 'Variables', type: 'list', addLabel: 'Variable', item: { type: 'text', placeholder: 'name' }, hint: 'resolved from query params first, then JSON body fields' },
    { key: 'operation_name', label: 'Operation name', type: 'text', placeholder: 'ListPersons', hint: 'for multi-operation documents' },
  ],
};

/**
 * Looks up the form schema for a plugin type.
 *
 * @param type - Plugin type name (a {@link pluginConfig} key).
 * @returns The field list, or an empty array for config-less or unknown
 * types (e.g. `listener`, `client`).
 */
export function getPluginConfigSchema(type: string): FieldSchema[] {
  return pluginConfig[type] || [];
}
