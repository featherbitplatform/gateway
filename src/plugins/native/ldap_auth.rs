//! LDAP Basic-auth plugin (`ldap-auth`).
//!
//! Authenticates the request's HTTP Basic credentials against an LDAP server by
//! performing a **simple bind** with the user's DN and the presented password.
//! Port of APISIX's `ldap-auth` plugin (the bind-authentication core of it).
//!
//! Flow (matching `apisix/plugins/ldap-auth.lua`):
//! 1. Parse the `Authorization: Basic <base64(user:pass)>` header.
//! 2. Assemble the bind DN as `<uid>=<username>,<base_dn>`.
//! 3. Connect to `ldap_uri` and attempt a simple bind with that DN + password.
//! 4. Bind success → continue (`context.message["user"] = username`); a missing
//!    header, malformed credentials, or a bind failure → 401 `LDAP_AUTH_FAILED`
//!    with a `WWW-Authenticate: Basic` challenge.
//!
//! This is **bind-auth**, not search-then-bind: the DN is built directly from
//! `uid`/`base_dn` and no directory search is performed. See the Deviations in
//! `website/docs/reference/plugins/ldap-auth.md`.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ldap3::{LdapConnAsync, LdapConnSettings};

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Authenticates HTTP Basic credentials against an LDAP server via simple bind.
pub struct LdapAuthPlugin {
    /// Base DN the bind DN is built under (e.g. `ou=users,dc=example,dc=org`).
    base_dn: String,
    /// LDAP server URI (`ldap://host:389` or `ldaps://host:636`).
    ldap_uri: String,
    /// RDN attribute for the bind DN (APISIX `uid`, default `cn`).
    uid: String,
    /// When true, negotiate StartTLS on the connection.
    use_tls: bool,
    /// When false, TLS certificate verification is disabled.
    tls_verify: bool,
    /// Realm advertised in the `WWW-Authenticate` challenge.
    realm: String,
    /// Whole-operation deadline for the connect + bind.
    timeout: Duration,
}

impl LdapAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `base_dn` (string, **required**): base DN the bind DN is built under.
    /// - `ldap_uri` (string, **required**): LDAP server URI, e.g.
    ///   `ldap://ldap.example.org:389`.
    /// - `uid` (string, default `"cn"`): RDN attribute prefixing the username
    ///   in the bind DN.
    /// - `use_tls` (bool, default `false`): negotiate StartTLS after connecting.
    /// - `tls_verify` (bool, default `false`): verify the server certificate.
    /// - `realm` (string, default `"ldap"`): realm in the challenge header.
    /// - `timeout_ms` (u64, default `10000`): connect + bind deadline.
    ///
    /// ```yaml
    /// type: ldap-auth
    /// config:
    ///   base_dn: ou=users,dc=example,dc=org
    ///   ldap_uri: ldap://ldap.example.org:389
    ///   uid: cn
    ///   use_tls: false
    ///   tls_verify: false
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        _resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let base_dn = config
            .get("base_dn")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or("ldap-auth plugin requires a non-empty 'base_dn'")?
            .to_string();

        let ldap_uri = config
            .get("ldap_uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or("ldap-auth plugin requires a non-empty 'ldap_uri'")?
            .to_string();

        let uid = config
            .get("uid")
            .and_then(|v| v.as_str())
            .unwrap_or("cn")
            .to_string();

        let use_tls = config
            .get("use_tls")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let tls_verify = config
            .get("tls_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let realm = config
            .get("realm")
            .and_then(|v| v.as_str())
            .unwrap_or("ldap")
            .to_string();

        let timeout = Duration::from_millis(
            config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(10_000),
        );

        Ok(Self {
            base_dn,
            ldap_uri,
            uid,
            use_tls,
            tls_verify,
            realm,
            timeout,
        })
    }

    /// Builds the 401 rejection carrying the `WWW-Authenticate: Basic` challenge
    /// and routes the context through the node's error port.
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
        ctx.response.headers.insert(
            "www-authenticate".to_string(),
            vec![format!("Basic realm=\"{}\"", self.realm)],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "LDAP_AUTH_FAILED".to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Attempts a simple bind with `dn`/`password` against the LDAP server.
    /// Returns `Ok(true)` on a successful bind, `Ok(false)` on rejected
    /// credentials, and `Err` on a connection/transport error.
    async fn bind(&self, dn: &str, password: &str) -> Result<bool, String> {
        let settings = LdapConnSettings::new()
            .set_no_tls_verify(!self.tls_verify)
            .set_starttls(self.use_tls);

        let (conn, mut ldap) = LdapConnAsync::with_settings(settings, &self.ldap_uri)
            .await
            .map_err(|e| e.to_string())?;
        ldap3::drive!(conn);

        let result = ldap
            .simple_bind(dn, password)
            .await
            .map_err(|e| e.to_string())?;
        let ok = result.success().is_ok();
        let _ = ldap.unbind().await;
        Ok(ok)
    }
}

/// Parses an `Authorization: Basic ...` header value into `(username, password)`.
///
/// Mirrors the APISIX Lua: the scheme match is case-insensitive, the payload is
/// standard-base64 decoded, split on the first `:`, and **all whitespace is
/// stripped** from both fields. Returns `None` when the header is not Basic,
/// the payload is not valid base64/UTF-8, or there is no `:` separator.
fn parse_basic_credentials(header: &str) -> Option<(String, String)> {
    let rest = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))
        .or_else(|| header.strip_prefix("BASIC "))?;
    let decoded = STANDARD.decode(rest.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    let strip_ws = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    Some((strip_ws(user), strip_ws(pass)))
}

