//! AWS Lambda serverless-upstream plugin (`aws-lambda`).
//!
//! Port of APISIX's `aws-lambda` plugin (3.17). Invokes an AWS Lambda function
//! through its function URL (or an API Gateway endpoint) and returns the
//! function's reply as the gateway response — it **replaces the upstream**, so
//! the node's `success` port should be wired straight to `client.in`.
//!
//! Two authorization modes are supported:
//!
//! - **API key** (`authorization.apikey`) — the key is sent as the `x-api-key`
//!   request header, no signing.
//! - **IAM / SigV4** (`authorization.iam`) — the request is signed with AWS
//!   Signature Version 4 (`AWS4-HMAC-SHA256`). The signing itself is factored
//!   into the pure [`sign_v4`] function, unit-tested against the published AWS
//!   SigV4 `get-vanilla` test vector.
//!
//! On a callout failure the node rejects through its `error` port with
//! `AWS_LAMBDA_CALLOUT_ERROR` (a 502/503 depending on the failure kind).
//!
//! # Deviations from APISIX
//!
//! - Only the minimal header set `host`, `x-amz-date` and (when present)
//!   `x-amz-security-token` is covered by the SigV4 signature. AWS permits
//!   signing a subset of headers, and the remaining forwarded headers are sent
//!   unsigned; APISIX signs every forwarded header.
//! - `aws_service` defaults to `lambda` (APISIX defaults to `execute-api`).
//! - `ssl_verify` defaults to `false`, matching APISIX's documented behavior
//!   for this plugin.

use async_trait::async_trait;
use ring::{digest, hmac};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundError, OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// IAM credentials driving AWS SigV4 request signing.
#[derive(Clone)]
struct IamAuth {
    access_key: String,
    secret_key: String,
    region: String,
    service: String,
    session_token: Option<String>,
}

/// The plugin's resolved authorization strategy.
#[derive(Clone)]
enum Authorization {
    /// No credentials configured — forward as-is.
    None,
    /// Static API key sent as `x-api-key`.
    ApiKey(String),
    /// IAM role signing via AWS SigV4.
    Iam(IamAuth),
}

/// Invokes an AWS Lambda function and maps its reply into `Context.response`.
pub struct AwsLambdaPlugin {
    /// Full Lambda function URL / API endpoint.
    function_uri: String,
    authorization: Authorization,
    ssl_verify: bool,
    timeout: Duration,
    client: Arc<OutboundClient>,
}

