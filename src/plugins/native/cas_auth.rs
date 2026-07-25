//! CAS authentication plugin (`cas-auth`).
//!
//! Port of the ticket-validation step of APISIX's `cas-auth` plugin, with an
//! optional **interactive CAS SSO login flow** layered on top via the shared
//! [encrypted-cookie session primitive](crate::plugins::util::cookie_session).
//!
//! ## Two modes
//!
//! - **Stateless (default)** — when no session secret is configured the node
//!   behaves exactly as before: a request carrying a CAS service `ticket`
//!   query parameter is validated against the CAS server's `/serviceValidate`
//!   endpoint and, on success, the authenticated user is attached to the
//!   request; anything else is rejected with `CAS_AUTH_FAILED` (`401`).
//! - **Interactive (opt-in)** — set `session.secret` (or `session_secret`) to
//!   turn on the full browser login flow. The authenticated user is sealed
//!   into an encrypted client-side cookie (no server-side session store), so
//!   the node can redirect unauthenticated browsers to the IdP's `/login`,
//!   consume the returned ticket at the callback, and thereafter authenticate
//!   requests straight from the cookie. See the three-branch logic in
//!   [`CasAuthPlugin::execute_interactive`].
//!
//! ## Redirect wiring (interactive mode)
//!
//! A `302` produced by this node (login redirect, post-callback redirect, or
//! logout) is returned as an [`Err`] carrying the prepared response with code
//! `CAS_REDIRECT`, following the same early-exit convention as the
//! `fault-injection`/`mocking` nodes. **Wire the node's `error` edge to
//! `client.in`** so the redirect reaches the browser; the `success` edge
//! carries authenticated requests on to the upstream.

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::cookie_session::{
    build_set_cookie, delete_cookie, read_cookie, CookieAttrs, CookieSealer, SameSite,
};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Session payload sealed into the CAS session cookie (interactive mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CasSession {
    /// The CAS-authenticated username captured at login.
    user: String,
}

/// Validates CAS service tickets and, in interactive mode, runs the SSO flow.
pub struct CasAuthPlugin {
    /// CAS server base URI (e.g. `https://cas.example.org/cas`).
    idp_uri: String,
    /// Service URL sent to `/serviceValidate`; when unset it is derived from the
    /// request (`scheme://host/path`). Must match the service the ticket was
    /// issued for, so an explicit value is strongly recommended.
    service: Option<String>,
    /// Query parameter the ticket is read from (default `ticket`).
    ticket_param: String,
    /// Whether the CAS server's TLS certificate is verified.
    ssl_verify: bool,
    /// Whole-call deadline for the validation callout.
    timeout: Duration,
    /// When set, interactive SSO login is enabled and this seals/opens the
    /// session cookie. `None` keeps the stateless ticket-validator behavior.
    sealer: Option<CookieSealer>,
    /// Name of the session cookie (interactive mode).
    cookie_name: String,
    /// `Path` attribute of the session cookie (interactive mode). Scope it to a
    /// subpath (e.g. `/app_a`) so nodes on distinct subpaths keep independent
    /// sessions. Defaults to `/`.
    cookie_path: String,
    /// Session cookie lifetime in seconds (interactive mode).
    cookie_lifetime: u64,
    /// Optional logout path; a request to it clears the session cookie.
    logout_path: Option<String>,
    client: Arc<OutboundClient>,
}

impl CasAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `idp_uri` (string, **required**): CAS server base URI. The validation
    ///   request goes to `<idp_uri>/serviceValidate`.
    /// - `service` (string, optional): service URL passed to `/serviceValidate`.
    ///   When omitted it is derived from the request scheme/host/path; because
    ///   CAS requires the validated service to match the login service, set this
    ///   explicitly whenever the gateway sits behind a proxy.
    /// - `ticket_param` (string, default `"ticket"`): query parameter carrying
    ///   the CAS service ticket.
    /// - `ssl_verify` (bool, default `true`): verify the CAS server TLS cert.
    /// - `timeout_ms` (u64, default `3000`): callout deadline.
    ///
    /// Interactive-mode keys (present ⇒ interactive login is enabled):
    /// - `session_secret` (string) or `session.secret` (string): signing/encryption
    ///   secret for the session cookie. Setting it turns on the SSO flow.
    /// - `session.cookie.name` (string, default `"cas_session"`): session cookie name.
    /// - `session.cookie.path` (string, default `"/"`): session cookie `Path`;
    ///   scope to a subpath (e.g. `/app_a`) for independent per-app sessions.
    /// - `session.cookie.lifetime` (u64 seconds, default `3600`): cookie lifetime.
    /// - `logout_path` (string, optional): request path that clears the session
    ///   cookie and redirects to `/`.
    ///
    /// ```yaml
    /// type: cas-auth
    /// config:
    ///   idp_uri: https://cas.example.org/cas
    ///   service: https://app.example.org/
    ///   ssl_verify: true
    ///   session:
    ///     secret: ${CAS_SESSION_SECRET}
    ///     cookie: { name: cas_session, lifetime: 3600 }
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let idp_uri = config
            .get("idp_uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or("cas-auth plugin requires a non-empty 'idp_uri'")?
            .trim_end_matches('/')
            .to_string();

        let service = config
            .get("service")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from);

        let ticket_param = config
            .get("ticket_param")
            .and_then(|v| v.as_str())
            .unwrap_or("ticket")
            .to_string();

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let timeout = Duration::from_millis(
            config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(3_000),
        );

        // Interactive mode is enabled when a session secret is configured.
        let sealer = session_secret(config).map(|s| CookieSealer::new(&s));
        let cookie_name =
            session_cookie_str(config, "name").unwrap_or_else(|| "cas_session".to_string());
        let cookie_path = session_cookie_str(config, "path").unwrap_or_else(|| "/".to_string());
        let cookie_lifetime = session_cookie_u64(config, "lifetime").unwrap_or(3_600);
        let logout_path = config
            .get("logout_path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        Ok(Self {
            idp_uri,
            service,
            ticket_param,
            ssl_verify,
            timeout,
            sealer,
            cookie_name,
            cookie_path,
            cookie_lifetime,
            logout_path,
            client: resources.outbound.clone(),
        })
    }

    /// Builds a 401 rejection routed through the node's error port.
    fn reject(&self, ctx: Context, message: &str) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 401;
        ctx.response.body = Bytes::from(format!(
            r#"{{"error": "unauthorized", "message": "{}"}}"#,
            message
        ));
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "CAS_AUTH_FAILED".to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Builds a `302` early-exit carrying the prepared response. Wire the
    /// node's **error** edge to `client.in` so this reaches the browser.
    fn redirect(
        &self,
        mut ctx: Context,
        location: String,
        set_cookies: Vec<String>,
    ) -> PluginResult {
        ctx.response.status_code = 302;
        ctx.response
            .headers
            .insert("location".to_string(), vec![location]);
        if !set_cookies.is_empty() {
            ctx.response
                .headers
                .insert("set-cookie".to_string(), set_cookies);
        }
        ctx.response.body = Bytes::new();
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "CAS_REDIRECT".to_string(),
                message: "cas-auth redirect".to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// The service URL sent to `/serviceValidate`: the configured value, or one
    /// derived from the request (`scheme://host/path`, without the query so the
    /// ticket is dropped).
    fn service_url(&self, ctx: &Context) -> String {
        if let Some(ref service) = self.service {
            return service.clone();
        }
        format!(
            "{}://{}{}",
            ctx.request.scheme, ctx.request.host, ctx.request.path
        )
    }

    /// Cookie attributes for the session cookie: `HttpOnly`, `SameSite=Lax`, and
    /// `Secure` only over HTTPS (so plain-HTTP dev works).
    fn session_attrs(&self, ctx: &Context) -> CookieAttrs<'_> {
        CookieAttrs {
            path: &self.cookie_path,
            max_age: Some(self.cookie_lifetime),
            http_only: true,
            secure: ctx.request.scheme == "https",
            same_site: SameSite::Lax,
        }
    }

    /// Attaches the authenticated user to the request/context.
    fn attach_user(&self, ctx: &mut Context, user: &str) {
        ctx.request
            .headers
            .insert("x-cas-user".to_string(), vec![user.to_string()]);
        ctx.message.insert(
            "user".to_string(),
            serde_json::Value::String(user.to_string()),
        );
        ctx.message.insert(
            "user_id".to_string(),
            serde_json::Value::String(user.to_string()),
        );
    }

    /// Reads and opens the session cookie, returning the authenticated user.
    fn read_session(&self, ctx: &Context) -> Option<String> {
        let sealer = self.sealer.as_ref()?;
        let cookie_header = ctx.request.headers.get("cookie").and_then(|v| v.first())?;
        let raw = read_cookie(cookie_header, &self.cookie_name)?;
        let payload = sealer.open(raw).ok()?;
        let session: CasSession = serde_json::from_slice(&payload).ok()?;
        Some(session.user)
    }

    /// Validates a CAS ticket against `/serviceValidate`, returning the user.
    async fn cas_validate(&self, ctx: &Context, ticket: &str) -> Result<String, String> {
        let service = self.service_url(ctx);
        let url = build_validate_url(&self.idp_uri, ticket, &service);

        let outbound = OutboundRequest {
            method: http::Method::GET,
            url,
            headers: Vec::new(),
            body: Bytes::new(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        let response = self
            .client
            .request(outbound)
            .await
            .map_err(|e| format!("CAS validation request failed: {}", e))?;

        if response.status != 200 {
            return Err("CAS validation returned non-200".to_string());
        }
        parse_service_validate(&response.body).ok_or_else(|| "invalid ticket".to_string())
    }

    /// Interactive SSO flow: session cookie → callback → begin login.
    async fn execute_interactive(&self, mut ctx: Context) -> PluginResult {
        let sealer = self
            .sealer
            .as_ref()
            .expect("execute_interactive only called when a sealer is configured");

        // 0. Logout: clear the session cookie and bounce to "/".
        if let Some(ref logout_path) = self.logout_path {
            if &ctx.request.path == logout_path {
                let del = delete_cookie(&self.cookie_name, &self.cookie_path);
                return self.redirect(ctx, "/".to_string(), vec![del]);
            }
        }

        // 1. Valid session cookie → authenticate straight from it.
        if let Some(user) = self.read_session(&ctx) {
            self.attach_user(&mut ctx, &user);
            return Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            });
        }

        // 2. Callback: a CAS ticket came back on the service URL. Validate it,
        //    seal a session cookie, and redirect to the ticket-free service URL.
        if let Some(ticket) = extract_ticket(&ctx.request.query_params, &self.ticket_param) {
            return match self.cas_validate(&ctx, &ticket).await {
                Ok(user) => {
                    let payload = serde_json::to_vec(&CasSession { user }).unwrap_or_default();
                    let sealed = sealer.seal(&payload, Duration::from_secs(self.cookie_lifetime));
                    let set =
                        build_set_cookie(&self.cookie_name, &sealed, &self.session_attrs(&ctx));
                    let target = self.service_url(&ctx);
                    self.redirect(ctx, target, vec![set])
                }
                Err(reason) => self.reject(ctx, &reason),
            };
        }

        // 3. No session, not a callback → begin login at the IdP.
        let service = self.service_url(&ctx);
        let login = build_login_url(&self.idp_uri, &service);
        self.redirect(ctx, login, vec![])
    }

    /// Stateless ticket validation (the default, pre-interactive behavior).
    async fn execute_stateless(&self, mut ctx: Context) -> PluginResult {
        let ticket = match extract_ticket(&ctx.request.query_params, &self.ticket_param) {
            Some(t) => t,
            None => return self.reject(ctx, "missing CAS ticket"),
        };

        match self.cas_validate(&ctx, &ticket).await {
            Ok(user) => {
                self.attach_user(&mut ctx, &user);
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Err(reason) => self.reject(ctx, &reason),
        }
    }
}

