# Env-only field classification audit

Scope: every text-like field in `ui/src/pluginConfig.ts` that is effectively
`template: 'env-only'` (explicit, or defaulted because `text`/`textarea`/list-
item/objects-subfield carries no `template` at all). Fields already
`template: 'full'` or `template: 'none'` are out of scope and not listed.

Method: each field's Rust plugin (`src/plugins/native/<name>.rs`) was read
(`from_config` + `execute`) to determine whether the value is used as a
plain string at request time (**GROUP 1** — sweepable to `template: 'full'`)
or compiled/parsed once at load into a non-string form, or is genuine crypto
key material (**GROUP 2** — must stay env-only), or is ambiguous/security-
sensitive enough to need a human call (**BORDERLINE**).

Totals: **183** env-only fields — **103 GROUP 1**, **40 GROUP 2**, **40 BORDERLINE**.

Two evidence-based corrections were made during synthesis, overriding an initial
agent verdict, both noted inline below:
- `elasticsearch-logger.auth.username` / `auth.password` → moved **GROUP 1 → GROUP 2**
  (the Basic-auth string is built once in `from_config`, not re-derived per flush —
  same pattern as `openwhisk.service_token`, which the source explicitly documents
  as "never templated — a credential").
- `sls-logger.access_key_secret` / `tencent-cloud-cls.secret_key` → moved
  **GROUP 1 → BORDERLINE** (HMAC signing-key material, same risk class as
  `hmac-auth.secret_key`, which is GROUP 2, even though it is recomputed per
  flush rather than at load).

Note on SSRF-flagged fields: per the audit's own borderline rule ("an env-only
URL fetched per request as a string is GROUP 1, but flag it with an SSRF note"),
logger/tracer destination URLs (http-logger.uri, loki-logger endpoints, otel/
zipkin/skywalking endpoints, etc.) are filed under GROUP 1 with an inline SSRF/
exfil note, not under BORDERLINE — sweeping them is in scope, but the note
should travel with the change.

---

## GROUP 1 — sweepable (103 fields)