impl AwsLambdaPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `function_uri` (string, **required**): the Lambda function URL or API
    ///   endpoint to invoke. A missing/empty value is a config-load error.
    /// - `authorization` (object, optional): either
    ///   - `apikey` (string) — sent as the `x-api-key` header (no signing), or
    ///   - `iam` (object) — SigV4 credentials:
    ///     - `accesskey` (string, **required**)
    ///     - `secretkey` (string, **required**)
    ///     - `aws_region` (string, default `us-east-1`)
    ///     - `aws_service` (string, default `lambda`)
    ///     - `session_token` (string, optional) — adds `x-amz-security-token`.
    ///
    ///   An `iam` block missing `accesskey`/`secretkey` is a config error.
    /// - `ssl_verify` (bool, default `false`): verify TLS certificates.
    /// - `timeout` (integer ms, default `3000`): whole-call deadline.
    ///
    /// ```yaml
    /// type: aws-lambda
    /// config:
    ///   function_uri: https://xyz.lambda-url.us-east-1.on.aws/
    ///   authorization:
    ///     iam:
    ///       accesskey: AKIDEXAMPLE
    ///       secretkey: ${AWS_SECRET_KEY}
    ///       aws_region: us-east-1
    ///       aws_service: lambda
    ///   ssl_verify: false
    ///   timeout: 3000
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let function_uri = config
            .get("function_uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "aws-lambda plugin requires 'function_uri'".to_string())?
            .to_string();

        let authorization = match config.get("authorization").and_then(|v| v.as_object()) {
            None => Authorization::None,
            Some(authz) => {
                if let Some(iam) = authz.get("iam").and_then(|v| v.as_object()) {
                    let access_key = iam
                        .get("accesskey")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            "aws-lambda plugin: authorization.iam requires 'accesskey'".to_string()
                        })?
                        .to_string();
                    let secret_key = iam
                        .get("secretkey")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            "aws-lambda plugin: authorization.iam requires 'secretkey'".to_string()
                        })?
                        .to_string();
                    let region = iam
                        .get("aws_region")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("us-east-1")
                        .to_string();
                    let service = iam
                        .get("aws_service")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("lambda")
                        .to_string();
                    let session_token = iam
                        .get("session_token")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    Authorization::Iam(IamAuth {
                        access_key,
                        secret_key,
                        region,
                        service,
                        session_token,
                    })
                } else if let Some(apikey) = authz.get("apikey").and_then(|v| v.as_str()) {
                    Authorization::ApiKey(apikey.to_string())
                } else {
                    Authorization::None
                }
            }
        };

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let timeout = Duration::from_millis(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(3000),
        );

        Ok(Self {
            function_uri,
            authorization,
            ssl_verify,
            timeout,
            client: resources.outbound.clone(),
        })
    }

    /// Assembles the outbound headers, canonical query string, and target URL
    /// for the request, applying the configured authorization (API key header
    /// or SigV4 signing). `amz_time` is the signing timestamp (seconds since
    /// epoch), threaded in so tests are deterministic.
    fn build_request(&self, ctx: &Context, amz_time: u64) -> Result<OutboundRequest, String> {
        let parsed: http::Uri = self
            .function_uri
            .parse()
            .map_err(|e| format!("aws-lambda: invalid function_uri: {}", e))?;
        let host = parsed
            .authority()
            .map(|a| a.as_str().to_string())
            .ok_or_else(|| "aws-lambda: function_uri has no host".to_string())?;
        let canonical_uri = normalize_path(parsed.path());
        let canonical_query = canonical_query_string(&ctx.request.query_params);

        // Forward the client headers, excluding hop-by-hop and headers we set
        // ourselves; override Host with the function endpoint's authority.
        let mut headers: Vec<(String, String)> = Vec::new();
        for (name, values) in &ctx.request.headers {
            let lname = name.to_ascii_lowercase();
            if matches!(
                lname.as_str(),
                "host" | "connection" | "content-length" | "x-amz-date" | "authorization"
            ) {
                continue;
            }
            for value in values {
                headers.push((lname.clone(), value.clone()));
            }
        }
        headers.push(("host".to_string(), host.clone()));

        match &self.authorization {
            Authorization::None => {}
            Authorization::ApiKey(key) => {
                if !ctx
                    .request
                    .headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("x-api-key"))
                {
                    headers.push(("x-api-key".to_string(), key.clone()));
                }
            }
            Authorization::Iam(iam) => {
                let amz_date = format_amz_date(amz_time);
                let datestamp = amz_date[..8].to_string();
                headers.push(("x-amz-date".to_string(), amz_date.clone()));
                if let Some(token) = &iam.session_token {
                    headers.push(("x-amz-security-token".to_string(), token.clone()));
                }
                // Sign the minimal, gateway-controlled header set.
                let mut signed: Vec<(String, String)> = vec![
                    ("host".to_string(), host.clone()),
                    ("x-amz-date".to_string(), amz_date.clone()),
                ];
                if let Some(token) = &iam.session_token {
                    signed.push(("x-amz-security-token".to_string(), token.clone()));
                }
                let sig = sign_v4(&SigV4Input {
                    method: &ctx.request.method,
                    canonical_uri: &canonical_uri,
                    canonical_query: &canonical_query,
                    headers: &signed,
                    payload: &ctx.request.body,
                    access_key: &iam.access_key,
                    secret_key: &iam.secret_key,
                    region: &iam.region,
                    service: &iam.service,
                    amz_date: &amz_date,
                    datestamp: &datestamp,
                });
                headers.push(("authorization".to_string(), sig.authorization));
            }
        }

        let scheme = parsed.scheme_str().unwrap_or("https");
        let url = if canonical_query.is_empty() {
            format!("{}://{}{}", scheme, host, canonical_uri)
        } else {
            format!("{}://{}{}?{}", scheme, host, canonical_uri, canonical_query)
        };

        let method: http::Method = ctx.request.method.parse().unwrap_or(http::Method::POST);

        Ok(OutboundRequest {
            method,
            url,
            headers,
            body: ctx.request.body.clone(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        })
    }
}

