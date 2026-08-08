//! Static port declarations for every node type.
//!
//! One [`PortSpec`] per plugin type, resolved through
//! [`crate::plugins::port_spec`] — the single source of truth shared by the
//! graph compiler (edge validation), the admin catalog (`GET /api/plugins`),
//! and by extension the UI editor. Plugins never override the `Plugin::ports`
//! trait method; the registry match in `port_spec` IS the declaration.

use serde::Serialize;

/// The flavor of an output port, driving validation and UI color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PortKind {
    /// The node completed normally and the request continues.
    Success,
    /// The node did its job and chose an alternate route (deny, redirect,
    /// throttle, preflight). Mandatory wiring, same as success.
    #[allow(dead_code)]
    Outcome,
    /// The node could not do its job. Optional wiring (fallback chain:
    /// per-node edge -> policy catch-all -> default 500).
    Error,
}

/// One declared output port.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PortDecl {
    pub name: &'static str,
    pub kind: PortKind,
    pub description: &'static str,
}

/// A node type's full port declaration.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PortSpec {
    /// Description of the single `in` port; `None` = the node has no input
    /// (only `listener`).
    pub input: Option<&'static str>,
    pub outputs: &'static [PortDecl],
}

/// Names no custom port may use. `out` is a YAML alias for `success`.
pub const RESERVED_PORT_NAMES: &[&str] = &["in", "out", "success", "error"];

const SUCCESS: PortDecl = PortDecl {
    name: "success",
    kind: PortKind::Success,
    description: "The node completed normally; the request continues.",
};
const ERROR: PortDecl = PortDecl {
    name: "error",
    kind: PortKind::Error,
    description: "The node failed (configuration, parse, or infrastructure error).",
};

/// The default pair every plugin without alternate outcomes uses.
pub const DEFAULT_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[SUCCESS, ERROR],
};

/// `listener`: pipeline entry, no input, single exit.
pub const LISTENER_SPEC: PortSpec = PortSpec {
    input: None,
    outputs: &[PortDecl {
        name: "success",
        kind: PortKind::Success,
        description: "Entry into the policy pipeline.",
    }],
};

/// `client`: terminal node, the response is sent from here.
pub const CLIENT_SPEC: PortSpec = PortSpec {
    input: Some("Final context; the response is sent to the client."),
    outputs: &[],
};

/// `cors`: preflight answers short-circuit on their own port.
pub const CORS_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "preflight",
            kind: PortKind::Outcome,
            description: "OPTIONS preflight answered with a prepared 204; wire to client.",
        },
        ERROR,
    ],
};

/// `redirect`: prepared 3xx responses exit on their own port.
pub const REDIRECT_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "redirect",
            kind: PortKind::Outcome,
            description: "A 3xx redirect response is prepared; wire to client.",
        },
        ERROR,
    ],
};

/// `fault-injection`: injected abort responses exit on their own port.
pub const FAULT_INJECTION_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "abort",
            kind: PortKind::Outcome,
            description: "An injected fault response is prepared; wire to client.",
        },
        ERROR,
    ],
};

/// Credential-auth plugins: deliberate 401/403 rejections exit on `denied`.
/// Genuine infrastructure failures (consumer store unavailable, LDAP
/// unreachable, IdP HTTP errors) remain on `error`.
pub const AUTH_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "denied",
            kind: PortKind::Outcome,
            description: "Authentication or authorization was denied; a 4xx response is prepared. Wire to client (or a custom denial handler).",
        },
        ERROR,
    ],
};

/// Interactive SSO plugins: denied rejections plus browser redirects.
pub const INTERACTIVE_AUTH_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "denied",
            kind: PortKind::Outcome,
            description: "Authentication was denied; a 4xx response is prepared. Wire to client.",
        },
        PortDecl {
            name: "redirect",
            kind: PortKind::Outcome,
            description: "The browser must move (login/logout/callback 3xx); response is prepared. Wire to client.",
        },
        ERROR,
    ],
};