### proxy-rewrite / auth / consumer / acl
- `proxy-rewrite.add_headers[].name` — literal `HashMap` insert key each request — `src/plugins/native/proxy_rewrite.rs:205-227`
- `error-handler.body_template` — already string-substituted per request against live error fields (bespoke replace, not `Template`) — `src/plugins/native/error_handler.rs:69-76`
- `rate-limit.key_from` — header name is a plain lookup key every request — `src/plugins/native/rate_limit.rs:111-146`
- `key-auth.header_name` — plain `HashMap` lookup key each request — `src/plugins/native/key_auth.rs:168-172`
- `basic-auth.users[].username` — `self.users.get(username)` lookup key each request — `src/plugins/native/basic_auth.rs:185-186`
- `basic-auth.anonymous_consumer` — `store.get(name)` lookup key each request — `src/plugins/native/basic_auth.rs:234-236`
- `jwt-auth.header_name` — plain `HashMap` lookup key each request — `src/plugins/native/jwt_auth.rs:208-212`
- `hmac-auth.access_key` — plain identifier compared (`==ak`) per request, not secret material — `src/plugins/native/hmac_auth.rs:548-549`
- `hmac-auth.signed_headers[]` — header names looked up per request while building the signing string — `src/plugins/native/hmac_auth.rs:445-458`
- `hmac-auth.anonymous_consumer` — `store.get(name)` lookup key each request — `src/plugins/native/hmac_auth.rs:596-599`
- `jwe-decrypt.header` — plain `HashMap` lookup key each request — `src/plugins/native/jwe_decrypt.rs:283-287`
- `jwe-decrypt.forward_header` — literal insertion key on `headers.insert` each request — `src/plugins/native/jwe_decrypt.rs:371-373`
- `consumer-restriction.whitelist[]` — `.contains(&value)` each request — `src/plugins/native/consumer_restriction.rs:264`
- `consumer-restriction.blacklist[]` — `.contains(&value)` each request — `src/plugins/native/consumer_restriction.rs:256`
- `consumer-restriction.allowed_by_methods[].user` — string-equality compare per request — `src/plugins/native/consumer_restriction.rs:274`
- `consumer-restriction.allowed_by_methods[].methods[]` — `.contains(&method)` per request — `src/plugins/native/consumer_restriction.rs:276`
- `acl.allowed_by[]` — `.contains(g)` per request — `src/plugins/native/acl.rs:169`
- `acl.denied_by[]` — `.contains(g)` per request — `src/plugins/native/acl.rs:161`
- `authz-casbin.username_header` — lowercased once, used as header-map key per request — `src/plugins/native/authz_casbin.rs:167`
- `authz-keycloak.client_id` — sent as `audience` in per-request form body — `src/plugins/native/authz_keycloak.rs:297`
- `authz-keycloak.permissions[]` — serialized into per-request form body — `src/plugins/native/authz_keycloak.rs:296`
- `authz-casdoor.scope` — interpolated into authorize URL per request — `src/plugins/native/authz_casdoor.rs:474`
- `authz-casdoor.session_cookie_name` — per-request cookie name for read/write — `src/plugins/native/authz_casdoor.rs:308`
- `authz-casdoor.logout_path` — `==` compare to request path per request — `src/plugins/native/authz_casdoor.rs:377`
- `wolf-rbac.appid` — percent-encoded into query string per request — `src/plugins/native/wolf_rbac.rs:277`
- `wolf-rbac.header_prefix` — concatenated into response header names per request — `src/plugins/native/wolf_rbac.rs:316`
- `cas-auth.service` — plain string returned/encoded per request, no load parsing — `src/plugins/native/cas_auth.rs:237`
- `cas-auth.ticket_param` — query-param lookup key per request — `src/plugins/native/cas_auth.rs:336`
- `cas-auth.session_cookie_name` — per-request cookie name — `src/plugins/native/cas_auth.rs:277`
- `cas-auth.session_cookie_path` — per-request cookie attribute (no load-time path_covers tie for this plugin) — `src/plugins/native/cas_auth.rs:249`
- `cas-auth.logout_path` — `==` compare to request path — `src/plugins/native/cas_auth.rs:319`
- `openid-connect.client_id` — value in per-request introspection/token bodies — `src/plugins/native/openid_connect.rs:452`
- `openid-connect.session_cookie_name` — per-request cookie name — `src/plugins/native/openid_connect.rs:632`
- `openid-connect.scope` — interpolated into authorize URL per request — `src/plugins/native/openid_connect.rs:681`
- `openid-connect.logout_path` — `==` compare to request path — `src/plugins/native/openid_connect.rs:582`
- `dingtalk-auth.app_key` — JSON field in access-token request body — `src/plugins/native/dingtalk_auth.rs:198`
- `dingtalk-auth.code_header` — lowercased once, header-map key per request — `src/plugins/native/dingtalk_auth.rs:169`
- `dingtalk-auth.code_query` — query-param key per request — `src/plugins/native/dingtalk_auth.rs:178`
- `feishu-auth.app_id` — `client_id` field in per-request token body — `src/plugins/native/feishu_auth.rs:200`
- `feishu-auth.auth_redirect_uri` — `redirect_uri` field, forwarded verbatim, no load parsing — `src/plugins/native/feishu_auth.rs:202`
- `feishu-auth.code_header` — header-map key per request — `src/plugins/native/feishu_auth.rs:156`
- `feishu-auth.code_query` — query-param key per request — `src/plugins/native/feishu_auth.rs:168`
- `forward-auth.client_headers[]` — plain lookup-key list for mirroring deny-response headers — `src/plugins/native/forward_auth.rs:301`

### FaaS
- `aws-lambda.authorization.apikey` — literal `x-api-key` header value, set fresh per `execute()` — `src/plugins/native/aws_lambda.rs:227-236,312`
- `aws-lambda.authorization.iam.aws_region` — plain string concatenated into `credential_scope`, recomputed per request — `src/plugins/native/aws_lambda.rs:260,437-439`
- `aws-lambda.authorization.iam.aws_service` — same, concatenated into `credential_scope` per request — `src/plugins/native/aws_lambda.rs:261,437-439`
- `azure-functions.authorization.apikey` — literal `x-functions-key` header, pushed per request — `src/plugins/native/azure_functions.rs:127-129,164`
- `azure-functions.authorization.clientid` — literal `x-functions-clientid` header, pushed per request — `src/plugins/native/azure_functions.rs:130-132`