/// Copies the FaaS reply into `Context.response` (status, headers, body).
fn apply_response(ctx: &mut Context, response: OutboundResponse) {
    ctx.response.status_code = response.status;
    ctx.response.headers = response.headers;
    ctx.response.body = response.body;
}

#[async_trait]
impl Plugin for AwsLambdaPlugin {
    fn plugin_type(&self) -> &str {
        "aws-lambda"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let request = match self.build_request(&ctx, now) {
            Ok(req) => req,
            Err(message) => {
                return Err(reject(ctx, 502, message));
            }
        };

        match self.client.request(request).await {
            Ok(response) => {
                apply_response(&mut ctx, response);
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Err(e) => {
                let (status, message) = match &e {
                    OutboundError::Timeout(d) => {
                        (504, format!("aws-lambda callout timed out after {:?}", d))
                    }
                    OutboundError::InvalidRequest(m) => {
                        (502, format!("aws-lambda request build error: {}", m))
                    }
                    OutboundError::Transport(m) => {
                        (503, format!("aws-lambda callout failed: {}", m))
                    }
                };
                Err(reject(ctx, status, message))
            }
        }
    }
}

/// Builds the `AWS_LAMBDA_CALLOUT_ERROR` rejection carrying the context.
fn reject(mut ctx: Context, status: u16, message: String) -> PluginExecutionError {
    ctx.response.status_code = status;
    PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: "AWS_LAMBDA_CALLOUT_ERROR".to_string(),
            message,
            metadata: HashMap::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// AWS Signature Version 4 (pure helpers)
// ---------------------------------------------------------------------------

/// Inputs to [`sign_v4`]. `canonical_query` is a pre-built, sorted, encoded
/// query string; `headers` is the exact set of headers to cover by the
/// signature (names in any case, values already trimmed of outer whitespace).
struct SigV4Input<'a> {
    method: &'a str,
    canonical_uri: &'a str,
    canonical_query: &'a str,
    headers: &'a [(String, String)],
    payload: &'a [u8],
    access_key: &'a str,
    secret_key: &'a str,
    region: &'a str,
    service: &'a str,
    amz_date: &'a str,
    datestamp: &'a str,
}

/// Result of a SigV4 signing pass.
struct SigV4Output {
    /// The `Authorization` header value.
    authorization: String,
    /// The `SignedHeaders` list (`;`-joined).
    #[allow(dead_code)]
    signed_headers: String,
    /// The lowercase-hex signature.
    #[allow(dead_code)]
    signature: String,
    /// The canonical request (retained for testing).
    #[allow(dead_code)]
    canonical_request: String,
    /// The string-to-sign (retained for testing).
    #[allow(dead_code)]
    string_to_sign: String,
}

const ALGO: &str = "AWS4-HMAC-SHA256";

/// Computes an AWS Signature Version 4 for the given request.
///
/// Pure function: given identical inputs it always produces the same output.
/// The four SigV4 steps are: build the canonical request, form the
/// string-to-sign, derive the signing key via the HMAC date/region/service
/// chain, and HMAC the string-to-sign.
fn sign_v4(input: &SigV4Input) -> SigV4Output {
    // Canonical headers: lowercase names, trimmed values, sorted by name.
    let mut headers: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let payload_hash = hex(sha256(input.payload).as_ref());

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method.to_uppercase(),
        input.canonical_uri,
        input.canonical_query,
        canonical_headers,
        signed_headers,
        payload_hash
    );

    let credential_scope = format!(
        "{}/{}/{}/aws4_request",
        input.datestamp, input.region, input.service
    );
    let string_to_sign = format!(
        "{}\n{}\n{}\n{}",
        ALGO,
        input.amz_date,
        credential_scope,
        hex(sha256(canonical_request.as_bytes()).as_ref())
    );

