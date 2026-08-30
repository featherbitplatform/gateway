//! The one response shape for "the node could not do its job because the
//! identity/authorization provider it depends on failed" — discovery, JWKS,
//! introspection, token endpoint, CAS `/serviceValidate`, an LDAP bind
//! transport error, a Keycloak/Casdoor callout.
//!
//! Such a failure is **not** an authentication or authorization decision, so
//! it must not look like one: a `502` with `{"error": "provider_error"}` and
//! no `WWW-Authenticate` challenge, exiting through the node's `error` port.
//! Before this helper each plugin mirrored its own `denied` shape (`401
//! unauthorized` + challenge, or `403 access_denied`), which made an IdP
//! outage indistinguishable from a rejected credential — to API clients and,
//! for the interactive plugins, to browser users who saw "unauthorized"
//! instead of a login redirect.

use std::collections::HashMap;

use bytes::Bytes;

use crate::context::{Context, GatewayError};
use crate::plugins::PluginExecutionError;

/// Prepares the provider-failure response on `ctx` and wraps it in the
/// `Err` the graph engine routes through the node's `error` port.
///
/// `code` is the plugin's own error code (e.g. `OIDC_PROVIDER_ERROR`) —
/// policies, loggers and traces key on it, so it stays per plugin; `message`
/// is the operational reason and is echoed verbatim (JSON-escaped) in the
/// body so a client or the notification log can show it.
pub fn provider_error(mut ctx: Context, code: &str, message: String) -> PluginExecutionError {
    ctx.response.status_code = 502;
    ctx.response.headers.remove("www-authenticate");
    ctx.response.body = Bytes::from(
        serde_json::json!({ "error": "provider_error", "message": message }).to_string(),
    );
    ctx.response.headers.insert(
        "content-type".to_string(),
        vec!["application/json".to_string()],
    );
    PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: code.to_string(),
            message,
            metadata: HashMap::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Context, GatewayRequest};
    use bytes::Bytes;
    use std::collections::HashMap;

    fn ctx() -> Context {
        Context::new(GatewayRequest {
            method: "GET".into(),
            path: "/".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers: HashMap::new(),
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        })
    }

    #[test]
    fn prepares_a_502_provider_error_response() {
        let err = provider_error(ctx(), "X_PROVIDER_ERROR", "idp unreachable".to_string());
        testing::assert_provider_error(&err, "X_PROVIDER_ERROR");
        assert_eq!(err.error.message, "idp unreachable");
        // node_id is stamped by the graph engine, never by the plugin.
        assert_eq!(err.error.node_id, "");
    }

    #[test]
    fn strips_a_challenge_a_caller_may_have_prepared() {
        let mut c = ctx();
        c.response.headers.insert(
            "www-authenticate".to_string(),
            vec!["Basic realm=\"x\"".to_string()],
        );
        let err = provider_error(c, "X", "m".to_string());
        assert!(!err
            .context
            .response
            .headers
            .contains_key("www-authenticate"));
    }

    #[test]
    fn message_is_json_escaped_not_interpolated() {
        let err = provider_error(ctx(), "X", r#"said "no" \ bye"#.to_string());
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(body["message"], r#"said "no" \ bye"#);
    }
}

/// Test-only assertions shared by every plugin that emits a provider error,
/// so the shape is pinned in one place and cannot drift per plugin.
#[cfg(test)]
pub mod testing {
    use crate::plugins::PluginExecutionError;

    /// Asserts `err` has the provider-error shape: `502`, JSON body with
    /// `error: "provider_error"` and the error's message, no
    /// `WWW-Authenticate` challenge, and the expected error code.
    pub fn assert_provider_error(err: &PluginExecutionError, code: &str) {
        assert_eq!(err.error.code, code);
        assert_eq!(
            err.context.response.status_code, 502,
            "provider failures are 502"
        );
        assert!(
            !err.context
                .response
                .headers
                .contains_key("www-authenticate"),
            "a provider failure must not challenge the client"
        );
        assert_eq!(
            err.context
                .response
                .headers
                .get("content-type")
                .map(|v| v[0].as_str()),
            Some("application/json")
        );
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body)
            .expect("provider error body is JSON");
        assert_eq!(body["error"], "provider_error", "{body}");
        assert_eq!(body["message"], err.error.message, "{body}");
    }
}