### Tracing (SSRF note: endpoint fields below are raw strings spliced into a POST URL fresh per span/request, not baked into a persistent client)
- `opentelemetry.endpoint` — **[SSRF]** raw `String` used directly in per-request POST URL — `src/plugins/native/opentelemetry.rs:189-196,275`
- `opentelemetry.service_name` — literal string inserted as `service.name` attribute per request — `src/plugins/native/opentelemetry.rs:267,322`
- `zipkin.endpoint` — **[SSRF]** raw `String`, POST url per request — `src/plugins/native/zipkin.rs:139-144,254`
- `zipkin.service_name` — literal string in per-request span JSON — `src/plugins/native/zipkin.rs:246,283`
- `zipkin.server_addr` — literal string, passed straight into `build_zipkin(...)` and written as `localEndpoint.ipv4` in the per-request payload — `src/plugins/native/zipkin.rs:246,277-284`
- `skywalking.endpoint_addr` — **[SSRF]** raw `String` used in per-request segment POST url — `src/plugins/native/skywalking.rs:188-194,299`
- `skywalking.service_name` — literal string in per-request segment JSON + `sw8` header — `src/plugins/native/skywalking.rs:271-294`
- `skywalking.service_instance_name` — same — `src/plugins/native/skywalking.rs:271-294`