/// Assembles the bind DN as `<uid>=<username>,<base_dn>` (APISIX's `user_dn`).
fn build_bind_dn(uid: &str, username: &str, base_dn: &str) -> String {
    format!("{}={},{}", uid, username, base_dn)
}

#[async_trait]
impl Plugin for LdapAuthPlugin {
    fn plugin_type(&self) -> &str {
        "ldap-auth"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let auth_header = ctx
            .request
            .headers
            .get("authorization")
            .and_then(|v| v.first())
            .cloned();

        let header = match auth_header {
            Some(h) => h,
            None => return self.reject(ctx, "Missing authorization in request"),
        };

        let (username, password) = match parse_basic_credentials(&header) {
            Some(creds) => creds,
            None => return self.reject(ctx, "Invalid authorization in request"),
        };

        // Reject empty credentials: an empty password would trigger an
        // unauthenticated (anonymous) bind that most servers accept, silently
        // authenticating anyone. Guard against it explicitly.
        if username.is_empty() || password.is_empty() {
            return self.reject(ctx, "Invalid authorization in request");
        }

        let dn = build_bind_dn(&self.uid, &username, &self.base_dn);

        let bind = tokio::time::timeout(self.timeout, self.bind(&dn, &password)).await;
        match bind {
            Ok(Ok(true)) => {
                ctx.message.insert(
                    "user".to_string(),
                    serde_json::Value::String(username.clone()),
                );
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Ok(Ok(false)) => self.reject(ctx, "Invalid user authorization"),
            Ok(Err(e)) => {
                let mut ctx = ctx;
                ctx.response.status_code = 401;
                ctx.response.headers.insert(
                    "www-authenticate".to_string(),
                    vec![format!("Basic realm=\"{}\"", self.realm)],
                );
                Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "LDAP_AUTH_FAILED".to_string(),
                        message: format!("LDAP connection error: {}", e),
                        metadata: HashMap::new(),
                    },
                })
            }
            Err(_) => self.reject(ctx, "LDAP authentication timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", STANDARD.encode(format!("{}:{}", user, pass)))
    }

    #[test]
    fn test_from_config_requires_base_dn_and_uri() {
        let mut cfg = HashMap::new();
        assert!(LdapAuthPlugin::from_config(&cfg, &PluginResources::empty()).is_err());
        cfg.insert(
            "base_dn".to_string(),
            serde_json::json!("dc=example,dc=org"),
        );
        assert!(LdapAuthPlugin::from_config(&cfg, &PluginResources::empty()).is_err());
        cfg.insert(
            "ldap_uri".to_string(),
            serde_json::json!("ldap://localhost:389"),
        );
        let plugin = LdapAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        assert_eq!(plugin.uid, "cn");
        assert_eq!(plugin.timeout, Duration::from_millis(10_000));
    }

    #[test]
    fn test_parse_basic_credentials() {
        let (u, p) = parse_basic_credentials(&basic("alice", "s3cret")).unwrap();
        assert_eq!(u, "alice");
        assert_eq!(p, "s3cret");

        // case-insensitive scheme
        let header = format!("basic {}", STANDARD.encode("bob:pw"));
        assert_eq!(
            parse_basic_credentials(&header),
            Some(("bob".into(), "pw".into()))
        );

        // whitespace stripped from both fields (APISIX gsub behavior)
        let header = format!("Basic {}", STANDARD.encode("a li ce:pa ss"));
        assert_eq!(
            parse_basic_credentials(&header),
            Some(("alice".into(), "pass".into()))
        );
    }

    #[test]
    fn test_parse_basic_credentials_rejects_malformed() {
        assert!(parse_basic_credentials("Bearer xyz").is_none());
        assert!(parse_basic_credentials("Basic !!!not-base64!!!").is_none());
        // no colon separator
        let header = format!("Basic {}", STANDARD.encode("nocolon"));
        assert!(parse_basic_credentials(&header).is_none());
    }

    #[test]
    fn test_build_bind_dn() {
        assert_eq!(
            build_bind_dn("cn", "alice", "ou=users,dc=example,dc=org"),
            "cn=alice,ou=users,dc=example,dc=org"
        );
        assert_eq!(build_bind_dn("uid", "bob", "dc=corp"), "uid=bob,dc=corp");
    }

    #[tokio::test]
    async fn test_missing_header_rejected() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "base_dn".to_string(),
            serde_json::json!("dc=example,dc=org"),
        );
        cfg.insert(
            "ldap_uri".to_string(),
            serde_json::json!("ldap://localhost:389"),
        );
        let plugin = LdapAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let ctx = crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: "/".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers: HashMap::new(),
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        });
        let err = plugin.execute(ctx, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "LDAP_AUTH_FAILED");
        assert_eq!(err.context.response.status_code, 401);
        assert_eq!(
            err.context.response.headers.get("www-authenticate"),
            Some(&vec!["Basic realm=\"ldap\"".to_string()])
        );
    }

    #[tokio::test]
    async fn test_empty_password_rejected() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "base_dn".to_string(),
            serde_json::json!("dc=example,dc=org"),
        );
        cfg.insert(
            "ldap_uri".to_string(),
            serde_json::json!("ldap://localhost:389"),
        );
        let plugin = LdapAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), vec![basic("alice", "")]);
        let ctx = crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: "/".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers,
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        });
        // Never reaches the network: empty password is rejected before binding.
        assert!(plugin.execute(ctx, &HashMap::new()).await.is_err());
    }
}