    let signing_key = derive_signing_key(
        input.secret_key,
        input.datestamp,
        input.region,
        input.service,
    );
    let signature = hex(hmac_sha256(&signing_key, string_to_sign.as_bytes()).as_ref());

    let authorization = format!(
        "{} Credential={}/{}, SignedHeaders={}, Signature={}",
        ALGO, input.access_key, credential_scope, signed_headers, signature
    );

    SigV4Output {
        authorization,
        signed_headers,
        signature,
        canonical_request,
        string_to_sign,
    }
}

/// Derives the SigV4 signing key via the HMAC date → region → service chain.
fn derive_signing_key(secret: &str, datestamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{}", secret);
    let k_date = hmac_sha256(k_secret.as_bytes(), datestamp.as_bytes());
    let k_region = hmac_sha256(k_date.as_ref(), region.as_bytes());
    let k_service = hmac_sha256(k_region.as_ref(), service.as_bytes());
    let k_signing = hmac_sha256(k_service.as_ref(), b"aws4_request");
    k_signing.as_ref().to_vec()
}

/// HMAC-SHA256 over `msg` with `key`.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> hmac::Tag {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, msg)
}

/// SHA-256 digest of `data`.
fn sha256(data: &[u8]) -> digest::Digest {
    digest::digest(&digest::SHA256, data)
}

/// Lowercase-hex encoding of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Percent-encodes a string per RFC 3986, leaving the SigV4 unreserved set
/// (`A-Z a-z 0-9 - _ . ~`) intact.
fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Normalizes a request path for the SigV4 canonical URI: an empty path
/// becomes `/`, and a trailing slash on a non-root path is dropped.
fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    if path != "/" && path.ends_with('/') {
        return path.trim_end_matches('/').to_string();
    }
    path.to_string()
}

/// Builds the SigV4 canonical query string: URI-encode every name and value,
/// then sort the pairs by encoded name and value.
fn canonical_query_string(query: &HashMap<String, Vec<String>>) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (name, values) in query {
        let name = uri_encode(name);
        for value in values {
            pairs.push((name.clone(), uri_encode(value)));
        }
    }
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&")
}

/// Formats a unix timestamp as an AWS `amz-date` (`YYYYMMDDTHHMMSSZ`, UTC).
fn format_amz_date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        year, month, day, hour, minute, second
    )
}