### Loggers (SSRF note: none of these build a dedicated per-endpoint client; all HTTP loggers share the process-wide generic `OutboundClient`, `src/outbound/mod.rs:88-120`, and rebuild the URL/request fresh per flush — so destination fields are GROUP 1 with an SSRF/exfil flag, not baked-in)
- `http-logger.uri` — **[SSRF]** cloned into a fresh `OutboundRequest.url` every flush — `src/plugins/native/http_logger.rs:76`
- `http-logger.auth_header` — folded into headers at construction, cloned fresh per flush — `src/plugins/native/http_logger.rs:71-77,184-188`
- `loki-logger.endpoint_addrs[]` — **[SSRF]** one picked per flush and set as `OutboundRequest.url` — `src/plugins/native/loki_logger.rs:66-77`
- `loki-logger.endpoint_uri` — path suffix only, appended at construction, doesn't control destination host — `src/plugins/native/loki_logger.rs:71,165-168`
- `loki-logger.tenant_id` — pushed as `X-Scope-OrgID` header fresh each flush — `src/plugins/native/loki_logger.rs:74`
- `splunk-hec-logging.endpoint.uri` — **[SSRF]** `self.uri.clone()` set as URL every flush — `src/plugins/native/splunk_hec_logging.rs:85`
- `splunk-hec-logging.endpoint.token` — formatted into `Authorization: Splunk <token>` header fresh each flush — `src/plugins/native/splunk_hec_logging.rs:75-77`
- `splunk-hec-logging.endpoint.channel` — pushed as `X-Splunk-Request-Channel` header fresh each flush — `src/plugins/native/splunk_hec_logging.rs:79-81`
- `splunk-hec-logging.source` — literal `"source"` field in the HEC event envelope, built fresh every flush via `build_splunk_body` — `src/plugins/native/splunk_hec_logging.rs:32-46,70`
- `datadog.host` — **[net-dest]** new `UdpSocket` opened and `.connect()`'d fresh on every single flush (no persistent socket) — `src/plugins/native/datadog.rs:103-110,190`
- `datadog.namespace` — passed into `build_metric_lines` fresh each flush — `src/plugins/native/datadog.rs:113-119`
- `datadog.constant_tags[]` — passed into `build_metric_lines` fresh each flush — `src/plugins/native/datadog.rs:116`
- `loggly.customer_token` — baked into the stored `url` string at construction, only affects the URL path segment (not destination host) — `src/plugins/native/loggly.rs:38-49,81`
- `loggly.tags[]` — feeds `tags_header`, cloned into `X-LOGGLY-TAG` fresh each flush — `src/plugins/native/loggly.rs:77`
- `loggly.host` — **[SSRF]** determines scheme+host of the stored `url`, cloned into request every flush — `src/plugins/native/loggly.rs:38-43,81`
- `elasticsearch-logger.endpoint_addr` — **[SSRF]** one of `endpoints` chosen round-robin, formatted into `url` fresh each flush — `src/plugins/native/elasticsearch_logger.rs:212-213`
- `elasticsearch-logger.field.index` — passed into `build_bulk_body` fresh each flush — `src/plugins/native/elasticsearch_logger.rs:214`
- `clickhouse-logger.endpoint_addr` — **[SSRF]** round-robin `endpoints[idx]` used directly as `url` fresh each flush — `src/plugins/native/clickhouse_logger.rs:191-192`
- `clickhouse-logger.database` — sent as `X-ClickHouse-Database` header fresh each flush — `src/plugins/native/clickhouse_logger.rs:199`
- `clickhouse-logger.user` — sent as `X-ClickHouse-User` header fresh each flush — `src/plugins/native/clickhouse_logger.rs:197`
- `clickhouse-logger.password` — sent as `X-ClickHouse-Key` header fresh each flush — `src/plugins/native/clickhouse_logger.rs:198`
- `sls-logger.host` — **[SSRF]** URL authority rebuilt fresh via `format!(...)` every flush over the shared client — `src/plugins/native/sls_logger.rs:131,413-416`
- `sls-logger.project` — **[SSRF]** same URL-authority construction, rebuilt fresh per flush — `src/plugins/native/sls_logger.rs:131,413-416`
- `sls-logger.logstore` — used in canonical_resource (signing input) and URL path, both recomputed fresh each flush — `src/plugins/native/sls_logger.rs:213-214,404-416`
- `sls-logger.access_key_id` — plain identifier (not secret) used to build fresh `Authorization` header per flush — `src/plugins/native/sls_logger.rs:229-236,404-410`
- `tencent-cloud-cls.cls_host` — **[SSRF]** url built fresh each flush via `format!(...)` — `src/plugins/native/tencent_cloud_cls.rs:271-274`
- `tencent-cloud-cls.cls_topic` — appended as query param, rebuilt each flush — `src/plugins/native/tencent_cloud_cls.rs:271-274`
- `tencent-cloud-cls.secret_id` — plain identifier passed to `sign()` fresh every flush — `src/plugins/native/tencent_cloud_cls.rs:214-235,270`
- `tcp-logger.host` — **[SSRF/exfil]** brand-new `TcpStream::connect(&addr)` opened every flush, not a pinned socket — `src/plugins/native/tcp_logger.rs:56-61`
- `udp-logger.host` — **[SSRF/exfil]** fresh ephemeral `UdpSocket` bound and sent per flush — `src/plugins/native/udp_logger.rs:49-54`
- `syslog.host` — **[SSRF/exfil]** new `TcpStream`/`UdpSocket` opened per flush depending on `sock_type` — `src/plugins/native/syslog.rs:132-158`
- `file-logger.path` — **[arbitrary local write note]** file opened fresh (`OpenOptions::open`) every flush, no held handle — `src/plugins/native/file_logger.rs:53-56`
- `error-log-logger.host` — **[SSRF/exfil]** same fresh-TcpStream-per-flush pattern as tcp-logger — `src/plugins/native/error_log_logger.rs:63-72`
- `google-cloud-logging.log_id` — baked once into `log_name` string but that string is plain payload data (not a connection), reused unchanged per flush — `src/plugins/native/google_cloud_logging.rs:158,278-300`
- `skywalking-logger.endpoint_addr` — **[SSRF]** precomputed `self.url` reused as the `url` of a fresh `OutboundRequest` every flush over the shared generic client (no dedicated bound connection) — `src/plugins/native/skywalking_logger.rs:116,171-179`
- `skywalking-logger.service_name` — plain field consumed fresh per request in `build_log_item` — `src/plugins/native/skywalking_logger.rs:205-219`
- `skywalking-logger.service_instance_name` — same — `src/plugins/native/skywalking_logger.rs:205-219`
- `lago.endpoint` — **[SSRF]** precomputed `self.url`, reused unchanged in a fresh `OutboundRequest` each flush via the shared client — `src/plugins/native/lago.rs:138,202-216`
- `lago.token` — builds `Authorization: Bearer` header fresh each flush call — `src/plugins/native/lago.rs:207-211`
- `lago.event_code` — plain string used directly per-request in `build_event` — `src/plugins/native/lago.rs:242-257`