/// Restriction and request-shape plugins: a deliberate policy rejection
/// (IP/UA/referer/consumer/group deny, blocked URI, missing/invalid CSRF
/// token, oversized body, schema mismatch) exits on `denied`. Same shape as
/// [`AUTH_SPEC`] but kept as its own const so the description can speak to
/// policy rejections rather than credentials.
pub const DENY_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "denied",
            kind: PortKind::Outcome,
            description: "The request was denied by a policy rule; a 4xx response is prepared. Wire to client.",
        },
        ERROR,
    ],
};

/// Traffic-control plugins (`rate-limit`, `limit-conn`, `limit-count`): a
/// throttled request exits on `limited`.
pub const LIMIT_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "limited",
            kind: PortKind::Outcome,
            description: "The request exceeded a traffic limit; a 429 response is prepared. Wire to client.",
        },
        ERROR,
    ],
};

/// `api-breaker`: the check phase's open-circuit short-circuit exits on
/// `broken`.
pub const BREAKER_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "broken",
            kind: PortKind::Outcome,
            description: "The circuit breaker is open; the break response is prepared. Wire to client.",
        },
        ERROR,
    ],
};

/// `workflow`: a rejecting `return` rule exits on `denied`; an exceeded
/// `limit-count` rule exits on `limited`.
pub const WORKFLOW_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "denied",
            kind: PortKind::Outcome,
            description: "A 'return' rule rejected the request; the configured response is prepared. Wire to client.",
        },
        PortDecl {
            name: "limited",
            kind: PortKind::Outcome,
            description: "A 'limit-count' rule's quota was exceeded; a rejection response is prepared. Wire to client.",
        },
        ERROR,
    ],
};

/// `traffic-split`: a request steered to and served by a weighted split
/// target exits on `routed`. `success` covers both "no rule matched" and
/// "the default slot was picked" — the request continues to the route's
/// normal upstream unchanged.
pub const TRAFFIC_SPLIT_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "routed",
            kind: PortKind::Outcome,
            description: "The request was steered to and served by a weighted split target; wire to client.",
        },
        ERROR,
    ],
};

/// `proxy-cache` (lookup phase): a cache hit exits on `hit`. `success`
/// covers a miss or a non-cacheable method/bypass — the request continues to
/// the upstream.
pub const PROXY_CACHE_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "hit",
            kind: PortKind::Outcome,
            description: "The response was served from cache; wire to client.",
        },
        ERROR,
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registered plugin type resolves to a spec, and every custom
    /// (non-default) output name is lowercase-kebab and non-reserved.
    #[test]
    fn test_every_known_type_has_a_valid_spec() {
        for ty in crate::plugins::KNOWN_PLUGIN_TYPES {
            let spec = crate::plugins::port_spec(ty)
                .unwrap_or_else(|| panic!("no port spec for '{ty}'"));
            for p in spec.outputs {
                if p.name != "success" && p.name != "error" {
                    assert!(!RESERVED_PORT_NAMES.contains(&p.name),
                        "'{ty}' declares reserved port '{}'", p.name);
                    assert!(p.name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                        "'{ty}' port '{}' is not lowercase-kebab", p.name);
                    assert!(matches!(p.kind, PortKind::Outcome),
                        "'{ty}' custom port '{}' must be kind outcome", p.name);
                }
                assert!(!p.description.is_empty(), "'{ty}' port '{}' lacks description", p.name);
            }
        }
    }

    #[test]
    fn test_structural_specs() {
        let l = crate::plugins::port_spec("listener").unwrap();
        assert!(l.input.is_none());
        assert_eq!(l.outputs.len(), 1);
        assert_eq!(l.outputs[0].name, "success");

        let c = crate::plugins::port_spec("client").unwrap();
        assert!(c.input.is_some());
        assert!(c.outputs.is_empty());

        assert!(crate::plugins::port_spec("no-such-type").is_none());
    }
}