/// Converts a count of days since the unix epoch into `(year, month, day)`
/// using Howard Hinnant's civil-date algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    fn ctx_with(method: &str, path: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: method.to_string(),
                path: path.to_string(),
                host: "gw".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn plugin(config: serde_json::Value) -> AwsLambdaPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        AwsLambdaPlugin::from_config(&map, &PluginResources::empty()).unwrap()
    }

    #[test]
    fn test_requires_function_uri() {
        assert!(AwsLambdaPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_iam_requires_keys() {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
            "function_uri": "https://x.on.aws/",
            "authorization": { "iam": { "accesskey": "AK" } }
        }))
        .unwrap();
        assert!(AwsLambdaPlugin::from_config(&map, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_iam_defaults() {
        let p = plugin(serde_json::json!({
            "function_uri": "https://x.on.aws/",
            "authorization": { "iam": { "accesskey": "AK", "secretkey": "SK" } }
        }));
        match p.authorization {
            Authorization::Iam(ref iam) => {
                assert_eq!(iam.region, "us-east-1");
                assert_eq!(iam.service, "lambda");
            }
            _ => panic!("expected IAM auth"),
        }
        // ssl_verify defaults to false for aws-lambda
        assert!(!p.ssl_verify);
    }

    /// The published AWS SigV4 `get-vanilla` test vector.
    /// See https://docs.aws.amazon.com/general/latest/gr/sigv4_signing.html
    #[test]
    fn test_sigv4_get_vanilla_vector() {
        let headers = vec![
            ("host".to_string(), "example.amazonaws.com".to_string()),
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
        ];
        let out = sign_v4(&SigV4Input {
            method: "GET",
            canonical_uri: "/",
            canonical_query: "",
            headers: &headers,
            payload: b"",
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
            datestamp: "20150830",
        });
        assert_eq!(
            out.signature,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert_eq!(out.signed_headers, "host;x-amz-date");
        assert_eq!(
            out.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        // The string-to-sign carries the algorithm and credential scope; the
        // signature above already pins the (correct) canonical-request hash.
        assert!(out.string_to_sign.starts_with(
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n"
        ));
        // Canonical request has the expected first line and payload hash.
        assert!(out.canonical_request.starts_with("GET\n/\n\n"));
        assert!(out
            .canonical_request
            .ends_with("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"));
    }

    #[test]
    fn test_sigv4_is_deterministic() {
        let headers = vec![("host".to_string(), "h".to_string())];
        let mk = || {
            sign_v4(&SigV4Input {
                method: "POST",
                canonical_uri: "/f",
                canonical_query: "a=1&b=2",
                headers: &headers,
                payload: b"body",
                access_key: "AK",
                secret_key: "SK",
                region: "us-east-1",
                service: "lambda",
                amz_date: "20240101T000000Z",
                datestamp: "20240101",
            })
            .signature
        };
        assert_eq!(mk(), mk());
    }

    #[test]
    fn test_canonical_query_sorted_and_encoded() {
        let mut q = HashMap::new();
        q.insert("b".to_string(), vec!["2".to_string()]);
        q.insert("a".to_string(), vec!["hello world".to_string()]);
        assert_eq!(canonical_query_string(&q), "a=hello%20world&b=2");
    }

    #[test]
    fn test_format_amz_date_vector() {
        // 20150830T123600Z corresponds to unix 1440938160.
        assert_eq!(format_amz_date(1440938160), "20150830T123600Z");
    }

    #[test]
    fn test_apikey_sets_header() {
        let p = plugin(serde_json::json!({
            "function_uri": "https://x.on.aws/fn",
            "authorization": { "apikey": "secret-key" }
        }));
        let req = p
            .build_request(&ctx_with("POST", "/ignored"), 1440938160)
            .unwrap();
        let hdr = req
            .headers
            .iter()
            .find(|(k, _)| k == "x-api-key")
            .map(|(_, v)| v.as_str());
        assert_eq!(hdr, Some("secret-key"));
        assert!(req.url.starts_with("https://x.on.aws/fn"));
    }

    #[test]
    fn test_iam_build_request_adds_signature_headers() {
        let p = plugin(serde_json::json!({
            "function_uri": "https://example.amazonaws.com/",
            "authorization": {
                "iam": { "accesskey": "AK", "secretkey": "SK", "session_token": "TOK" }
            }
        }));
        let req = p.build_request(&ctx_with("GET", "/"), 1440938160).unwrap();
        let has = |name: &str| req.headers.iter().any(|(k, _)| k == name);
        assert!(has("x-amz-date"));
        assert!(has("x-amz-security-token"));
        let authz = req
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(authz.starts_with("AWS4-HMAC-SHA256 Credential=AK/"));
        // session token is covered by the signature
        assert!(authz.contains("x-amz-security-token"));
    }

    #[tokio::test]
    async fn test_callout_failure_routes_error() {
        // Unresolvable host -> transport error -> error port.
        let p = plugin(serde_json::json!({
            "function_uri": "http://127.0.0.1:1/fn",
            "timeout": 200
        }));
        let err = p
            .execute(ctx_with("POST", "/"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "AWS_LAMBDA_CALLOUT_ERROR");
        assert!(err.context.response.status_code >= 502);
    }

    #[test]
    fn test_apply_response_maps_fields() {
        let mut ctx = ctx_with("GET", "/");
        let mut headers = HashMap::new();
        headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        apply_response(
            &mut ctx,
            OutboundResponse {
                status: 201,
                headers,
                body: Bytes::from_static(b"{\"ok\":true}"),
            },
        );
        assert_eq!(ctx.response.status_code, 201);
        assert_eq!(ctx.response.body, Bytes::from_static(b"{\"ok\":true}"));
        assert_eq!(
            ctx.response.headers.get("content-type"),
            Some(&vec!["application/json".to_string()])
        );
    }
}