### Traffic control & misc
- `limit-count.group` — plain string concatenated into the counter key every request (`format!("{}:{}", group, key)`), no compile step — `src/plugins/native/limit_count.rs:126-130,179-182`
- `proxy-cache.cache_method[]` — parsed into `Vec<String>` (uppercased) once, checked with plain `.contains(&method)` per request; no regex — `src/plugins/native/proxy_cache.rs:200-218,237-241`
- `referer-restriction.whitelist[]` — no regex; pre-split into exact/suffix strings once, `==`/`ends_with` per request — cheap, no ReDoS/compile risk — `src/plugins/native/referer_restriction.rs:58-89,49-53`
- `referer-restriction.blacklist[]` — same — `src/plugins/native/referer_restriction.rs:58-89,49-53`
- `echo.headers[].name` — inserted as literal response-header key per request; only the value is currently `Template` — `src/plugins/native/echo.rs:213-215,144-147`
- `gzip.types[]` — plain string equality against response content-type at request time, no compilation step — `src/plugins/native/gzip.rs:76-92`
- `brotli.types[]` — same `ContentTypes::matches` reused from gzip.rs, string comparison at request time — `src/plugins/native/brotli.rs:22`, `src/plugins/native/gzip.rs:89`
- `mocking.response_headers[].name` — literal header-name key used in `ctx.response.headers.insert` per request — `src/plugins/native/mocking.rs:260-262`

---

## GROUP 2 — structural, stays env-only (40 fields)