/// Reads the session secret from `session_secret` or nested `session.secret`.
fn session_secret(config: &HashMap<String, serde_json::Value>) -> Option<String> {
    config
        .get("session_secret")
        .and_then(|v| v.as_str())
        .or_else(|| {
            config
                .get("session")
                .and_then(|s| s.get("secret"))
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Reads a string field from nested `session.cookie.<key>`, falling back to the
/// flat `session_cookie_<key>` form (used by the UI schema).
fn session_cookie_str(config: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    config
        .get("session")
        .and_then(|s| s.get("cookie"))
        .and_then(|c| c.get(key))
        .or_else(|| config.get(&format!("session_cookie_{key}")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Reads a u64 field from nested `session.cookie.<key>`, falling back to the
/// flat `session_cookie_<key>` form (used by the UI schema).
fn session_cookie_u64(config: &HashMap<String, serde_json::Value>, key: &str) -> Option<u64> {
    config
        .get("session")
        .and_then(|s| s.get("cookie"))
        .and_then(|c| c.get(key))
        .or_else(|| config.get(&format!("session_cookie_{key}")))
        .and_then(|v| v.as_u64())
}

/// Percent-encodes a query-argument value (RFC3986 unreserved chars kept).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Reads the CAS ticket from the request's query parameters.
fn extract_ticket(query: &HashMap<String, Vec<String>>, param: &str) -> Option<String> {
    query
        .get(param)
        .and_then(|v| v.first())
        .filter(|s| !s.is_empty())
        .cloned()
}

/// Builds the CAS `/serviceValidate` URL.
fn build_validate_url(idp_uri: &str, ticket: &str, service: &str) -> String {
    format!(
        "{}/serviceValidate?ticket={}&service={}",
        idp_uri,
        percent_encode(ticket),
        percent_encode(service),
    )
}

/// Builds the CAS `/login?service=<service>` URL used to begin interactive login.
fn build_login_url(idp_uri: &str, service: &str) -> String {
    format!("{}/login?service={}", idp_uri, percent_encode(service))
}

/// Extracts the authenticated username from a CAS `/serviceValidate` response.
///
/// Handles both the default CAS 2.0 **XML** (`<cas:authenticationSuccess>` /
/// `<cas:user>`, with or without the `cas:` prefix) and the CAS 3.0 **JSON**
/// (`serviceResponse.authenticationSuccess.user`) formats. Returns `None` for
/// an authentication-failure response or anything unparseable.
fn parse_service_validate(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;

    // JSON format (format=json / CAS 3.0).
    if text.trim_start().starts_with('{') {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
            let user = json
                .get("serviceResponse")
                .and_then(|v| v.get("authenticationSuccess"))
                .and_then(|v| v.get("user"))
                .and_then(|v| v.as_str());
            return user.map(|s| s.trim().to_string());
        }
    }

    // XML format (default CAS 2.0).
    if !text.contains("authenticationSuccess") {
        return None;
    }
    extract_xml_tag(text, "cas:user").or_else(|| extract_xml_tag(text, "user"))
}

/// Returns the trimmed text between the first `<tag>` and its `</tag>`.
fn extract_xml_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let value = text[start..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[async_trait]
impl Plugin for CasAuthPlugin {
    fn plugin_type(&self) -> &str {
        "cas-auth"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if self.sealer.is_some() {
            self.execute_interactive(ctx).await
        } else {
            self.execute_stateless(ctx).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin() -> CasAuthPlugin {
        let mut cfg = HashMap::new();
        cfg.insert(
            "idp_uri".to_string(),
            serde_json::json!("https://cas.example.org/cas"),
        );
        cfg.insert(
            "service".to_string(),
            serde_json::json!("https://app.example.org/"),
        );
        CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap()
    }

    fn ctx(path: &str, query: HashMap<String, Vec<String>>) -> Context {
        crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: path.into(),
            host: "app.example.org".into(),
            scheme: "https".into(),
            headers: HashMap::new(),
            query_params: query,
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        })
    }

    #[test]
    fn test_from_config_requires_idp_uri() {
        assert!(CasAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
        let p = plugin();
        assert_eq!(p.idp_uri, "https://cas.example.org/cas");
        assert_eq!(p.ticket_param, "ticket");
        assert!(p.ssl_verify);
    }

    #[test]
    fn test_stateless_by_default() {
        // No session secret ⇒ stateless (sealer absent), defaults populated.
        let p = plugin();
        assert!(p.sealer.is_none());
        assert_eq!(p.cookie_name, "cas_session");
        assert_eq!(p.cookie_path, "/");
        assert_eq!(p.cookie_lifetime, 3600);
        assert!(p.logout_path.is_none());
    }

    #[test]
    fn test_session_cookie_path_configurable() {
        // Nested form.
        let mut cfg = HashMap::new();
        cfg.insert("idp_uri".to_string(), serde_json::json!("https://cas/cas"));
        cfg.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "abc", "cookie": { "path": "/app_a" } }),
        );
        let p = CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        assert_eq!(p.cookie_path, "/app_a");

        // Flat form the Web UI emits.
        let mut cfg = HashMap::new();
        cfg.insert("idp_uri".to_string(), serde_json::json!("https://cas/cas"));
        cfg.insert("session_secret".to_string(), serde_json::json!("abc"));
        cfg.insert(
            "session_cookie_path".to_string(),
            serde_json::json!("/app_b"),
        );
        let p = CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        assert_eq!(p.cookie_path, "/app_b");
    }

    #[test]
    fn test_interactive_enabled_by_session_secret() {
        // top-level session_secret
        let mut cfg = HashMap::new();
        cfg.insert("idp_uri".to_string(), serde_json::json!("https://cas/cas"));
        cfg.insert("session_secret".to_string(), serde_json::json!("s3cr3t"));
        let p = CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        assert!(p.sealer.is_some());

        // nested session.secret + cookie overrides
        let mut cfg = HashMap::new();
        cfg.insert("idp_uri".to_string(), serde_json::json!("https://cas/cas"));
        cfg.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "abc", "cookie": { "name": "sess", "lifetime": 60 } }),
        );
        cfg.insert("logout_path".to_string(), serde_json::json!("/logout"));
        let p = CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        assert!(p.sealer.is_some());
        assert_eq!(p.cookie_name, "sess");
        assert_eq!(p.cookie_lifetime, 60);
        assert_eq!(p.logout_path.as_deref(), Some("/logout"));
    }

    #[test]
    fn test_extract_ticket() {
        let mut query = HashMap::new();
        query.insert("ticket".to_string(), vec!["ST-12345".to_string()]);
        assert_eq!(
            extract_ticket(&query, "ticket"),
            Some("ST-12345".to_string())
        );
        assert_eq!(extract_ticket(&query, "other"), None);
        // empty ticket ignored
        let mut query = HashMap::new();
        query.insert("ticket".to_string(), vec!["".to_string()]);
        assert_eq!(extract_ticket(&query, "ticket"), None);
        assert_eq!(extract_ticket(&HashMap::new(), "ticket"), None);
    }

    #[test]
    fn test_build_validate_url() {
        assert_eq!(
            build_validate_url("https://cas.example.org/cas", "ST-1 2", "https://app/"),
            "https://cas.example.org/cas/serviceValidate?ticket=ST-1%202&service=https%3A%2F%2Fapp%2F"
        );
    }

    #[test]
    fn test_build_login_url() {
        assert_eq!(
            build_login_url(
                "https://cas.example.org/cas",
                "https://app.example.org/dashboard"
            ),
            "https://cas.example.org/cas/login?service=https%3A%2F%2Fapp.example.org%2Fdashboard"
        );
    }

    #[test]
    fn test_session_seal_open_round_trip() {
        let sealer = CookieSealer::new("cas-secret");
        let payload = serde_json::to_vec(&CasSession {
            user: "alice".into(),
        })
        .unwrap();
        let cookie = sealer.seal(&payload, Duration::from_secs(3600));
        let opened = sealer.open(&cookie).unwrap();
        let session: CasSession = serde_json::from_slice(&opened).unwrap();
        assert_eq!(session.user, "alice");
    }

    #[test]
    fn test_parse_service_validate_xml_success() {
        let body = br#"<cas:serviceResponse xmlns:cas='http://www.yale.edu/tp/cas'>
  <cas:authenticationSuccess>
    <cas:user>alice</cas:user>
  </cas:authenticationSuccess>
</cas:serviceResponse>"#;
        assert_eq!(parse_service_validate(body), Some("alice".to_string()));

        // no cas: prefix
        let body = b"<serviceResponse><authenticationSuccess><user>bob</user></authenticationSuccess></serviceResponse>";
        assert_eq!(parse_service_validate(body), Some("bob".to_string()));
    }

    #[test]
    fn test_parse_service_validate_xml_failure() {
        let body = br#"<cas:serviceResponse xmlns:cas='http://www.yale.edu/tp/cas'>
  <cas:authenticationFailure code='INVALID_TICKET'>ticket not recognized</cas:authenticationFailure>
</cas:serviceResponse>"#;
        assert_eq!(parse_service_validate(body), None);
    }

    #[test]
    fn test_parse_service_validate_json() {
        let body = br#"{"serviceResponse":{"authenticationSuccess":{"user":"carol"}}}"#;
        assert_eq!(parse_service_validate(body), Some("carol".to_string()));

        let body = br#"{"serviceResponse":{"authenticationFailure":{"code":"INVALID_TICKET"}}}"#;
        assert_eq!(parse_service_validate(body), None);
    }

    #[tokio::test]
    async fn test_missing_ticket_rejected() {
        let p = plugin();
        let err = p
            .execute(ctx("/", HashMap::new()), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "CAS_AUTH_FAILED");
        assert_eq!(err.context.response.status_code, 401);
    }

    #[test]
    fn test_service_url_derived_from_request() {
        let mut cfg = HashMap::new();
        cfg.insert("idp_uri".to_string(), serde_json::json!("https://cas/cas"));
        let p = CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        assert_eq!(
            p.service_url(&ctx("/dashboard", HashMap::new())),
            "https://app.example.org/dashboard"
        );
    }

    #[tokio::test]
    async fn test_interactive_begin_login_redirects() {
        // Interactive mode, no cookie, no ticket → 302 to the IdP /login.
        let mut cfg = HashMap::new();
        cfg.insert(
            "idp_uri".to_string(),
            serde_json::json!("https://cas.example.org/cas"),
        );
        cfg.insert("session_secret".to_string(), serde_json::json!("s3cr3t"));
        let p = CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let err = p
            .execute(ctx("/dashboard", HashMap::new()), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "CAS_REDIRECT");
        assert_eq!(err.context.response.status_code, 302);
        let location = &err.context.response.headers.get("location").unwrap()[0];
        assert!(
            location.starts_with("https://cas.example.org/cas/login?service="),
            "{location}"
        );
        // No cookie is set when merely beginning login.
        assert!(!err.context.response.headers.contains_key("set-cookie"));
    }

    #[tokio::test]
    async fn test_interactive_valid_session_passes() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "idp_uri".to_string(),
            serde_json::json!("https://cas.example.org/cas"),
        );
        cfg.insert("session_secret".to_string(), serde_json::json!("s3cr3t"));
        let p = CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        // Seal a session cookie the way a successful callback would.
        let sealer = CookieSealer::new("s3cr3t");
        let payload = serde_json::to_vec(&CasSession {
            user: "dave".into(),
        })
        .unwrap();
        let sealed = sealer.seal(&payload, Duration::from_secs(3600));

        let mut c = ctx("/dashboard", HashMap::new());
        c.request.headers.insert(
            "cookie".to_string(),
            vec![format!("cas_session={}", sealed)],
        );

        let out = p.execute(c, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.request.headers.get("x-cas-user").unwrap()[0],
            "dave"
        );
        assert_eq!(out.context.message.get("user").unwrap(), "dave");
    }

    #[tokio::test]
    async fn test_interactive_logout_clears_cookie() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "idp_uri".to_string(),
            serde_json::json!("https://cas.example.org/cas"),
        );
        cfg.insert("session_secret".to_string(), serde_json::json!("s3cr3t"));
        cfg.insert("logout_path".to_string(), serde_json::json!("/logout"));
        let p = CasAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let err = p
            .execute(ctx("/logout", HashMap::new()), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "CAS_REDIRECT");
        assert_eq!(err.context.response.status_code, 302);
        assert_eq!(
            err.context.response.headers.get("location").unwrap()[0],
            "/"
        );
        let set = &err.context.response.headers.get("set-cookie").unwrap()[0];
        assert!(
            set.contains("cas_session=") && set.contains("Max-Age=0"),
            "{set}"
        );
    }
}