- `jwt-auth.secret` — HMAC key material fed to `DecodingKey::from_secret` — `src/plugins/native/jwt_auth.rs:164,226`
- `hmac-auth.secret_key` — HMAC key bytes rebuilt via `hmac::Key::new` in `verify_signature`; crypto secret, must stay static — `src/plugins/native/hmac_auth.rs:461-468,550`
- `multi-auth.auth_plugins[]` — sub-plugins instantiated once via `create_plugin` at `from_config` — `src/plugins/native/multi_auth.rs:74-96`
- `jwe-decrypt.key` — base64-decoded once at `from_config` into a 32-byte AES key (`Vec<u8>`) — `src/plugins/native/jwe_decrypt.rs:180-193`
- `authz-casbin.model_path` — `DefaultModel::from_file` parsed once at load — `src/plugins/native/authz_casbin.rs:213`
- `authz-casbin.policy_path` — `FileAdapter::new` built once at load — `src/plugins/native/authz_casbin.rs:216`
- `authz-casbin.model` — `DefaultModel::from_str` parsed once at load — `src/plugins/native/authz_casbin.rs:222`
- `authz-casbin.policy` — `StringAdapter::new` built once at load — `src/plugins/native/authz_casbin.rs:225`
- `serverless-pre-function.functions[]` — compiled once into a `LuaRuntime`/`ServerlessRunner` at load; `execute()` reuses it — `src/plugins/native/serverless_pre_function.rs:97-116`
- `serverless-pre-function.phase` — parsed but entirely unused/inert — `src/plugins/native/serverless_pre_function.rs:53-54`
- `serverless-post-function.functions[]` — same compile-once path via `ServerlessRunner::from_config` — `src/plugins/native/serverless_post_function.rs:44`
- `serverless-post-function.phase` — inert, unused — `src/plugins/native/serverless_post_function.rs:34`
- `oas-validator.spec` — walked once into `Vec<CompiledOp>` (path segments + `jsonschema::Validator`) at `from_config` — `src/plugins/native/oas_validator.rs:271-348`
- `aws-lambda.authorization.iam.accesskey` — SigV4 credential id feeding the HMAC signing chain — `src/plugins/native/aws_lambda.rs:258,472-479`
- `aws-lambda.authorization.iam.secretkey` — SigV4 signing-key seed via HMAC chain — `src/plugins/native/aws_lambda.rs:258,472-479`
- `openwhisk.service_token` — base64-encoded once at load into `self.authorization`; source comment: "Never templated — a credential" — `src/plugins/native/openwhisk.rs:61,136,220`
- `openfunction.authorization.service_token` — base64-encoded once at load into `self.authorization`, reused verbatim — `src/plugins/native/openfunction.rs:83,111-114`
- `authz-casdoor.callback_url` — path parsed once into `callback_path`, validated once against cookie_path — `src/plugins/native/authz_casdoor.rs:186,211`
- `authz-casdoor.session_secret` — builds a `CookieSealer` once at load — `src/plugins/native/authz_casdoor.rs:189`
- `cas-auth.session_secret` — builds a `CookieSealer` once at load — `src/plugins/native/cas_auth.rs:155`
- `openid-connect.token_signing_alg_values_expected` — parsed once into `Vec<Algorithm>` — `src/plugins/native/openid_connect.rs:274`
- `openid-connect.redirect_uri` — path parsed once into `redirect_path`, validated via `path_covers` at load — `src/plugins/native/openid_connect.rs:892,913`
- `openid-connect.session_secret` — builds a `CookieSealer` once at load — `src/plugins/native/openid_connect.rs:886`
- `traffic-split.rules[].match` — compiled once via `Expr::parse` in `from_config`; `execute()` only calls `.eval(ctx)` — `src/plugins/native/traffic_split.rs:165-168,239-243`
- `traffic-split.rules[].weighted_upstreams[].upstream` — parsed once into `Vec<Target>` via `parse_targets` in `from_config` — `src/plugins/native/traffic_split.rs:193-199,338-341,379`
- `real-ip.trusted_addresses[]` — each entry parsed once into `IpNet`/`IpAddr`; `execute()` does `.contains()` on the typed value — `src/plugins/native/real_ip.rs:107-133,149-153`
- `ua-restriction.allowlist[]` — `Regex::new` compiled once per entry at `from_config`; `execute()` uses the compiled `Vec<Regex>` — `src/plugins/native/ua_restriction.rs:41-61,184-193`
- `ua-restriction.denylist[]` — same — `src/plugins/native/ua_restriction.rs:41-61,184-193`
- `uri-blocker.block_rules[]` — `Regex::new` compiled once per rule at `from_config`; `execute()` uses compiled regexes — `src/plugins/native/uri_blocker.rs:72-89,133`
- `csrf.key` — compiled once into `hmac::Key` at `from_config`; also a secret, so even mechanically-safe re-rendering is undesirable — `src/plugins/native/csrf.rs:148,156-159,205-210`
- `script.source` — file path read once via `std::fs::read_to_string` in `from_config`, then compiled once into a `LuaRuntime` — `src/plugins/script/mod.rs:75-91,113-115`, `src/plugins/script/lua_runtime.rs:36-58`
- `script.inline` — compiled/validated once in `LuaRuntime::new`; never re-rendered against request-time templates — `src/plugins/script/mod.rs:80-94`, `src/plugins/script/lua_runtime.rs:19-21,42-46,70-74`
- `response-rewrite.filters[].regex` — `Regex::new` compiled once in `parse_filter` at `from_config`; `execute()` only calls `.replace`/`.replace_all` — `src/plugins/native/response_rewrite.rs:230,451,453`
- `fault-injection.delay` — despite the textarea UI, `duration`/`percentage`/`vars` are pulled out of the JSON once at `from_config` into a non-string `DelayRule` struct — `src/plugins/native/fault_injection.rs:272-294,315-319`
- `data-mask.request[].regex` — `Regex::new` called once in `parse_rule` at `from_config`; only `regex.is_match`/`regex.replace` used per request — `src/plugins/native/data_mask.rs:346-347,202-208`
- `request-validation.header_schema` — `jsonschema::validator_for` compiled once in `compile_schema` at `from_config` — `src/plugins/native/request_validation.rs:42-59,236`
- `request-validation.body_schema` — same `compile_schema` path — `src/plugins/native/request_validation.rs:151-152,271`
- `google-cloud-logging.auth_file` — read once via `std::fs::read_to_string` in `resolve_auth()`, called once from `from_config`; parsed client_email/private_key/project_id reused for the plugin's life — `src/plugins/native/google_cloud_logging.rs:206-216`
- `elasticsearch-logger.auth.username` / `auth.password` — **[synthesis override, see note above]** combined once into a Basic-auth string at `from_config`/construction (`elasticsearch_logger.rs:121-131`), reused verbatim as a header value across every flush; never re-derived per request — `src/plugins/native/elasticsearch_logger.rs:121-131,226-227`

---

## BORDERLINE — needs a human decision (40 fields)

Grouped by why they're borderline:

**"Never templated by design" per the plugin's own doc comment** (mechanically GROUP 1, but the source explicitly says otherwise):
- `proxy-rewrite.strip_path_prefix` — plain `starts_with` match at request time, but `proxy_rewrite.rs:18-20` documents it as "a matcher, never templated, by design" — `src/plugins/native/proxy_rewrite.rs:187-196`
- `proxy-rewrite.remove_headers[]` — `remove_ci` compare per request, same "never templated" doc note — `src/plugins/native/proxy_rewrite.rs:18-20,214-217`
- `cors.allowed_origins[]` — plain `==` compare per request, but it's a CORS allowlist; live templating = bypass risk — `src/plugins/native/cors.rs:26,134-136`

**Access-control / auth-bypass risk if made request-templatable** (mechanically per-request string use, but the string IS the security boundary):
- `upstream.targets[].host` — the actual proxied backend destination; per-request host templating is SSRF against your own upstream selection, more severe than a logger sink — `src/plugins/native/upstream.rs:261-264,138`
- `ip-restriction.allow[]` — re-parsed to `IpAddr`/CIDR every call, so mechanically GROUP 1, but it's an access-control boundary — `src/plugins/native/ip_restriction.rs:72-110,127`
- `ip-restriction.deny[]` — same — `src/plugins/native/ip_restriction.rs:72-110,127`
- `key-auth.keys[]` — `valid_keys.contains(k)` fits GROUP 1 mechanically, but this list IS the accepted-credential set; templating from request context is auth bypass — `src/plugins/native/key_auth.rs:187-188`
- `basic-auth.users[].password` — `expected==password` compare fits GROUP 1 mechanically, but it's a stored secret — `src/plugins/native/basic_auth.rs:186-188`

**Auth-authority endpoints/identities** (callout URL or identity value that decides trust, fetched/read live per request):
- `authz-keycloak.token_endpoint` — UMA decision-authority URL, fetched per request — `src/plugins/native/authz_keycloak.rs:301`
- `authz-casdoor.endpoint_addr` — reused verbatim in introspect/authorize/token URLs each request; auth authority — `src/plugins/native/authz_casdoor.rs:340,586`
- `authz-casdoor.client_id` — baked into a load-time Basic-auth string AND read live per request; templating would desync the two — `src/plugins/native/authz_casdoor.rs:229,601`
- `authz-casdoor.client_secret` — secret; baked into load-time Basic-auth AND used live in the code-exchange body — `src/plugins/native/authz_casdoor.rs:229,347`
- `authz-casdoor.session_cookie_path` — per-request cookie Path, but tied at load time to `callback_path` via `path_covers` — `src/plugins/native/authz_casdoor.rs:211`
- `ldap-auth.ldap_uri` — new LDAP connection built per request from this URI; auth authority — `src/plugins/native/ldap_auth.rs:175`
- `ldap-auth.base_dn` — concatenated into the bind DN per request; defines the auth scope — `src/plugins/native/ldap_auth.rs:248`
- `ldap-auth.uid` — same bind-DN construction; changes which attribute is used for identity — `src/plugins/native/ldap_auth.rs:248`
- `wolf-rbac.server` — builds the RBAC access-check URL per request — `src/plugins/native/wolf_rbac.rs:277`
- `cas-auth.idp_uri` — builds serviceValidate/login URLs per request; CAS authority — `src/plugins/native/cas_auth.rs:286,352`
- `openid-connect.discovery` — fetch target for JWKS/endpoint resolution; trust-anchor source — `src/plugins/native/openid_connect.rs:350`
- `openid-connect.jwks_uri` — JWT-verification trust anchor, fetched live — `src/plugins/native/openid_connect.rs:337`
- `openid-connect.introspection_endpoint` — introspection callout URL — `src/plugins/native/openid_connect.rs:448`
- `openid-connect.client_secret` — secret used in per-request Basic-auth encoding for token/introspection calls — `src/plugins/native/openid_connect.rs:453`
- `openid-connect.session_cookie_path` — per-request cookie Path, but tied at load time to `redirect_path` via `path_covers` — `src/plugins/native/openid_connect.rs:913`
- `dingtalk-auth.app_secret` — secret in the access-token request body — `src/plugins/native/dingtalk_auth.rs:199`
- `dingtalk-auth.access_token_url` — token-mint authority URL — `src/plugins/native/dingtalk_auth.rs:203`
- `dingtalk-auth.userinfo_url` — identity-resolution authority URL — `src/plugins/native/dingtalk_auth.rs:227`
- `feishu-auth.app_secret` — secret in the per-request token body — `src/plugins/native/feishu_auth.rs:201`
- `feishu-auth.access_token_url` — token-exchange authority URL — `src/plugins/native/feishu_auth.rs:180`
- `feishu-auth.userinfo_url` — identity-resolution authority URL — `src/plugins/native/feishu_auth.rs:211`
- `opa.host` — policy-decision callout authority URL — `src/plugins/native/opa.rs:417`
- `opa.policy` — selects which policy is evaluated; bypass risk if request-templated — `src/plugins/native/opa.rs:417`
- `forward-auth.uri` — authorization-callout authority URL — `src/plugins/native/forward_auth.rs:353`

**Cross-node pairing / unbounded-state risk** (two separately configured nodes must resolve to the identical key; registries never evict by key):
- `api-breaker.id` — `resources.traffic.breakers.breaker(&self.id)` lookup is GROUP-1-shaped, but check/observe nodes must share the value, and the breaker registry is an unbounded `DashMap` with no TTL/removal — templating risks desync + unbounded entries (DoS) — `src/plugins/native/api_breaker.rs:174-179,265`; `src/traffic/mod.rs:108-119`
- `proxy-cache.id` — same cross-node-pairing pattern (lookup/store must derive an identical key); cache registry only evicts lazily on read of the same key — `src/plugins/native/proxy_cache.rs:148-153,248-256`; `src/traffic/mod.rs:135-150`

**Dual-use field (behavior depends on a sibling field)**:
- `data-mask.request[].name` — for `type: query/header` it's a literal per-request map key (GROUP 1); for `type: body` the same field is parsed once at load into `Vec<PathSeg>` (GROUP 2) — one config field, two behaviors gated by the `type` selector — `src/plugins/native/data_mask.rs:213-239,357-361,133-198`

**Different vulnerability class than SSRF (flag explicitly, don't fold into the generic SSRF note)**:
- `clickhouse-logger.logtable` — interpolated raw into `INSERT INTO <logtable>` SQL text fresh every flush — SQL-injection surface if made request-templatable, not just SSRF — `src/plugins/native/clickhouse_logger.rs:177-186,193`
- `file-logger.path` is filed under GROUP 1 above with an arbitrary-local-write note; if reviewers want stricter treatment it belongs here instead — flagging for visibility.

**Crypto/signing secret recomputed per-request rather than at load** (same risk class as `hmac-auth.secret_key`, which is GROUP 2, but classified once mechanically as "just a per-request string use" — synthesis override, see top-of-file note):
- `sls-logger.access_key_secret` — HMAC-signed fresh in `sign_request()` every flush call; still credential/signing material — `src/plugins/native/sls_logger.rs:219-222,404-410`
- `tencent-cloud-cls.secret_key` — same, HMAC-signed fresh each flush — `src/plugins/native/tencent_cloud_cls.rs:214-235,270`

**Out of scope for the full/env-only mechanism entirely** (already rendered per request by a bespoke engine, independent of `crate::vars::template::Template`):
- `body-transformer.request[].template` — never passed through `Template` at all; stored as a raw `String` and rendered per request by body-transformer's own engine (`render()`, mixing `{{body.path}}` and `{{$var}}`/`$var`) — the UI `template` attribute has no real effect on this plugin's behavior either way — `src/plugins/native/body_transformer.rs:56-105,138-177,258`
- `body-transformer.response[].template` — identical reasoning — `src/plugins/native/body_transformer.rs:289`
