# MCP Server (backend) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The gateway serves a Model Context Protocol server (Streamable HTTP) on the Admin listener, behind scoped bearer tokens, exposing read/write tools over routes/policies/supernodes/plugin-configs/stores, debug traces and the sandbox, embedded documentation resources, and the precompiled debugging/authoring prompts — plus two Basic-Auth Admin endpoints (`/api/mcp/status`, `/api/mcp/prompts…`) the web UI will consume in the companion plan.

**Architecture:** A new `src/mcp/` module holds transport-independent pieces that compile in every build — `auth.rs` (bearer middleware), `tools/` (typed tool functions over `SharedState`), `docs.rs` (rust-embed'd Markdown pages), `prompts.rs` (template renderer) — and one feature-gated file, `server.rs`, that adapts them to `rmcp`'s `ServerHandler` and mounts `StreamableHttpService` at `admin.mcp.path` **outside** the Basic-Auth layer. Shared logic the Admin handlers currently hold inline (trace rendering/filtering, the sandbox pipeline, store referrers) is lifted into reusable functions so MCP and REST cannot drift.

**Tech Stack:** Rust (axum 0.8, tokio), `rmcp` 3.1 (`server`, `transport-streamable-http-server`; `client` + `transport-streamable-http-client-reqwest` as dev-dependency for tests), `schemars` 1, `subtle` 2, `rust-embed` 8, `serde_yaml`.

**Spec:** `docs/superpowers/specs/2026-08-30-mcp-server-design.md`

**Companion plan (UI):** `docs/superpowers/plans/2026-08-30-mcp-agent-ui.md` — depends on Task 10 of this plan.

## Global Constraints

- Work on branch `feature/mcp-server` off `develop`. Conventional Commits (`feat(mcp): …`, `refactor(debug): …`, `test(mcp): …`, `docs: …`), **no** `Co-Authored-By` trailer. Commit per task on the branch; do **not** push or open the PR until Francesco says so.
- Before every commit: `cargo fmt`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`. Every task must also keep `cargo check --no-default-features --locked` and `cargo clippy --all-targets --no-default-features --locked -- -D warnings` green (CI's `headless-check`).
- The dependency tree stays **ring-only**: after adding deps run `cargo tree -i aws-lc-sys` and expect *no output*. `cargo deny check` (via `./dev/sast.ps1 deny` or `cargo deny check`) must pass; allowed licenses are listed in `deny.toml` — do not add licenses without flagging it.
- `${ENV_VAR}` placeholders in `gateway.yaml` data are served **raw** over MCP exactly as the Admin API serves them; never call `interpolate_env`/`interpolate_env_json` on anything returned to an agent.
- Every config mutation goes through `state.config_store.clone().commit(&state, candidate)`; dry runs go through `state.validate_gateway(&candidate)`. Never write `state.gateway` directly.
- Never anonymous: `admin.mcp.enabled: true` with no tokens is a config-load error. Tokens are compared in constant time (`subtle::ConstantTimeEq`).
- Disabled/unbuilt MCP answers `404 {"error":"not_found"}` on the MCP path (the `/api/debug/*` convention). `GET /api/mcp/status` always answers.
- Existing Admin API behavior stays byte-for-byte identical through the extraction refactors in Task 3 — existing tests must pass unchanged.
- Do not commit the user's local files (`tests/oidc-test.yml`, `*.png`, the unrelated untracked `2026-07-24-sast-pipeline-design.md`).
- After the last task: `graphify update .`.

## Verified library facts (rmcp 3.1.4) — use these names, not memory

- Trait methods take `RequestContext<RoleServer>`; params types are **plural**: `CallToolRequestParams { name: Cow<'static,str>, arguments: Option<JsonObject>, .. }`, `PaginatedRequestParams`, `ReadResourceRequestParams { uri }`, `GetPromptRequestParams { name, arguments: Option<JsonObject> }`. `JsonObject = serde_json::Map<String, Value>`.
- `call_tool` returns `Result<CallToolResponse, ErrorData>`; build with `CallToolResult::success(vec![ContentBlock::text(..)]).into()` / `CallToolResult::error(..).into()`. `read_resource` → `ReadResourceResult::new(vec![ResourceContents::text(text, uri).with_mime_type("text/markdown")]).into()` (**text first, uri second**). `get_prompt` → `GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]).with_description(..).into()`.
- List results are exhaustive structs: `ListToolsResult::with_all_items(tools)`, `ListResourcesResult { resources, ..Default::default() }`, `ListResourceTemplatesResult { resource_templates, ..Default::default() }`, `ListPromptsResult { prompts, ..Default::default() }`.
- `Tool::new(name, description, Arc<JsonObject>)`; `Resource::new(uri, name).with_description(..)`; `ResourceTemplate::new(uri_template, name)`; `Prompt::new(name, Some(description), Some(vec![PromptArgument::new("x").with_description("..").with_required(true)]))`.
- `ServerInfo::new(ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build()).with_server_info(Implementation::new("featherbit", VERSION)).with_instructions(..)` — `ServerInfo` is `#[non_exhaustive]`, no struct literals.
- `ErrorData` (alias `McpError`): `invalid_params(msg, None)`, `invalid_request(msg, None)`, `internal_error(msg, None)`, `resource_not_found(msg, Some(json!({"uri": uri})))`.
- HTTP `Parts` reach handlers via `ctx.extensions.get::<http::request::Parts>()` and axum extensions set by middleware are in `parts.extensions`.
- `StreamableHttpService::new(|| Ok(handler.clone()), Arc::new(LocalSessionManager::default()), StreamableHttpServerConfig::default().disable_allowed_hosts().disable_allowed_origins())` — **defaults allow loopback `Host` only**, so hosts must be disabled (we validate `Origin` ourselves). Mount with `Router::route_service(path, service)`. Imports: `rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager}`.
- Test client: `StreamableHttpClientTransport::from_config(StreamableHttpClientTransportConfig::with_uri(url).auth_header(token))` (adds `Authorization: Bearer <token>`), `ClientInfo::new(ClientCapabilities::default(), Implementation::new("t","0")).serve(transport).await?` → `client.list_all_tools()`, `client.call_tool(CallToolRequestParams::new("x").with_arguments(obj))`, `client.list_prompts(None)`, `client.get_prompt(GetPromptRequestParams::new("p").with_arguments(obj))`, `client.read_resource(ReadResourceRequestParams::new(uri))`, `client.cancel()`. The client caches `list_tools` — use a fresh client per scope.

## File map

| Path | Responsibility |
|---|---|
| `Cargo.toml` | `mcp` feature (`dep:rmcp`), `rmcp`, `schemars`, `subtle`; `rust-embed` becomes non-optional; dev-dep `rmcp` client features |
| `src/config/system.rs` | `McpConfig`, `McpTokenConfig`, `McpScope`, validation; `AdminConfig.mcp` |
| `src/config/mod.rs` | re-export the three types |
| `src/debug/render.rs` (new) | `TraceFilter`, `apply_filter`, `render_trace` — moved out of `src/admin/debug.rs` |
| `src/debug/sandbox.rs` | `SandboxError`, `SandboxRun`, `run_sandbox(state, req)` — pipeline lifted out of the handler |
| `src/graph/prepare.rs` (new) | `prepare_policy(policy, supernodes, plugin_configs)` = validate → resolve config_refs → expand supernodes |
| `src/consumers/mod.rs` | `mask_credentials` |
| `src/admin/stores.rs` | `store_referrers` becomes `pub(crate)` |
| `src/mcp/mod.rs` | module root; `router()` (feature-gated) + `disabled_router()` |
| `src/mcp/auth.rs` | `McpPrincipal`, `McpAuthState`, `authenticate`, `bearer_middleware` |
| `src/mcp/tools/mod.rs` | `ToolError`, `ToolDef`, `tool_defs()`, `tool_def()`, `call()`, `parse_payload`, `schema_of` |
| `src/mcp/tools/catalog.rs` | `list_node_types`, `get_node_type`, `list_vars`, `get_status`, `export_config` |
| `src/mcp/tools/config.rs` | list/get for routes, policies, supernodes, plugin configs, stores, consumers; `validate_policy`, `validate_supernode` |
| `src/mcp/tools/debug.rs` | `list_traces`, `get_trace`, `get_trace_step`, `run_sandbox` |
| `src/mcp/tools/writes.rs` | `commit_candidate`, `put_*`/`delete_*`, `reload_config` |
| `src/mcp/docs.rs` | embedded docs, cleaning, URI ↔ page mapping |
| `src/mcp/prompts.rs` | `PromptDef`, `prompt_defs()`, `render()` |
| `src/mcp/server.rs` (feature `mcp`) | `McpServer: ServerHandler`; `build_service()`; integration tests |
| `src/admin/mod.rs` | mount MCP outside Basic Auth; `pub(crate) fn build_router` |
| `src/admin/mcp.rs` (new) | `GET /api/mcp/status`, `GET /api/mcp/prompts`, `GET /api/mcp/prompts/{name}` |
| `src/main.rs` | `mod mcp;` |
| `config/system.yaml` | commented example block |
| `.github/workflows/ci.yml` | "everything but MCP" check |
| `website/docs/guides/mcp.md`, `website/sidebars.ts`, `website/docs/reference/roadmap.md`, `CLAUDE.md`, `docs/apisix-parity.md` | documentation |

---

### Task 1: Dependencies, feature flag, and `admin.mcp` configuration

**Files:**
- Modify: `Cargo.toml:9-16` (features), `Cargo.toml:18-98` (deps), `Cargo.toml:100-109` (dev-deps)
- Modify: `src/config/system.rs:686-715` (`AdminConfig`), `src/config/system.rs:599-620` (`validate`), `src/config/system.rs:779+` (tests)
- Modify: `src/config/mod.rs:23-26` (re-exports)
- Modify: `src/main.rs` (module declaration)
- Create: `src/mcp/mod.rs` (empty module root for now)

**Interfaces:**
- Produces: `crate::config::{McpConfig, McpTokenConfig, McpScope}`; `McpScope::allows(self, required: McpScope) -> bool`; `McpScope::as_str(self) -> &'static str`; `McpConfig::validate(&self) -> Result<(), String>`; `AdminConfig.mcp: Option<McpConfig>`; `crate::config::MCP_MIN_TOKEN_LEN: usize = 16`.

- [ ] **Step 1: Branch**

```bash
git checkout develop && git pull && git checkout -b feature/mcp-server
```

- [ ] **Step 2: Add dependencies and the feature**

In `Cargo.toml` replace the `[features]` block with:

```toml
[features]
# The embedded admin web UI. Headless build (no UI assets, no serving code):
# `cargo build --release --no-default-features`.
default = ["ui", "redis-store", "mcp"]
ui = ["dep:mime_guess"]
# Redis/Valkey client for the `stores:` resource (sessions, distributed rate
# limiting). Off = declaring a store fails config load with a clear error.
redis-store = ["dep:redis"]
# Model Context Protocol server on the admin listener (`admin.mcp`). Off = the
# tool/prompt layer still compiles (the UI's "copy as agent prompt" uses it)
# but no MCP transport is mounted and `admin.mcp.enabled` is ignored.
mcp = ["dep:rmcp"]
```

Change the `rust-embed` line to non-optional (docs pages are embedded in every build) and add the new deps in `[dependencies]`:

```toml
# Embedded assets: the admin UI bundle (`ui` feature) and the documentation
# pages served to agents as MCP resources (every build).
rust-embed = { version = "8", features = ["include-exclude"] }
mime_guess = { version = "2", optional = true }

# MCP server (the `mcp` feature). Pure Rust, no TLS of its own — the ring-only
# rule is unaffected. `server` + `macros` are rmcp's defaults; the transport is
# the tower service mounted on the admin router.
rmcp = { version = "3.1", optional = true, default-features = false, features = ["server", "macros", "transport-streamable-http-server"] }
# JSON Schema for MCP tool inputs (same major as rmcp's re-export, so one copy).
schemars = "1"
# Constant-time token comparison for MCP bearer auth (already in the tree via rustls).
subtle = "2"
```

In `[dev-dependencies]` add:

```toml
# MCP client for in-process server tests (plain http, no TLS feature needed).
rmcp = { version = "3.1", default-features = false, features = ["client", "transport-streamable-http-client-reqwest"] }
```

- [ ] **Step 3: Verify the tree**

Run:
```bash
cargo fetch && cargo tree -i aws-lc-sys; cargo deny check licenses
```
Expected: `cargo tree -i aws-lc-sys` prints `error: package ID specification ... did not match any packages` (i.e. absent). `cargo deny` passes; if a new crate's license is not in `deny.toml`'s allow list, stop and report it rather than editing the list.

- [ ] **Step 4: Write the failing config tests**

Append to `mod tests` in `src/config/system.rs` (after `test_admin_ui_enabled_defaults_true`):

```rust
    fn admin_with_mcp(mcp_yaml: &str) -> AdminConfig {
        let yaml = format!("username: u\npassword: p\nmcp:\n{}", mcp_yaml);
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn test_admin_mcp_absent_by_default() {
        let cfg: AdminConfig = serde_yaml::from_str("username: u\npassword: p\n").unwrap();
        assert!(cfg.mcp.is_none());
    }

    #[test]
    fn test_mcp_defaults() {
        let cfg = admin_with_mcp("  enabled: false\n");
        let mcp = cfg.mcp.unwrap();
        assert!(!mcp.enabled);
        assert_eq!(mcp.path, "/mcp");
        assert!(mcp.tokens.is_empty());
        assert!(mcp.allowed_origins.is_empty());
        assert!(mcp.validate().is_ok());
    }

    #[test]
    fn test_mcp_enabled_requires_tokens() {
        let mcp = admin_with_mcp("  enabled: true\n").mcp.unwrap();
        let err = mcp.validate().unwrap_err();
        assert!(err.contains("admin.mcp.tokens must declare at least one token"), "{err}");
    }

    #[test]
    fn test_mcp_empty_token_rejected() {
        let mcp = admin_with_mcp(
            "  enabled: true\n  tokens:\n    - token: \"\"\n      scope: read\n",
        )
        .mcp
        .unwrap();
        let err = mcp.validate().unwrap_err();
        assert!(err.contains("admin.mcp.tokens[0].token is empty"), "{err}");
    }

    #[test]
    fn test_mcp_short_token_rejected() {
        let mcp = admin_with_mcp(
            "  enabled: true\n  tokens:\n    - token: short\n      scope: read\n",
        )
        .mcp
        .unwrap();
        let err = mcp.validate().unwrap_err();
        assert!(err.contains("admin.mcp.tokens[0].token must be at least 16 characters"), "{err}");
    }

    #[test]
    fn test_mcp_duplicate_token_rejected() {
        let mcp = admin_with_mcp(
            "  enabled: true\n  tokens:\n    - token: aaaaaaaaaaaaaaaaaaaa\n      scope: read\n    - token: aaaaaaaaaaaaaaaaaaaa\n      scope: write\n",
        )
        .mcp
        .unwrap();
        let err = mcp.validate().unwrap_err();
        assert!(err.contains("admin.mcp.tokens[1] duplicates tokens[0]"), "{err}");
    }

    #[test]
    fn test_mcp_bad_paths_rejected() {
        for bad in ["mcp", "/", "/api", "/api/mcp", "/healthz", "/readyz", "/metrics"] {
            let mcp = admin_with_mcp(&format!("  path: \"{bad}\"\n")).mcp.unwrap();
            let err = mcp.validate().unwrap_err();
            assert!(err.contains("admin.mcp.path"), "{bad}: {err}");
        }
    }

    #[test]
    fn test_mcp_valid_config_and_scope_semantics() {
        let mcp = admin_with_mcp(
            "  enabled: true\n  path: /agent\n  tokens:\n    - token: rrrrrrrrrrrrrrrrrrrr\n      scope: read\n      name: local\n    - token: wwwwwwwwwwwwwwwwwwww\n      scope: write\n  allowed_origins: [\"http://localhost:5173\"]\n",
        )
        .mcp
        .unwrap();
        assert!(mcp.validate().is_ok());
        assert_eq!(mcp.tokens[0].name.as_deref(), Some("local"));
        assert_eq!(mcp.tokens[1].name, None);
        assert!(McpScope::Write.allows(McpScope::Read));
        assert!(McpScope::Write.allows(McpScope::Write));
        assert!(McpScope::Read.allows(McpScope::Read));
        assert!(!McpScope::Read.allows(McpScope::Write));
        assert_eq!(McpScope::Read.as_str(), "read");
        assert_eq!(McpScope::Write.as_str(), "write");
    }

    #[test]
    fn test_system_validate_runs_mcp_validate() {
        let s: SystemConfig = serde_yaml::from_str(
            "admin:\n  username: u\n  password: p\n  mcp:\n    enabled: true\n",
        )
        .unwrap();
        assert!(s.validate().unwrap_err().contains("admin.mcp.tokens"));
    }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test --lib config::system::tests::test_mcp -- --nocapture`
Expected: compile error — `McpConfig`/`McpScope` and field `mcp` do not exist.

- [ ] **Step 6: Implement the config types**

In `src/config/system.rs`, add to `AdminConfig` (after `tls`):

```rust
    /// Model Context Protocol server for AI agents, served on this listener
    /// at `mcp.path` behind its own bearer tokens (never Basic Auth). `None`
    /// (the default) means no MCP. Parsed in every build; only honored when
    /// the binary is compiled with the `mcp` feature.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
```

Add after `AdminConfig`:

```rust
/// Minimum accepted length of an MCP bearer token, in characters.
pub const MCP_MIN_TOKEN_LEN: usize = 16;

/// `admin.mcp` — the MCP server exposed to agents.
#[derive(Debug, Deserialize, Clone)]
pub struct McpConfig {
    /// Master switch; off by default. Typically `${FEATHERBIT_MCP_ENABLED:-false}`.
    #[serde(default)]
    pub enabled: bool,
    /// Mount path on the admin listener; defaults to `/mcp`. Must be absolute
    /// and outside `/api`, `/healthz`, `/readyz`, `/metrics`.
    #[serde(default = "default_mcp_path")]
    pub path: String,
    /// Bearer tokens and their scopes. Required (non-empty) when `enabled`.
    #[serde(default)]
    pub tokens: Vec<McpTokenConfig>,
    /// Browser origins allowed to call the endpoint. A request carrying an
    /// `Origin` header not listed here is refused (DNS-rebinding defence);
    /// non-browser agents send no `Origin`, so the empty default costs nothing.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

/// One MCP bearer token.
#[derive(Debug, Deserialize, Clone)]
pub struct McpTokenConfig {
    /// The secret; usually `${FEATHERBIT_MCP_READ_TOKEN}`. At least 16 chars.
    pub token: String,
    /// `read` (list/get/validate/traces/sandbox) or `write` (also mutations).
    pub scope: McpScope,
    /// Optional label used in logs only; never returned by any endpoint.
    #[serde(default)]
    pub name: Option<String>,
}

/// What an MCP token may do. `write` implies `read`.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpScope {
    Read,
    Write,
}

impl McpScope {
    /// Whether a token with this scope may use a tool requiring `required`.
    pub fn allows(self, required: McpScope) -> bool {
        self == McpScope::Write || required == McpScope::Read
    }

    /// The wire/log spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            McpScope::Read => "read",
            McpScope::Write => "write",
        }
    }
}

fn default_mcp_path() -> String {
    "/mcp".to_string()
}

impl McpConfig {
    /// Fail-fast validation, run from [`SystemConfig::validate`].
    pub fn validate(&self) -> Result<(), String> {
        let reserved = ["/", "/api", "/healthz", "/readyz", "/metrics"];
        if !self.path.starts_with('/')
            || reserved.contains(&self.path.as_str())
            || self.path.starts_with("/api/")
        {
            return Err(format!(
                "admin.mcp.path '{}' must be an absolute path outside /api (and not /healthz, /readyz, /metrics)",
                self.path
            ));
        }
        if self.enabled && self.tokens.is_empty() {
            return Err(
                "admin.mcp.tokens must declare at least one token when admin.mcp.enabled is true"
                    .into(),
            );
        }
        for (i, t) in self.tokens.iter().enumerate() {
            if t.token.is_empty() {
                return Err(format!(
                    "admin.mcp.tokens[{i}].token is empty (is the environment variable set?)"
                ));
            }
            if t.token.chars().count() < MCP_MIN_TOKEN_LEN {
                return Err(format!(
                    "admin.mcp.tokens[{i}].token must be at least {MCP_MIN_TOKEN_LEN} characters"
                ));
            }
            if let Some(j) = self.tokens[..i].iter().position(|o| o.token == t.token) {
                return Err(format!("admin.mcp.tokens[{i}] duplicates tokens[{j}]"));
            }
        }
        Ok(())
    }
}
```

`Serialize` must be in scope: check the `use serde::...` line at the top of `system.rs`; if only `Deserialize` is imported, change it to `use serde::{Deserialize, Serialize};`.

In `SystemConfig::validate`, inside the existing `if let Some(admin) = &self.admin {` block, add before its closing brace:

```rust
            if let Some(mcp) = &admin.mcp {
                mcp.validate()?;
            }
```

In `src/config/mod.rs` extend the `system` re-export list with `McpConfig, McpScope, McpTokenConfig, MCP_MIN_TOKEN_LEN` (keep alphabetical order with the existing names).

- [ ] **Step 7: Create the module root and declare it**

Create `src/mcp/mod.rs`:

```rust
//! Model Context Protocol server for AI agents.
//!
//! Layout: [`auth`] (bearer tokens → scope), [`tools`] (the typed tool
//! functions over [`crate::state::SharedState`]), [`docs`] (documentation
//! pages embedded in the binary), [`prompts`] (precompiled debugging and
//! authoring prompts). Everything here compiles in every build — the Admin
//! API's `/api/mcp/prompts` uses the renderer even without a transport. Only
//! [`server`] (the `rmcp` adapter and the mounted Streamable HTTP service)
//! sits behind the `mcp` cargo feature.
```

In `src/main.rs`, next to the other `mod` declarations (alphabetical), add `mod mcp;`.

- [ ] **Step 8: Run the tests**

Run: `cargo test --lib config::system::tests -- --nocapture` then `cargo check --no-default-features --locked`.
Expected: all pass (the new module is empty; `#![allow]` not needed yet — if clippy complains about the empty module, it will not: an empty module with a doc comment is fine).

- [ ] **Step 9: Commit**

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
git add Cargo.toml Cargo.lock src/config/system.rs src/config/mod.rs src/main.rs src/mcp/mod.rs
git commit -m "feat(config): admin.mcp block with scoped bearer tokens; rmcp/schemars/subtle deps behind the mcp feature"
```

---

### Task 2: Bearer-token middleware (`src/mcp/auth.rs`)

**Files:**
- Create: `src/mcp/auth.rs`
- Modify: `src/mcp/mod.rs` (`pub mod auth;`)

**Interfaces:**
- Consumes: `crate::config::{McpConfig, McpScope}`.
- Produces: `McpPrincipal { name: Option<String>, scope: McpScope }` (Clone, Debug, PartialEq); `McpAuthState::from_config(&McpConfig) -> Self`; `enum AuthFailure { OriginNotAllowed, Unauthorized }`; `fn authenticate(&McpAuthState, &HeaderMap) -> Result<McpPrincipal, AuthFailure>`; `async fn bearer_middleware(State<Arc<McpAuthState>>, Request<Body>, Next) -> Response` (inserts `McpPrincipal` into request extensions on success).

- [ ] **Step 1: Write the failing tests**

Create `src/mcp/auth.rs` with only the test module for now:

```rust
//! Bearer-token authentication for the MCP endpoint.
//!
//! Separate from the Admin API's Basic Auth on purpose: an agent gets a
//! narrower credential (`read` or `write`) that is useless on `/api/*`, and
//! the Admin credentials are useless here. The token is resolved on **every**
//! request (never cached on the MCP session) so a client cannot keep a scope
//! it no longer presents.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpConfig;
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    const READ: &str = "read-token-0123456789";
    const WRITE: &str = "write-token-0123456789";

    fn cfg() -> McpConfig {
        serde_yaml::from_str(&format!(
            "enabled: true\ntokens:\n  - token: {READ}\n    scope: read\n    name: local\n  - token: {WRITE}\n    scope: write\nallowed_origins: [\"http://localhost:5173\"]\n"
        ))
        .unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn read_token_yields_read_principal_with_name() {
        let auth = McpAuthState::from_config(&cfg());
        let p = authenticate(&auth, &headers(&[("authorization", &format!("Bearer {READ}"))])).unwrap();
        assert_eq!(p.scope, McpScope::Read);
        assert_eq!(p.name.as_deref(), Some("local"));
    }

    #[test]
    fn write_token_yields_write_principal_without_name() {
        let auth = McpAuthState::from_config(&cfg());
        let p = authenticate(&auth, &headers(&[("authorization", &format!("bearer {WRITE}"))])).unwrap();
        assert_eq!(p.scope, McpScope::Write);
        assert_eq!(p.name, None);
    }

    #[test]
    fn missing_malformed_and_unknown_tokens_are_unauthorized() {
        let auth = McpAuthState::from_config(&cfg());
        assert_eq!(authenticate(&auth, &headers(&[])), Err(AuthFailure::Unauthorized));
        assert_eq!(
            authenticate(&auth, &headers(&[("authorization", "Basic dTpw")])),
            Err(AuthFailure::Unauthorized)
        );
        assert_eq!(
            authenticate(&auth, &headers(&[("authorization", "Bearer nope-nope-nope-nope")])),
            Err(AuthFailure::Unauthorized)
        );
        // A prefix of a real token must not match.
        assert_eq!(
            authenticate(&auth, &headers(&[("authorization", "Bearer read-token-012345678")])),
            Err(AuthFailure::Unauthorized)
        );
    }

    #[test]
    fn origin_is_checked_before_the_token() {
        let auth = McpAuthState::from_config(&cfg());
        let bad = headers(&[("origin", "http://evil.example"), ("authorization", &format!("Bearer {READ}"))]);
        assert_eq!(authenticate(&auth, &bad), Err(AuthFailure::OriginNotAllowed));
        let ok = headers(&[("origin", "http://localhost:5173"), ("authorization", &format!("Bearer {READ}"))]);
        assert!(authenticate(&auth, &ok).is_ok());
        // With no allowed origins, ANY Origin header is refused.
        let mut none = cfg();
        none.allowed_origins.clear();
        let auth = McpAuthState::from_config(&none);
        assert_eq!(authenticate(&auth, &ok), Err(AuthFailure::OriginNotAllowed));
    }

    async fn echo_scope(req: Request<Body>) -> String {
        req.extensions()
            .get::<McpPrincipal>()
            .map(|p| p.scope.as_str().to_string())
            .unwrap_or_else(|| "none".into())
    }

    fn app() -> Router {
        Router::new().route("/mcp", get(echo_scope)).route_layer(
            axum::middleware::from_fn_with_state(
                Arc::new(McpAuthState::from_config(&cfg())),
                bearer_middleware,
            ),
        )
    }

    #[tokio::test]
    async fn middleware_inserts_principal_and_rejects_properly() {
        let resp = app()
            .oneshot(
                Request::get("/mcp")
                    .header("authorization", format!("Bearer {WRITE}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"write");

        let resp = app()
            .oneshot(Request::get("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get("www-authenticate").unwrap(),
            "Bearer realm=\"featherbit-mcp\""
        );

        let resp = app()
            .oneshot(
                Request::get("/mcp")
                    .header("origin", "http://evil.example")
                    .header("authorization", format!("Bearer {WRITE}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
```

Add `pub mod auth;` to `src/mcp/mod.rs`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib mcp::auth`
Expected: compile errors (`McpAuthState`, `authenticate`, … undefined).

- [ ] **Step 3: Implement**

Insert above the test module in `src/mcp/auth.rs`:

```rust
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use subtle::ConstantTimeEq;

use crate::config::{McpConfig, McpScope};

/// The identity behind an authenticated MCP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPrincipal {
    /// The token's optional label (`admin.mcp.tokens[].name`), for logs.
    pub name: Option<String>,
    /// What this request may do.
    pub scope: McpScope,
}

/// Configured tokens, ready for constant-time lookup.
pub struct McpAuthState {
    tokens: Vec<(Vec<u8>, McpPrincipal)>,
    allowed_origins: Vec<String>,
}

impl McpAuthState {
    /// Builds the lookup table from validated config.
    pub fn from_config(cfg: &McpConfig) -> Self {
        Self {
            tokens: cfg
                .tokens
                .iter()
                .map(|t| {
                    (
                        t.token.as_bytes().to_vec(),
                        McpPrincipal {
                            name: t.name.clone(),
                            scope: t.scope,
                        },
                    )
                })
                .collect(),
            allowed_origins: cfg.allowed_origins.clone(),
        }
    }
}

/// Why a request was refused. Deliberately coarse: callers must not leak
/// whether a token was unknown, malformed, or absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// An `Origin` header was present and not allow-listed.
    OriginNotAllowed,
    /// No usable bearer token.
    Unauthorized,
}

/// Resolves the principal for a request from its headers.
pub fn authenticate(auth: &McpAuthState, headers: &HeaderMap) -> Result<McpPrincipal, AuthFailure> {
    if let Some(origin) = headers.get("origin") {
        let allowed = origin
            .to_str()
            .map(|o| auth.allowed_origins.iter().any(|a| a == o))
            .unwrap_or(false);
        if !allowed {
            return Err(AuthFailure::OriginNotAllowed);
        }
    }

    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            // RFC 6750: the scheme is case-insensitive.
            let (scheme, rest) = h.split_at_checked(7)?;
            scheme.eq_ignore_ascii_case("Bearer ").then(|| rest.trim())
        })
        .filter(|t| !t.is_empty())
        .ok_or(AuthFailure::Unauthorized)?;

    // Compare against every configured token without early exit so timing
    // does not reveal which entry (if any) matched.
    let mut matched: Option<McpPrincipal> = None;
    for (token, principal) in &auth.tokens {
        let same_len = token.len() == presented.len();
        let eq = same_len && bool::from(token.as_slice().ct_eq(presented.as_bytes()));
        if eq && matched.is_none() {
            matched = Some(principal.clone());
        }
    }
    matched.ok_or(AuthFailure::Unauthorized)
}

/// axum middleware for the MCP path: authenticates, then stores the
/// [`McpPrincipal`] in request extensions for the server handler to read.
pub async fn bearer_middleware(
    State(auth): State<Arc<McpAuthState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    match authenticate(&auth, req.headers()) {
        Ok(principal) => {
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Err(AuthFailure::OriginNotAllowed) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "origin_not_allowed"})),
        )
            .into_response(),
        Err(AuthFailure::Unauthorized) => (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Bearer realm=\"featherbit-mcp\"")],
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response(),
    }
}
```

Note `str::split_at_checked` is stable since Rust 1.80 — fine on the pinned toolchain.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib mcp::auth`
Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
git add src/mcp/auth.rs src/mcp/mod.rs
git commit -m "feat(mcp): bearer-token middleware with constant-time lookup and Origin allow-list"
```

---

### Task 3: Lift shared logic out of the Admin handlers (no behavior change)

Four extractions so the MCP tools call the same code the REST handlers call. Each keeps the Admin API's responses byte-identical; the existing tests in `src/admin/debug.rs`, `src/admin/stores.rs`, `src/state.rs` are the regression net.

**Files:**
- Create: `src/debug/render.rs`; Modify: `src/debug/mod.rs`, `src/admin/debug.rs:77-161`
- Create: `src/graph/prepare.rs`; Modify: `src/graph/mod.rs:14-17`
- Modify: `src/debug/sandbox.rs`, `src/admin/debug.rs:212-343`
- Modify: `src/consumers/mod.rs`
- Modify: `src/admin/stores.rs:229` (`store_referrers` visibility)

**Interfaces:**
- Produces: `crate::debug::render::{TraceFilter, apply_filter, render_trace}` — `pub struct TraceFilter { pub route: Option<String>, pub policy: Option<String>, pub status: Option<u16>, pub source: Option<String>, pub limit: Option<usize> }` (Deserialize + Default), `pub fn apply_filter(traces: Vec<TraceSummary>, f: &TraceFilter) -> Vec<TraceSummary>`, `pub fn render_trace(trace: &Trace) -> serde_json::Value` (steps carry a `changes: Vec<Change>` array).
- `crate::graph::prepare_policy(policy: PolicyConfig, supernodes: &[SupernodeConfig], plugin_configs: &[PluginConfigDef]) -> Result<PolicyConfig, String>` — validate → resolve `config_ref` → expand supernodes; returns the compile-ready policy. Errors are the same strings the handlers produce today (`validate_policy` errors joined with `"; "`).
- `crate::debug::sandbox::{SandboxError, SandboxRun, run_sandbox}` — `pub enum SandboxError { Disabled, SandboxDisabled, BadRequest(String), UnknownPolicy(String), Timeout(u64) }`, `pub struct SandboxRun { pub mode: &'static str, pub policy: String, pub stored_trace_id: String, pub trace: serde_json::Value }`, `pub async fn run_sandbox(state: &SharedState, req: SandboxRequest) -> Result<SandboxRun, SandboxError>`.
- `crate::consumers::mask_credentials(c: &ConsumerConfig) -> ConsumerConfig` — every string/number/bool leaf under `credentials` becomes `"<masked>"` except values whose key is `username` or `access_key` (the identifying halves of `basic-auth`/`hmac-auth`).
- `crate::admin::stores::store_referrers` becomes `pub(crate)`.

- [ ] **Step 1: Failing tests for `render`**

Create `src/debug/render.rs` with the test module first:

```rust
//! Trace presentation shared by the Admin API and the MCP tools: list
//! filtering and the rendered trace shape (each step with its `changes`).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::{StepOutcome, TraceSource};

    fn summary(policy: &str, status: u16, source: TraceSource) -> TraceSummary {
        TraceSummary {
            id: format!("{policy}-{status}"),
            seq: 1,
            source,
            started_ms: 0,
            route: Some("r".into()),
            policy: policy.into(),
            method: "GET".into(),
            path: "/".into(),
            status,
            duration_us: 1,
            step_count: 1,
            error_count: 0,
            captured_bodies: false,
        }
    }

    #[test]
    fn filter_ands_fields_and_ignores_blank_strings() {
        let all = vec![
            summary("a", 200, TraceSource::Request),
            summary("a", 500, TraceSource::Sandbox),
            summary("b", 200, TraceSource::Request),
        ];
        let f = TraceFilter { policy: Some("a".into()), status: Some(200), ..Default::default() };
        assert_eq!(apply_filter(all.clone(), &f).len(), 1);
        let f = TraceFilter { source: Some("SANDBOX".into()), ..Default::default() };
        assert_eq!(apply_filter(all.clone(), &f)[0].id, "a-500");
        let f = TraceFilter { route: Some("  ".into()), limit: Some(2), ..Default::default() };
        assert_eq!(apply_filter(all, &f).len(), 2);
    }

    #[test]
    fn render_attaches_changes_per_step() {
        let ctx = crate::context::Context::default();
        let opts = crate::debug::CaptureOptions::default();
        let mut rec = crate::debug::TraceRecorder::new(&ctx, opts, 10);
        let mut after = crate::context::Context::default();
        after.response.status_code = 418;
        rec.record_step(
            "n1",
            "echo",
            StepOutcome::Success,
            std::time::Duration::from_micros(5),
            crate::debug::EdgeKind::Success,
            Some("success"),
            None,
            &after,
        );
        let trace = rec.finish(
            "t1".into(),
            1,
            TraceSource::Request,
            None,
            "p".into(),
            &after,
            std::time::Duration::from_micros(7),
        );
        let v = render_trace(&trace);
        let steps = v["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        let changes = steps[0]["changes"].as_array().unwrap();
        assert!(changes.iter().any(|c| c["path"] == "response.status_code"), "{changes:?}");
        assert_eq!(steps[0]["node_id"], "n1");
    }
}
```

If `Context::default()` does not exist, look at how `src/debug/sandbox.rs` builds a context in its tests (`SandboxContextInput::default().into_context().unwrap()`) and use that instead in both places. If `CaptureOptions` has no `Default`, construct it as `CaptureOptions { capture_bodies: false, max_body_bytes: 8192, redaction: RedactionPolicy::new(&[], &[], &[]) }`.

- [ ] **Step 2: Move the code**

Cut from `src/admin/debug.rs` the items `TraceFilter` (lines ≈77-86), `non_empty` (≈88-90), `apply_filter` (≈100-117), `StepWithChanges` and `render_trace` (≈136-161) and paste them above the test module in `src/debug/render.rs`, making them public:

```rust
use serde::{Deserialize, Serialize};

use crate::debug::diff::{diff, Change};
use crate::debug::store::TraceSummary;
use crate::debug::{NodeStep, Trace};

/// Optional filters for the trace list. All are ANDed; blank strings are
/// ignored (a bare `?route=` is not a filter).
#[derive(Debug, Default, Deserialize)]
pub struct TraceFilter {
    pub route: Option<String>,
    pub policy: Option<String>,
    pub status: Option<u16>,
    /// `request` or `sandbox` (case-insensitive).
    pub source: Option<String>,
    /// Cap on the number of rows returned, applied after filtering.
    pub limit: Option<usize>,
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Applies `f` to a newest-first summary list.
pub fn apply_filter(traces: Vec<TraceSummary>, f: &TraceFilter) -> Vec<TraceSummary> {
    // (body moved verbatim from src/admin/debug.rs)
}

#[derive(Serialize)]
struct StepWithChanges<'a> {
    #[serde(flatten)]
    step: &'a NodeStep,
    changes: Vec<Change>,
}

/// Serializes a trace with a derived `changes` array on every step.
pub fn render_trace(trace: &Trace) -> serde_json::Value {
    // (body moved verbatim from src/admin/debug.rs)
}
```

Keep the moved bodies verbatim. In `src/debug/mod.rs` add `pub mod render;`. In `src/admin/debug.rs` replace the removed items with `use crate::debug::render::{apply_filter, render_trace, TraceFilter};` and delete the now-unused `Serialize`, `Change`, `diff`, `NodeStep` imports.

- [ ] **Step 3: Run**

Run: `cargo test --lib debug:: && cargo test --lib admin::debug`
Expected: new tests pass; every existing admin debug test still passes.

- [ ] **Step 4: Failing test for `prepare_policy`**

Create `src/graph/prepare.rs`:

```rust
//! The pre-compile pipeline shared by the sandbox, the MCP `validate_policy`
//! tool and config apply: structural validation, `config_ref` resolution and
//! supernode expansion, in that order.

use crate::config::{GatewayConfig, PluginConfigDef, PolicyConfig, SupernodeConfig};
use crate::graph::{expand_policy, validate_policy};

/// Turns a stored policy into the compile-ready form.
///
/// Errors are the human-readable strings the Admin API already returns:
/// `validate_policy` violations joined with `"; "`, then the first
/// resolution or expansion error.
pub fn prepare_policy(
    policy: PolicyConfig,
    supernodes: &[SupernodeConfig],
    plugin_configs: &[PluginConfigDef],
) -> Result<PolicyConfig, String> {
    if let Err(errors) = validate_policy(&policy) {
        return Err(errors.join("; "));
    }
    let mut tmp: GatewayConfig = serde_yaml::from_str("{}").expect("empty config parses");
    tmp.policies = vec![policy];
    tmp.supernodes = supernodes.to_vec();
    tmp.plugin_configs = plugin_configs.to_vec();
    let resolved = crate::config::resolve_plugin_configs(&tmp)?;
    for warning in crate::config::collect_template_warnings(&resolved) {
        tracing::warn!("{warning}");
    }
    let policy = resolved
        .policies
        .into_iter()
        .next()
        .expect("one policy in, one policy out");
    expand_policy(&policy, &resolved.supernodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(yaml: &str) -> PolicyConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn rejects_structural_errors_with_joined_messages() {
        let p = policy("name: p\nnodes:\n  - id: l\n    type: listener\nedges: []\n");
        let err = prepare_policy(p, &[], &[]).unwrap_err();
        assert!(err.contains("client"), "{err}");
    }

    #[test]
    fn resolves_config_ref_and_passes_valid_policy_through() {
        let p = policy(
            "name: p\nnodes:\n  - id: l\n    type: listener\n  - id: e\n    type: echo\n    config_ref: shared-echo\n  - id: c\n    type: client\nedges:\n  - from: l.out\n    to: e.in\n  - from: e.out\n    to: c.in\n",
        );
        let pc: PluginConfigDef = serde_yaml::from_str(
            "name: shared-echo\ntype: echo\nconfig:\n  body: hi\n",
        )
        .unwrap();
        let out = prepare_policy(p, &[], &[pc]).unwrap();
        let echo = out.nodes.iter().find(|n| n.id == "e").unwrap();
        assert_eq!(echo.config.get("body").and_then(|v| v.as_str()), Some("hi"));
    }

    #[test]
    fn unknown_config_ref_is_an_error() {
        let p = policy(
            "name: p\nnodes:\n  - id: l\n    type: listener\n  - id: e\n    type: echo\n    config_ref: nope\n  - id: c\n    type: client\nedges:\n  - from: l.out\n    to: e.in\n  - from: e.out\n    to: c.in\n",
        );
        let err = prepare_policy(p, &[], &[]).unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }
}
```

Add to `src/graph/mod.rs`: `mod prepare;` and `pub use prepare::prepare_policy;`. If the `echo` plugin's config key is not `body`, check `src/plugins/native/echo.rs` and use its actual key (the assertion only needs *some* key to round-trip).

- [ ] **Step 5: Run**

Run: `cargo test --lib graph::prepare`
Expected: 3 pass.

- [ ] **Step 6: Failing test for the sandbox runner, then extract**

In `src/debug/sandbox.rs` append to its existing `#[cfg(test)] mod tests` (or create one):

```rust
    #[tokio::test]
    async fn run_sandbox_reports_disabled_and_unknown_policy() {
        use crate::config::{GatewayConfig, SystemConfig};
        use crate::config_store::FileConfigStore;
        use std::sync::Arc;

        let off: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        let store = Arc::new(FileConfigStore::new(std::path::PathBuf::from("gateway.yaml")));
        let state = crate::state::SharedState::new(off, gw.clone(), None, store.clone()).unwrap();
        let req = SandboxRequest { policy: Some("p".into()), ..Default::default() };
        assert!(matches!(run_sandbox(&state, req).await, Err(SandboxError::Disabled)));

        let on: SystemConfig = serde_yaml::from_str("debug:\n  enabled: true\n").unwrap();
        let state = crate::state::SharedState::new(on, gw, None, store).unwrap();
        let req = SandboxRequest { policy: Some("missing".into()), ..Default::default() };
        assert!(matches!(run_sandbox(&state, req).await, Err(SandboxError::UnknownPolicy(_))));
        let req = SandboxRequest::default();
        assert!(matches!(run_sandbox(&state, req).await, Err(SandboxError::BadRequest(_))));
    }

    #[tokio::test]
    async fn run_sandbox_executes_nodes_mode() {
        use crate::config::{GatewayConfig, SystemConfig};
        use crate::config_store::FileConfigStore;
        use std::sync::Arc;
        let on: SystemConfig = serde_yaml::from_str("debug:\n  enabled: true\n").unwrap();
        let gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        let store = Arc::new(FileConfigStore::new(std::path::PathBuf::from("gateway.yaml")));
        let state = crate::state::SharedState::new(on, gw, None, store).unwrap();
        let req: SandboxRequest = serde_json::from_value(serde_json::json!({
            "nodes": [{"id": "e", "type": "echo", "config": {}}],
            "context": {"method": "GET", "path": "/x"}
        }))
        .unwrap();
        let run = run_sandbox(&state, req).await.unwrap();
        assert_eq!(run.mode, "nodes");
        assert_eq!(run.policy, "__sandbox");
        assert!(state.debug.get(&run.stored_trace_id).is_some());
        assert!(run.trace["steps"].as_array().unwrap().len() >= 1);
    }
```

Then add to `src/debug/sandbox.rs` (above the tests):

```rust
use std::time::{Duration, Instant};

use crate::debug::render::render_trace;
use crate::debug::{new_trace_id, TraceRecorder, TraceSource};
use crate::graph::{compile_policy, prepare_policy};
use crate::state::SharedState;

/// Why a sandbox run did not happen. The Admin handler maps these to HTTP;
/// the MCP tool maps them to tool-error codes.
#[derive(Debug)]
pub enum SandboxError {
    /// `debug.enabled` is false.
    Disabled,
    /// `debug.sandbox` is false.
    SandboxDisabled,
    /// Malformed request (both/neither of `nodes`/`policy`, bad node list,
    /// invalid policy, unmaterializable context) — message is user-facing.
    BadRequest(String),
    /// `policy` names no stored policy.
    UnknownPolicy(String),
    /// The run exceeded `debug.sandbox_timeout_seconds` (the value carried).
    Timeout(u64),
}

/// A completed sandbox run.
pub struct SandboxRun {
    /// `"nodes"` or `"policy"`.
    pub mode: &'static str,
    /// The policy name executed (`__sandbox` for ad-hoc node lists).
    pub policy: String,
    /// Id of the trace stored in the debug ring buffer.
    pub stored_trace_id: String,
    /// The rendered trace (`render_trace` shape).
    pub trace: serde_json::Value,
}

/// Runs plugins or a stored policy against a synthetic request, for real,
/// recording a trace. Shared by `POST /api/debug/sandbox` and the MCP
/// `run_sandbox` tool.
pub async fn run_sandbox(state: &SharedState, req: SandboxRequest) -> Result<SandboxRun, SandboxError> {
    if !state.debug.enabled {
        return Err(SandboxError::Disabled);
    }
    if !state.debug.sandbox_enabled {
        return Err(SandboxError::SandboxDisabled);
    }

    let (mode, policy_name, policy) = match (req.nodes, req.policy) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(SandboxError::BadRequest(
                "provide exactly one of 'nodes' or 'policy'".into(),
            ))
        }
        (Some(nodes), None) => match synthesize_policy(nodes, req.on_error) {
            Ok(p) => ("nodes", "__sandbox".to_string(), p),
            Err(e) => return Err(SandboxError::BadRequest(e)),
        },
        (None, Some(name)) => {
            let gw = state.gateway.read().await;
            match gw.policies.iter().find(|p| p.name == name) {
                Some(p) => ("policy", name.clone(), p.clone()),
                None => return Err(SandboxError::UnknownPolicy(name)),
            }
        }
    };

    let (supernodes, plugin_configs) = {
        let gw = state.gateway.read().await;
        (gw.supernodes.clone(), gw.plugin_configs.clone())
    };
    let policy = prepare_policy(policy, &supernodes, &plugin_configs).map_err(SandboxError::BadRequest)?;
    let graph = compile_policy(&policy, state.resources.clone()).map_err(SandboxError::BadRequest)?;
    let ctx = materialize_context(req.context).map_err(SandboxError::BadRequest)?;

    tracing::warn!("sandbox run ({}): plugins execute for real against live resources", mode);

    let recorder = TraceRecorder::new(&ctx, state.debug.capture_options(), state.debug.max_steps);
    let started = Instant::now();
    let timeout = Duration::from_secs(state.debug.sandbox_timeout_seconds.max(1));
    let (out_ctx, recorder) = tokio::time::timeout(timeout, graph.execute_traced(ctx, recorder))
        .await
        .map_err(|_| SandboxError::Timeout(state.debug.sandbox_timeout_seconds))?;

    let id = new_trace_id();
    let trace = recorder.finish(
        id.clone(),
        state.debug.next_seq(),
        TraceSource::Sandbox,
        None,
        policy_name.clone(),
        &out_ctx,
        started.elapsed(),
    );
    let rendered = render_trace(&trace);
    state.debug.record(trace);
    Ok(SandboxRun { mode, policy: policy_name, stored_trace_id: id, trace: rendered })
}
```

Rewrite `run_sandbox` in `src/admin/debug.rs` to a thin mapper (same status codes and bodies as today):

```rust
async fn run_sandbox(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<SandboxRequest>,
) -> impl IntoResponse {
    use crate::debug::sandbox::{run_sandbox as run, SandboxError};
    match run(&state, req).await {
        Ok(r) => Json(serde_json::json!({
            "mode": r.mode,
            "policy": r.policy,
            "warning": "plugins executed for real: outbound calls were made and shared \
                        rate-limit/breaker state was mutated",
            "stored_trace_id": r.stored_trace_id,
            "trace": r.trace,
        }))
        .into_response(),
        Err(SandboxError::Disabled) => disabled("sandbox"),
        Err(SandboxError::SandboxDisabled) => disabled("sandbox (debug.sandbox is false)"),
        Err(SandboxError::BadRequest(e)) => bad_request(e),
        Err(SandboxError::UnknownPolicy(name)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("unknown policy '{name}'")})),
        )
            .into_response(),
        Err(SandboxError::Timeout(secs)) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({
                "error": "sandbox_timeout",
                "message": format!("run exceeded debug.sandbox_timeout_seconds ({}s)", secs),
            })),
        )
            .into_response(),
    }
}
```

Remove imports in `src/admin/debug.rs` that became unused (`Duration`, `Instant`, `compile_policy`, `validate_policy`, `synthesize_policy`, `TraceRecorder`, …) — clippy `-D warnings` will list them.

- [ ] **Step 7: Run**

Run: `cargo test --lib debug:: && cargo test --lib admin::debug`
Expected: all pass, including the pre-existing sandbox handler tests.

- [ ] **Step 8: `mask_credentials` + `store_referrers` visibility**

Append to `src/consumers/mod.rs` (above its tests):

```rust
/// Returns a copy of `c` with credential secrets replaced by `"<masked>"`.
///
/// Every scalar leaf under `credentials` is masked except the identifying
/// halves of a credential pair (`username`, `access_key`), so a read-only
/// viewer can still tell *which* credential exists without learning it.
pub fn mask_credentials(c: &ConsumerConfig) -> ConsumerConfig {
    fn mask(v: &serde_json::Value, key: Option<&str>) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter().map(|(k, v)| (k.clone(), mask(v, Some(k)))).collect(),
            ),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(|v| mask(v, key)).collect())
            }
            serde_json::Value::Null => serde_json::Value::Null,
            _ if matches!(key, Some("username") | Some("access_key")) => v.clone(),
            _ => serde_json::Value::String("<masked>".into()),
        }
    }
    let mut out = c.clone();
    out.credentials = c
        .credentials
        .iter()
        .map(|(plugin, v)| (plugin.clone(), mask(v, None)))
        .collect();
    out
}
```

with test:

```rust
    #[test]
    fn mask_credentials_keeps_identifiers_only() {
        let c: ConsumerConfig = serde_yaml::from_str(
            "name: alice\ncredentials:\n  key-auth:\n    key: s3cret\n  basic-auth:\n    username: alice\n    password: pw\n  hmac-auth:\n    access_key: ak\n    secret_key: sk\n    nested: {inner: x}\n",
        )
        .unwrap();
        let m = mask_credentials(&c);
        assert_eq!(m.name, "alice");
        assert_eq!(m.credentials["key-auth"]["key"], "<masked>");
        assert_eq!(m.credentials["basic-auth"]["username"], "alice");
        assert_eq!(m.credentials["basic-auth"]["password"], "<masked>");
        assert_eq!(m.credentials["hmac-auth"]["access_key"], "ak");
        assert_eq!(m.credentials["hmac-auth"]["secret_key"], "<masked>");
        assert_eq!(m.credentials["hmac-auth"]["nested"]["inner"], "<masked>");
        // The original is untouched.
        assert_eq!(c.credentials["key-auth"]["key"], "s3cret");
    }
```

In `src/admin/stores.rs` change `fn store_referrers(` to `pub(crate) fn store_referrers(`.

- [ ] **Step 9: Run everything and commit**

Run: `cargo test --locked && cargo check --no-default-features --locked`
Expected: green.

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
git add src/debug src/graph src/admin/debug.rs src/admin/stores.rs src/consumers/mod.rs
git commit -m "refactor(debug): share trace rendering, sandbox runner, policy preparation and credential masking with the MCP layer"
```

---

### Task 4: Tool layer core + catalog/config read tools

**Files:**
- Create: `src/mcp/tools/mod.rs`, `src/mcp/tools/catalog.rs`, `src/mcp/tools/config.rs`
- Modify: `src/mcp/mod.rs` (`pub mod tools;`), `src/admin/policies.rs:174` (`plugin_catalog` → `pub(crate)`)

**Interfaces:**
- Consumes: `crate::admin::policies::plugin_catalog() -> Vec<Value>` (made `pub(crate)`), `crate::plugins::port_spec`, `crate::vars::catalog::var_catalog`, `crate::graph::{prepare_policy, compile_policy, validate_supernode}`, `crate::consumers::mask_credentials`, `crate::mcp::docs::plugin_page` (Task 7 — until then `get_node_type` returns `docs: null`; Task 7 fills it in).
- Produces:
  - `pub type JsonObject = serde_json::Map<String, serde_json::Value>;`
  - `pub struct ToolError { pub code: &'static str, pub message: String, pub errors: Vec<String>, pub hint: Option<String> }` with constructors `not_found(what, name)`, `invalid_input(msg)`, `invalid_config(errors: Vec<String>)`, `debug_disabled()`, `sandbox_disabled()`, `forbidden(have: McpScope)`, `store_error(msg)`, `unknown_tool(name)`, `internal(msg)`; `fn to_json(&self) -> Value`.
  - `pub struct ToolDef { pub name: &'static str, pub scope: McpScope, pub description: &'static str, pub input_schema: fn() -> JsonObject }`; `pub fn tool_defs() -> &'static [ToolDef]`; `pub fn tool_def(name) -> Option<&'static ToolDef>`; `pub async fn call(state: &SharedState, name: &str, args: JsonObject) -> Result<Value, ToolError>` (unknown name → `unknown_tool`; does **not** check scope — the server does).
  - `pub fn parse_payload<T: DeserializeOwned>(v: Value, what: &str) -> Result<T, ToolError>` (YAML string or JSON object); `pub fn schema_of<T: schemars::JsonSchema>() -> JsonObject`; `fn args<T: DeserializeOwned>(a: JsonObject) -> Result<T, ToolError>`.

- [ ] **Step 1: Failing tests for the core**

Create `src/mcp/tools/mod.rs`:

```rust
//! The MCP tool layer: typed functions over [`SharedState`] plus the registry
//! the server advertises. Transport-agnostic — the `rmcp` adapter in
//! `server.rs` and the Admin API's prompt renderer both call into here.

pub mod catalog;
pub mod config;

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::McpScope;
use crate::state::SharedState;

/// A JSON object, as MCP tool arguments arrive.
pub type JsonObject = serde_json::Map<String, Value>;

/// A domain failure surfaced to the agent as an MCP tool error (never a
/// JSON-RPC error): `{code, message, errors?, hint?}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    pub errors: Vec<String>,
    pub hint: Option<String>,
}

impl ToolError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), errors: Vec::new(), hint: None }
    }
    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn not_found(what: &str, name: &str) -> Self {
        Self::new("not_found", format!("{what} '{name}' does not exist"))
    }
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self::new("invalid_input", msg)
    }
    pub fn invalid_config(errors: Vec<String>) -> Self {
        let mut e = Self::new("invalid_config", "the configuration failed validation");
        e.errors = errors;
        e.with_hint(
            "Every success/outcome port must be wired. Use get_node_type(<type>) to see a node's ports and config keys.",
        )
    }
    pub fn debug_disabled() -> Self {
        Self::new("debug_disabled", "debug mode is off; traces and the sandbox are unavailable")
            .with_hint("set debug.enabled: true in system.yaml (or FEATHERBIT_DEBUG=true) and restart")
    }
    pub fn sandbox_disabled() -> Self {
        Self::new("sandbox_disabled", "the plugin sandbox is disabled")
            .with_hint("set debug.sandbox: true in system.yaml and restart")
    }
    pub fn forbidden(have: McpScope) -> Self {
        Self::new("forbidden", "this token may not use write tools").with_hint(format!(
            "this token has scope {}; write tools need a token with scope write. Return the YAML for a human to apply instead.",
            have.as_str()
        ))
    }
    pub fn store_error(msg: impl Into<String>) -> Self {
        Self::new("store_error", msg)
    }
    pub fn unknown_tool(name: &str) -> Self {
        Self::new("unknown_tool", format!("no tool named '{name}'"))
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new("internal", msg)
    }
    /// The body of the `isError` tool result.
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::json!({"code": self.code, "message": self.message});
        if !self.errors.is_empty() {
            v["errors"] = serde_json::json!(self.errors);
        }
        if let Some(h) = &self.hint {
            v["hint"] = Value::String(h.clone());
        }
        v
    }
}

/// Deserializes tool arguments, reporting schema mismatches as `invalid_input`.
pub fn args<T: DeserializeOwned>(a: JsonObject) -> Result<T, ToolError> {
    serde_json::from_value(Value::Object(a))
        .map_err(|e| ToolError::invalid_input(format!("invalid arguments: {e}")))
}

/// Accepts a payload either as a JSON object or as a YAML document in a
/// string — agents naturally write the YAML the docs show.
pub fn parse_payload<T: DeserializeOwned>(v: Value, what: &str) -> Result<T, ToolError> {
    match v {
        Value::String(yaml) => serde_yaml::from_str(&yaml)
            .map_err(|e| ToolError::invalid_input(format!("{what}: YAML did not parse: {e}"))),
        other => serde_json::from_value(other)
            .map_err(|e| ToolError::invalid_input(format!("{what}: JSON did not deserialize: {e}"))),
    }
}

/// JSON Schema (draft 2020-12, as schemars 1 emits) for a tool's arguments,
/// with the `$schema`/`title` noise removed.
pub fn schema_of<T: schemars::JsonSchema>() -> JsonObject {
    let schema = schemars::schema_for!(T);
    let mut v = serde_json::to_value(schema).expect("schema serializes");
    let obj = v.as_object_mut().expect("schema is an object");
    obj.remove("$schema");
    obj.remove("title");
    obj.clone()
}

/// Static description of one tool.
pub struct ToolDef {
    pub name: &'static str,
    pub scope: McpScope,
    pub description: &'static str,
    pub input_schema: fn() -> JsonObject,
}

/// Every tool, in the order clients see them.
pub fn tool_defs() -> &'static [ToolDef] {
    &TOOLS
}

/// Looks up a tool by name.
pub fn tool_def(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

/// Executes a tool. Scope is **not** checked here (see `server.rs`).
pub async fn call(state: &SharedState, name: &str, a: JsonObject) -> Result<Value, ToolError> {
    match name {
        "list_node_types" => catalog::list_node_types().await,
        "get_node_type" => catalog::get_node_type(args(a)?).await,
        "list_vars" => catalog::list_vars().await,
        "get_status" => catalog::get_status(state).await,
        "export_config" => catalog::export_config(state).await,
        "list_routes" => config::list_routes(state).await,
        "get_route" => config::get_route(state, args(a)?).await,
        "list_policies" => config::list_policies(state).await,
        "get_policy" => config::get_policy(state, args(a)?).await,
        "list_supernodes" => config::list_supernodes(state).await,
        "get_supernode" => config::get_supernode(state, args(a)?).await,
        "list_plugin_configs" => config::list_plugin_configs(state).await,
        "get_plugin_config" => config::get_plugin_config(state, args(a)?).await,
        "list_stores" => config::list_stores(state).await,
        "list_consumers" => config::list_consumers(state).await,
        "get_consumer" => config::get_consumer(state, args(a)?).await,
        "validate_policy" => config::validate_policy(state, args(a)?).await,
        "validate_supernode" => config::validate_supernode(args(a)?).await,
        _ => Err(ToolError::unknown_tool(name)),
    }
}

use McpScope::{Read, Write};

static TOOLS: [ToolDef; 18] = [
    ToolDef { name: "list_node_types", scope: Read, description: "List every node (plugin) type with its description and declared ports. Start here when designing a policy.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_node_type", scope: Read, description: "Full reference for one node type: description, input/output ports (which must be wired), and its documentation page with every config key and a YAML example.", input_schema: schema_of::<catalog::TypeArgs> },
    ToolDef { name: "list_vars", scope: Read, description: "The $var catalog usable in plugin config (e.g. $remote_addr, $http_<header>, $consumer_name) with examples.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_status", scope: Read, description: "Gateway version, route/policy counts, and whether debug mode and the sandbox are on.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "export_config", scope: Read, description: "The whole gateway.yaml as YAML text (routes, policies, supernodes, plugin_configs, stores, consumers). ${ENV} placeholders stay unresolved.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "list_routes", scope: Read, description: "All routes: name, match rule (path/methods/host/headers) and the policy each references.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_route", scope: Read, description: "One route by name.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "list_policies", scope: Read, description: "All policies (node graphs) with their nodes and edges.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_policy", scope: Read, description: "One policy by name, plus the routes that reference it.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "list_supernodes", scope: Read, description: "All supernode definitions (reusable subgraphs with input/output/error boundary nodes).", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_supernode", scope: Read, description: "One supernode definition by name, plus the policies that use it.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "list_plugin_configs", scope: Read, description: "Named shared plugin config profiles referenced by nodes via config_ref.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_plugin_config", scope: Read, description: "One plugin config profile by name.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "list_stores", scope: Read, description: "Named redis/valkey stores referenced by plugin config (`store:`) and sessions.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "list_consumers", scope: Read, description: "API consumers (name, group, labels, credential kinds). Credential secrets are masked.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_consumer", scope: Read, description: "One consumer by name; credential secrets are masked.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "validate_policy", scope: Read, description: "Validate and compile an unsaved policy (JSON object or YAML string) against the live gateway: structure, port wiring, config_ref/store references, and every node's config. Returns {valid, errors}. Persists nothing.", input_schema: schema_of::<config::ValidatePolicyArgs> },
    ToolDef { name: "validate_supernode", scope: Read, description: "Structurally validate an unsaved supernode definition (boundary nodes, reserved ids, inner wiring). Node config errors surface when a policy using it is validated or saved with dry_run.", input_schema: schema_of::<config::ValidateSupernodeArgs> },
];

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;

    /// A state with the given system/gateway YAML (both default to `{}`).
    pub fn state(system_yaml: &str, gateway_yaml: &str) -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str(system_yaml).unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(gateway_yaml).unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from("gateway.yaml"))),
            )
            .unwrap(),
        )
    }

    /// A minimal valid gateway: one route → one echo policy.
    pub const ECHO_GATEWAY: &str = r#"
routes:
  - name: hello
    match: { path: /hello }
    policy: echo-policy
policies:
  - name: echo-policy
    nodes:
      - { id: l, type: listener }
      - { id: e, type: echo, config: {} }
      - { id: c, type: client }
    edges:
      - { from: l.out, to: e.in }
      - { from: e.out, to: c.in }
"#;

    pub fn obj(v: Value) -> JsonObject {
        v.as_object().cloned().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique_and_scopes_follow_naming() {
        let mut seen = std::collections::HashSet::new();
        for t in tool_defs() {
            assert!(seen.insert(t.name), "duplicate tool {}", t.name);
            let mutating = t.name.starts_with("put_") || t.name.starts_with("delete_") || t.name == "reload_config";
            assert_eq!(t.scope == Write, mutating, "scope/name mismatch for {}", t.name);
            assert!(!t.description.is_empty());
            let schema = (t.input_schema)();
            assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"), "{}", t.name);
        }
    }

    #[tokio::test]
    async fn unknown_tool_is_reported() {
        let state = test_support::state("{}", "{}");
        let err = call(&state, "nope", JsonObject::new()).await.unwrap_err();
        assert_eq!(err.code, "unknown_tool");
    }

    #[test]
    fn payload_accepts_yaml_or_json() {
        let r: crate::config::RouteConfig =
            parse_payload(Value::String("name: r\nmatch: {path: /x}\npolicy: p\n".into()), "route").unwrap();
        assert_eq!(r.name, "r");
        let r: crate::config::RouteConfig =
            parse_payload(serde_json::json!({"name": "r2", "match": {"path": "/y"}, "policy": "p"}), "route").unwrap();
        assert_eq!(r.name, "r2");
        let err = parse_payload::<crate::config::RouteConfig>(Value::String("name: [".into()), "route").unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert!(err.message.contains("YAML"));
    }

    #[test]
    fn error_json_shape() {
        let e = ToolError::invalid_config(vec!["a".into(), "b".into()]);
        let v = e.to_json();
        assert_eq!(v["code"], "invalid_config");
        assert_eq!(v["errors"].as_array().unwrap().len(), 2);
        assert!(v["hint"].as_str().unwrap().contains("get_node_type"));
        assert!(ToolError::not_found("policy", "x").to_json().get("errors").is_none());
    }
}
```

`TOOLS` is an array of 18 for now; Tasks 5 and 6 append entries and bump the length (a wrong count is a compile error, which is the point).

- [ ] **Step 2: Catalog tools (write tests first, then code)**

Create `src/mcp/tools/catalog.rs`:

```rust
//! Read tools over static knowledge: node types, vars, status, config export.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::ToolError;
use crate::state::SharedState;

/// Tools that take no arguments.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

/// `{ "type": "<node type>" }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeArgs {
    /// Node type name as it appears in YAML `type:` (e.g. `limit-count`).
    #[serde(rename = "type")]
    pub node_type: String,
}

pub async fn list_node_types() -> Result<Value, ToolError> {
    Ok(serde_json::json!({ "node_types": crate::admin::policies::plugin_catalog() }))
}

pub async fn get_node_type(a: TypeArgs) -> Result<Value, ToolError> {
    let entry = crate::admin::policies::plugin_catalog()
        .into_iter()
        .find(|e| e["type"] == a.node_type)
        .ok_or_else(|| ToolError::not_found("node type", &a.node_type))?;
    let mut out = entry;
    out["docs"] = match crate::mcp::docs::plugin_page(&a.node_type) {
        Some(md) => Value::String(md),
        None => Value::Null,
    };
    Ok(out)
}

pub async fn list_vars() -> Result<Value, ToolError> {
    Ok(serde_json::json!({ "vars": crate::vars::catalog::var_catalog() }))
}

pub async fn get_status(state: &SharedState) -> Result<Value, ToolError> {
    let routes = state.routes.read().await.len();
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "routes": routes,
        "policies": gw.policies.len(),
        "supernodes": gw.supernodes.len(),
        "stores": gw.stores.len(),
        "debug": { "enabled": state.debug.enabled, "sandbox": state.debug.sandbox_enabled },
    }))
}

pub async fn export_config(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let yaml = serde_yaml::to_string(&*gw).map_err(|e| ToolError::internal(e.to_string()))?;
    Ok(serde_json::json!({ "format": "yaml", "content": yaml }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};
    use crate::mcp::tools::call;

    #[tokio::test]
    async fn node_types_and_lookup() {
        let s = state("{}", "{}");
        let v = call(&s, "list_node_types", obj(serde_json::json!({}))).await.unwrap();
        let types = v["node_types"].as_array().unwrap();
        assert!(types.iter().any(|t| t["type"] == "limit-count"));
        assert!(types[0]["ports"].is_object());

        let v = call(&s, "get_node_type", obj(serde_json::json!({"type": "condition"}))).await.unwrap();
        let outs = v["ports"]["outputs"].as_array().unwrap();
        assert!(outs.iter().any(|p| p["name"] == "true") && outs.iter().any(|p| p["name"] == "false"));

        let err = call(&s, "get_node_type", obj(serde_json::json!({"type": "nope"}))).await.unwrap_err();
        assert_eq!(err.code, "not_found");
        let err = call(&s, "get_node_type", obj(serde_json::json!({}))).await.unwrap_err();
        assert_eq!(err.code, "invalid_input");
    }

    #[tokio::test]
    async fn vars_status_export() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        let v = call(&s, "list_vars", obj(serde_json::json!({}))).await.unwrap();
        assert!(v["vars"].as_array().unwrap().iter().any(|e| e["name"] == "remote_addr"));
        let v = call(&s, "get_status", obj(serde_json::json!({}))).await.unwrap();
        assert_eq!(v["routes"], 1);
        assert_eq!(v["debug"]["enabled"], true);
        let v = call(&s, "export_config", obj(serde_json::json!({}))).await.unwrap();
        assert!(v["content"].as_str().unwrap().contains("echo-policy"));
    }
}
```

Until Task 7 exists, add a temporary stub so this compiles — create `src/mcp/docs.rs` containing only:

```rust
//! Documentation pages embedded in the binary (filled in by the docs task).

/// The cleaned Markdown page for a node type, if one exists.
pub fn plugin_page(_node_type: &str) -> Option<String> {
    None
}
```

and `pub mod docs;` in `src/mcp/mod.rs`. In `src/admin/policies.rs` change `fn plugin_catalog()` to `pub(crate) fn plugin_catalog()`; in `src/admin/mod.rs` change `mod policies;` to `pub(crate) mod policies;` (and `mod stores;` to `pub(crate) mod stores;` for Task 6).

- [ ] **Step 3: Config read tools**

Create `src/mcp/tools/config.rs`:

```rust
//! Read tools over the stored gateway config, plus standalone validation.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{parse_payload, ToolError};
use crate::config::{PolicyConfig, SupernodeConfig};
use crate::state::SharedState;

/// `{ "name": "<resource name>" }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NameArgs {
    pub name: String,
}

/// `{ "policy": <object | YAML string> }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidatePolicyArgs {
    /// The policy definition, as a JSON object or a YAML document string.
    pub policy: Value,
}

/// `{ "definition": <object | YAML string> }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidateSupernodeArgs {
    /// The supernode definition, as a JSON object or a YAML document string.
    pub definition: Value,
}

fn json<T: serde::Serialize>(v: &T) -> Result<Value, ToolError> {
    serde_json::to_value(v).map_err(|e| ToolError::internal(e.to_string()))
}

pub async fn list_routes(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "routes": json(&gw.routes)? }))
}

pub async fn get_route(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let r = gw.routes.iter().find(|r| r.name == a.name).ok_or_else(|| ToolError::not_found("route", &a.name))?;
    json(r)
}

pub async fn list_policies(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "policies": json(&gw.policies)? }))
}

pub async fn get_policy(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let p = gw.policies.iter().find(|p| p.name == a.name).ok_or_else(|| ToolError::not_found("policy", &a.name))?;
    let referenced_by: Vec<&str> = gw.routes.iter().filter(|r| r.policy == a.name).map(|r| r.name.as_str()).collect();
    Ok(serde_json::json!({ "policy": json(p)?, "referenced_by_routes": referenced_by }))
}

pub async fn list_supernodes(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "supernodes": json(&gw.supernodes)? }))
}

pub async fn get_supernode(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let s = gw.supernodes.iter().find(|s| s.name == a.name).ok_or_else(|| ToolError::not_found("supernode", &a.name))?;
    let used_by: Vec<&str> = gw
        .policies
        .iter()
        .filter(|p| p.nodes.iter().any(|n| n.node_type == "supernode" && n.config.get("name").and_then(Value::as_str) == Some(a.name.as_str())))
        .map(|p| p.name.as_str())
        .collect();
    Ok(serde_json::json!({ "supernode": json(s)?, "used_by_policies": used_by }))
}

pub async fn list_plugin_configs(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "plugin_configs": json(&gw.plugin_configs)? }))
}

pub async fn get_plugin_config(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let pc = gw.plugin_configs.iter().find(|p| p.name == a.name).ok_or_else(|| ToolError::not_found("plugin config", &a.name))?;
    json(pc)
}

pub async fn list_stores(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "stores": json(&gw.stores)? }))
}

pub async fn list_consumers(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let masked: Vec<_> = gw.consumers.iter().map(crate::consumers::mask_credentials).collect();
    Ok(serde_json::json!({ "consumers": json(&masked)?, "note": "credential secrets are masked" }))
}

pub async fn get_consumer(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let c = gw.consumers.iter().find(|c| c.name == a.name).ok_or_else(|| ToolError::not_found("consumer", &a.name))?;
    json(&crate::consumers::mask_credentials(c))
}

/// Validates + compiles a policy against the live supernodes, plugin configs
/// and stores, without persisting. Mirrors what `put_policy(dry_run)` checks
/// for the policy itself (cross-references from routes are not included).
pub async fn validate_policy(state: &SharedState, a: ValidatePolicyArgs) -> Result<Value, ToolError> {
    let policy: PolicyConfig = parse_payload(a.policy, "policy")?;
    let (supernodes, plugin_configs) = {
        let gw = state.gateway.read().await;
        (gw.supernodes.clone(), gw.plugin_configs.clone())
    };
    let errors: Vec<String> = match crate::graph::prepare_policy(policy, &supernodes, &plugin_configs)
        .and_then(|p| crate::graph::compile_policy(&p, state.resources.clone()).map(|_| ()))
    {
        Ok(()) => Vec::new(),
        Err(e) => e.split("; ").map(str::to_string).collect(),
    };
    Ok(serde_json::json!({ "valid": errors.is_empty(), "errors": errors }))
}

/// Structural validation of a supernode definition.
pub async fn validate_supernode(a: ValidateSupernodeArgs) -> Result<Value, ToolError> {
    let def: SupernodeConfig = parse_payload(a.definition, "definition")?;
    let errors = crate::graph::validate_supernode(&def).err().unwrap_or_default();
    Ok(serde_json::json!({ "valid": errors.is_empty(), "errors": errors }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    #[tokio::test]
    async fn reads_mirror_config_and_report_not_found() {
        let s = state("{}", ECHO_GATEWAY);
        let v = call(&s, "list_routes", obj(serde_json::json!({}))).await.unwrap();
        assert_eq!(v["routes"][0]["name"], "hello");
        assert_eq!(v["routes"][0]["match"]["path"], "/hello");
        let v = call(&s, "get_policy", obj(serde_json::json!({"name": "echo-policy"}))).await.unwrap();
        assert_eq!(v["referenced_by_routes"][0], "hello");
        assert_eq!(v["policy"]["nodes"].as_array().unwrap().len(), 3);
        let err = call(&s, "get_route", obj(serde_json::json!({"name": "nope"}))).await.unwrap_err();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("route 'nope'"));
    }

    #[tokio::test]
    async fn consumers_are_masked() {
        let gw = format!(
            "{ECHO_GATEWAY}\nconsumers:\n  - name: alice\n    credentials:\n      key-auth: {{ key: topsecret }}\n"
        );
        let s = state("{}", &gw);
        let v = call(&s, "list_consumers", obj(serde_json::json!({}))).await.unwrap();
        assert_eq!(v["consumers"][0]["credentials"]["key-auth"]["key"], "<masked>");
        let v = call(&s, "get_consumer", obj(serde_json::json!({"name": "alice"}))).await.unwrap();
        assert_eq!(v["credentials"]["key-auth"]["key"], "<masked>");
    }

    #[tokio::test]
    async fn validate_policy_reports_unwired_port_and_accepts_yaml() {
        let s = state("{}", ECHO_GATEWAY);
        let bad = "name: p\nnodes:\n  - {id: l, type: listener}\n  - {id: k, type: key-auth, config: {}}\n  - {id: c, type: client}\nedges:\n  - {from: l.out, to: k.in}\n  - {from: k.out, to: c.in}\n";
        let v = call(&s, "validate_policy", obj(serde_json::json!({"policy": bad}))).await.unwrap();
        assert_eq!(v["valid"], false);
        let errors = v["errors"].as_array().unwrap();
        assert!(errors.iter().any(|e| e.as_str().unwrap().contains("denied")), "{errors:?}");

        let good = serde_json::json!({"name": "p", "nodes": [
            {"id": "l", "type": "listener"}, {"id": "e", "type": "echo", "config": {}}, {"id": "c", "type": "client"}],
            "edges": [{"from": "l.out", "to": "e.in"}, {"from": "e.out", "to": "c.in"}]});
        let v = call(&s, "validate_policy", obj(serde_json::json!({"policy": good}))).await.unwrap();
        assert_eq!(v["valid"], true);
    }

    #[tokio::test]
    async fn validate_supernode_structural() {
        let s = state("{}", "{}");
        let def = "name: sn\nnodes:\n  - {id: input, type: input}\n  - {id: e, type: echo, config: {}}\nedges:\n  - {from: input.out, to: e.in}\n";
        let v = call(&s, "validate_supernode", obj(serde_json::json!({"definition": def}))).await.unwrap();
        assert_eq!(v["valid"], false, "{v}");
        assert!(!v["errors"].as_array().unwrap().is_empty());
    }
}
```

If `key-auth`'s outcome port is not named `denied`, read `src/plugins/ports.rs` `AUTH_SPEC` and use the actual name in the assertion. If the supernode boundary node types are not literally `input`/`output`/`error`, check `src/graph/validation.rs:145-170` and adjust the fixture (the test only needs an *invalid* definition, e.g. missing any `output` boundary).

- [ ] **Step 4: Run**

Run: `cargo test --lib mcp::tools`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings && cargo check --no-default-features --locked
git add src/mcp src/admin/policies.rs src/admin/mod.rs
git commit -m "feat(mcp): tool registry, error shape, and read tools over the catalog and stored config"
```

---

### Task 5: Debug tools (traces + sandbox)

**Files:**
- Create: `src/mcp/tools/debug.rs`
- Modify: `src/mcp/tools/mod.rs` (`pub mod debug;`, four `call` arms, four `TOOLS` entries → `[ToolDef; 22]`)

**Interfaces:**
- Consumes: `crate::debug::render::{TraceFilter, apply_filter, render_trace}`, `crate::debug::sandbox::{run_sandbox, SandboxError, SandboxRequest}`, `state.debug.{enabled, list(), get(id)}`.
- Produces: `list_traces(state, ListTracesArgs)`, `get_trace(state, GetTraceArgs { id, include_snapshots: bool })`, `get_trace_step(state, GetTraceStepArgs { id, node_id: Option<String>, index: Option<usize> })`, `run_sandbox_tool(state, Value)`. `get_trace` without snapshots strips `initial` and each step's `after` (keeps `changes`, `outcome`, `port`, `edge`, `duration_us`, …). `get_trace_step` returns `{step: <NodeStep with changes>, before: <ContextSnapshot>, after: <ContextSnapshot>, node_config: <NodeConfig|null>}`.

- [ ] **Step 1: Tests + implementation**

Create `src/mcp/tools/debug.rs`:

```rust
//! Read tools over debug mode: trace listing/inspection and the sandbox.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::ToolError;
use crate::debug::render::{apply_filter, render_trace, TraceFilter};
use crate::debug::sandbox::{run_sandbox, SandboxError, SandboxRequest};
use crate::state::SharedState;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListTracesArgs {
    /// Only traces recorded for this route name.
    pub route: Option<String>,
    /// Only traces of this policy.
    pub policy: Option<String>,
    /// Only traces whose final response had this status.
    pub status: Option<u16>,
    /// `request` (real traffic) or `sandbox`.
    pub source: Option<String>,
    /// Maximum rows (newest first). Default 20.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTraceArgs {
    /// Trace id from list_traces.
    pub id: String,
    /// Include the full context snapshot after every step (large). Default false;
    /// use get_trace_step for one node's before/after.
    #[serde(default)]
    pub include_snapshots: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTraceStepArgs {
    /// Trace id from list_traces.
    pub id: String,
    /// Node id of the step (as shown in the trace). Either this or `index`.
    pub node_id: Option<String>,
    /// Zero-based step index. Either this or `node_id`.
    pub index: Option<usize>,
}

fn require_debug(state: &SharedState) -> Result<(), ToolError> {
    if state.debug.enabled { Ok(()) } else { Err(ToolError::debug_disabled()) }
}

pub async fn list_traces(state: &SharedState, a: ListTracesArgs) -> Result<Value, ToolError> {
    require_debug(state)?;
    let f = TraceFilter {
        route: a.route,
        policy: a.policy,
        status: a.status,
        source: a.source,
        limit: Some(a.limit.unwrap_or(20)),
    };
    let traces = apply_filter(state.debug.list(), &f);
    Ok(serde_json::json!({ "traces": traces }))
}

pub async fn get_trace(state: &SharedState, a: GetTraceArgs) -> Result<Value, ToolError> {
    require_debug(state)?;
    let trace = state.debug.get(&a.id).ok_or_else(|| ToolError::not_found("trace", &a.id))?;
    let mut v = render_trace(&trace);
    if !a.include_snapshots {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("initial");
        }
        if let Some(steps) = v["steps"].as_array_mut() {
            for s in steps {
                if let Some(o) = s.as_object_mut() {
                    o.remove("after");
                }
            }
        }
        v["snapshots_omitted"] = Value::Bool(true);
    }
    Ok(v)
}

pub async fn get_trace_step(state: &SharedState, a: GetTraceStepArgs) -> Result<Value, ToolError> {
    require_debug(state)?;
    let trace = state.debug.get(&a.id).ok_or_else(|| ToolError::not_found("trace", &a.id))?;
    let idx = match (&a.node_id, a.index) {
        (Some(id), _) => trace
            .steps
            .iter()
            .position(|s| &s.node_id == id)
            .ok_or_else(|| ToolError::not_found("step for node", id))?,
        (None, Some(i)) if i < trace.steps.len() => i,
        (None, Some(i)) => return Err(ToolError::not_found("step index", &i.to_string())),
        (None, None) => return Err(ToolError::invalid_input("provide node_id or index")),
    };
    let rendered = render_trace(&trace);
    let step = rendered["steps"][idx].clone();
    let before = if idx == 0 { &trace.initial } else { &trace.steps[idx - 1].after };
    let node_config = {
        let gw = state.gateway.read().await;
        gw.policies
            .iter()
            .find(|p| p.name == trace.policy)
            .and_then(|p| p.nodes.iter().find(|n| n.id == trace.steps[idx].node_id))
            .map(|n| serde_json::to_value(n).unwrap_or(Value::Null))
            .unwrap_or(Value::Null)
    };
    Ok(serde_json::json!({
        "trace_id": trace.id,
        "policy": trace.policy,
        "step": step,
        "before": serde_json::to_value(before).map_err(|e| ToolError::internal(e.to_string()))?,
        "after": serde_json::to_value(&trace.steps[idx].after).map_err(|e| ToolError::internal(e.to_string()))?,
        "node_config": node_config,
    }))
}

/// `run_sandbox` takes the same body as `POST /api/debug/sandbox`.
pub async fn run_sandbox_tool(state: &SharedState, a: Value) -> Result<Value, ToolError> {
    let req: SandboxRequest = serde_json::from_value(a)
        .map_err(|e| ToolError::invalid_input(format!("invalid sandbox request: {e}")))?;
    match run_sandbox(state, req).await {
        Ok(r) => Ok(serde_json::json!({
            "mode": r.mode,
            "policy": r.policy,
            "warning": "plugins executed for real: outbound calls were made and shared rate-limit/breaker state was mutated",
            "stored_trace_id": r.stored_trace_id,
            "trace": r.trace,
        })),
        Err(SandboxError::Disabled) => Err(ToolError::debug_disabled()),
        Err(SandboxError::SandboxDisabled) => Err(ToolError::sandbox_disabled()),
        Err(SandboxError::BadRequest(m)) => Err(ToolError::invalid_input(m)),
        Err(SandboxError::UnknownPolicy(n)) => Err(ToolError::not_found("policy", &n)),
        Err(SandboxError::Timeout(s)) => Err(ToolError::internal(format!("run exceeded debug.sandbox_timeout_seconds ({s}s)"))),
    }
}

/// Schema for `run_sandbox`: a permissive object (the body is documented by
/// the sandbox docs; nodes/policy are mutually exclusive).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SandboxArgs {
    /// Ad-hoc node list to run in order (exclusive with `policy`).
    pub nodes: Option<Vec<Value>>,
    /// Name of a stored policy to run (exclusive with `nodes`).
    pub policy: Option<String>,
    /// `stop` (default) or `client`: what an error port does in nodes mode.
    pub on_error: Option<String>,
    /// Synthetic request: {method, path, host, headers, query_params, body, message, response}.
    pub context: Option<Value>,
}

#[cfg(test)]
mod tests {
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    #[tokio::test]
    async fn debug_off_is_a_tool_error() {
        let s = state("{}", ECHO_GATEWAY);
        for (tool, a) in [
            ("list_traces", serde_json::json!({})),
            ("get_trace", serde_json::json!({"id": "x"})),
            ("get_trace_step", serde_json::json!({"id": "x", "index": 0})),
            ("run_sandbox", serde_json::json!({"policy": "echo-policy", "context": {}})),
        ] {
            let err = call(&s, tool, obj(a)).await.unwrap_err();
            assert_eq!(err.code, "debug_disabled", "{tool}");
            assert!(err.hint.as_deref().unwrap().contains("debug.enabled"));
        }
    }

    #[tokio::test]
    async fn sandbox_then_inspect_trace() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        let run = call(&s, "run_sandbox", obj(serde_json::json!({"policy": "echo-policy", "context": {"path": "/hello"}})))
            .await
            .unwrap();
        let id = run["stored_trace_id"].as_str().unwrap().to_string();

        let list = call(&s, "list_traces", obj(serde_json::json!({"source": "sandbox"}))).await.unwrap();
        assert_eq!(list["traces"][0]["id"], id);
        let list = call(&s, "list_traces", obj(serde_json::json!({"policy": "other"}))).await.unwrap();
        assert!(list["traces"].as_array().unwrap().is_empty());

        let t = call(&s, "get_trace", obj(serde_json::json!({"id": id}))).await.unwrap();
        assert_eq!(t["snapshots_omitted"], true);
        assert!(t.get("initial").is_none());
        assert!(t["steps"][0].get("after").is_none());
        assert!(t["steps"][0]["changes"].is_array());
        let t = call(&s, "get_trace", obj(serde_json::json!({"id": id, "include_snapshots": true}))).await.unwrap();
        assert!(t["initial"].is_object() && t["steps"][0]["after"].is_object());

        let st = call(&s, "get_trace_step", obj(serde_json::json!({"id": id, "node_id": "e"}))).await.unwrap();
        assert_eq!(st["step"]["node_id"], "e");
        assert_eq!(st["node_config"]["type"], "echo");
        assert!(st["before"].is_object() && st["after"].is_object());
        let err = call(&s, "get_trace_step", obj(serde_json::json!({"id": id, "node_id": "zz"}))).await.unwrap_err();
        assert_eq!(err.code, "not_found");
        let err = call(&s, "get_trace_step", obj(serde_json::json!({"id": id}))).await.unwrap_err();
        assert_eq!(err.code, "invalid_input");

        let err = call(&s, "run_sandbox", obj(serde_json::json!({"policy": "nope", "context": {}}))).await.unwrap_err();
        assert_eq!(err.code, "not_found");
        let err = call(&s, "run_sandbox", obj(serde_json::json!({"context": {}}))).await.unwrap_err();
        assert_eq!(err.code, "invalid_input");
    }
}
```

In `src/mcp/tools/mod.rs`: add `pub mod debug;`, these `call` arms —

```rust
        "list_traces" => debug::list_traces(state, args(a)?).await,
        "get_trace" => debug::get_trace(state, args(a)?).await,
        "get_trace_step" => debug::get_trace_step(state, args(a)?).await,
        "run_sandbox" => debug::run_sandbox_tool(state, Value::Object(a)).await,
```

— and these `TOOLS` entries (bump to `[ToolDef; 22]`):

```rust
    ToolDef { name: "list_traces", scope: Read, description: "Recent debug traces (newest first): id, route, policy, method, path, status, step and error counts. Filter by route/policy/status/source. Requires debug.enabled.", input_schema: schema_of::<debug::ListTracesArgs> },
    ToolDef { name: "get_trace", scope: Read, description: "One trace: the request, final response, and every node step with outcome, exit port, edge taken and the context changes it made. Snapshots omitted unless include_snapshots.", input_schema: schema_of::<debug::GetTraceArgs> },
    ToolDef { name: "get_trace_step", scope: Read, description: "One step of a trace in full: context before and after the node, the diff, outcome/port, and the node's stored config. Use to answer 'why did this node exit on this port?'.", input_schema: schema_of::<debug::GetTraceStepArgs> },
    ToolDef { name: "run_sandbox", scope: Read, description: "Run a stored policy or an ad-hoc node list against a synthetic request, for real (outbound calls happen), and get the resulting trace. Requires debug.enabled and debug.sandbox.", input_schema: schema_of::<debug::SandboxArgs> },
```

- [ ] **Step 2: Run and commit**

Run: `cargo test --lib mcp::tools`
Expected: all pass (the registry test now also covers the four new entries).

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
git add src/mcp/tools
git commit -m "feat(mcp): trace and sandbox tools"
```

---

### Task 6: Write tools with `dry_run`

**Files:**
- Create: `src/mcp/tools/writes.rs`
- Modify: `src/mcp/tools/mod.rs` (`pub mod writes;`, eleven `call` arms, eleven `TOOLS` entries → `[ToolDef; 33]`)

**Interfaces:**
- Consumes: `state.validate_gateway(&GatewayConfig) -> Result<(), String>`, `state.config_store.clone().commit(&state, GatewayConfig) -> Result<(), String>`, `state.reload_from_disk()`, `crate::admin::stores::store_referrers(&GatewayConfig, &str) -> Vec<String>`.
- Produces: `pub async fn commit_candidate(state, dry_run: bool, changed: Vec<String>, mutate: impl FnOnce(&mut GatewayConfig) -> Result<(), ToolError>) -> Result<Value, ToolError>` returning `{"applied": bool, "dry_run": bool, "changed": [..]}`; tools `put_route`, `delete_route`, `put_policy`, `delete_policy`, `put_supernode`, `delete_supernode`, `put_plugin_config`, `delete_plugin_config`, `put_store`, `delete_store`, `reload_config`. Upserts set `name` from the argument (as the REST `PUT` handlers do). `delete_store` refuses with `invalid_config` + `errors: referrers` when referenced (REST's `409 in_use`).

- [ ] **Step 1: Implementation with tests**

Create `src/mcp/tools/writes.rs`:

```rust
//! Write tools. Every mutation builds a full candidate `GatewayConfig`, then
//! either validates it (`dry_run`) or commits it through the configured
//! `ConfigStore` — exactly the path the Admin API takes.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{parse_payload, ToolError};
use crate::config::{GatewayConfig, PluginConfigDef, PolicyConfig, RouteConfig, StoreConfig, SupernodeConfig};
use crate::state::SharedState;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PutArgs {
    /// Resource name; overrides any `name` inside the payload.
    pub name: String,
    /// The definition, as a JSON object or a YAML document string.
    pub definition: Value,
    /// Validate the whole resulting config without applying it. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteArgs {
    /// Resource name.
    pub name: String,
    /// Validate the whole resulting config without applying it. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

/// Applies `mutate` to a clone of the live config, then validates or commits.
pub async fn commit_candidate(
    state: &SharedState,
    dry_run: bool,
    changed: Vec<String>,
    mutate: impl FnOnce(&mut GatewayConfig) -> Result<(), ToolError>,
) -> Result<Value, ToolError> {
    let mut candidate = state.gateway.read().await.clone();
    mutate(&mut candidate)?;
    // Validate first in both modes so a store failure after a clean
    // validation is reported as store_error, not invalid_config.
    state
        .validate_gateway(&candidate)
        .map_err(|e| ToolError::invalid_config(vec![e]))?;
    if !dry_run {
        state
            .config_store
            .clone()
            .commit(state, candidate)
            .await
            .map_err(ToolError::store_error)?;
    }
    Ok(serde_json::json!({ "applied": !dry_run, "dry_run": dry_run, "changed": changed }))
}

fn upsert<T>(items: &mut Vec<T>, item: T, same: impl Fn(&T) -> bool) {
    if let Some(existing) = items.iter_mut().find(|i| same(i)) {
        *existing = item;
    } else {
        items.push(item);
    }
}

fn remove<T>(items: &mut Vec<T>, what: &str, name: &str, same: impl Fn(&T) -> bool) -> Result<(), ToolError> {
    let before = items.len();
    items.retain(|i| !same(i));
    if items.len() == before { Err(ToolError::not_found(what, name)) } else { Ok(()) }
}

pub async fn put_route(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let mut route: RouteConfig = parse_payload(a.definition, "route")?;
    route.name = a.name.clone();
    commit_candidate(state, a.dry_run, vec![format!("route:{}", a.name)], |gw| {
        upsert(&mut gw.routes, route, |r| r.name == a.name);
        Ok(())
    })
    .await
}

pub async fn delete_route(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec![format!("route:{}", a.name)], |gw| {
        remove(&mut gw.routes, "route", &a.name, |r| r.name == a.name)
    })
    .await
}

pub async fn put_policy(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let mut policy: PolicyConfig = parse_payload(a.definition, "policy")?;
    policy.name = a.name.clone();
    commit_candidate(state, a.dry_run, vec![format!("policy:{}", a.name)], |gw| {
        upsert(&mut gw.policies, policy, |p| p.name == a.name);
        Ok(())
    })
    .await
}

pub async fn delete_policy(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec![format!("policy:{}", a.name)], |gw| {
        remove(&mut gw.policies, "policy", &a.name, |p| p.name == a.name)
    })
    .await
}

pub async fn put_supernode(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let mut sn: SupernodeConfig = parse_payload(a.definition, "supernode")?;
    sn.name = a.name.clone();
    commit_candidate(state, a.dry_run, vec![format!("supernode:{}", a.name)], |gw| {
        upsert(&mut gw.supernodes, sn, |s| s.name == a.name);
        Ok(())
    })
    .await
}

pub async fn delete_supernode(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec![format!("supernode:{}", a.name)], |gw| {
        remove(&mut gw.supernodes, "supernode", &a.name, |s| s.name == a.name)
    })
    .await
}

pub async fn put_plugin_config(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let mut pc: PluginConfigDef = parse_payload(a.definition, "plugin config")?;
    pc.name = a.name.clone();
    commit_candidate(state, a.dry_run, vec![format!("plugin_config:{}", a.name)], |gw| {
        upsert(&mut gw.plugin_configs, pc, |p| p.name == a.name);
        Ok(())
    })
    .await
}

pub async fn delete_plugin_config(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec![format!("plugin_config:{}", a.name)], |gw| {
        remove(&mut gw.plugin_configs, "plugin config", &a.name, |p| p.name == a.name)
    })
    .await
}

pub async fn put_store(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let mut store: StoreConfig = parse_payload(a.definition, "store")?;
    store.name = a.name.clone();
    commit_candidate(state, a.dry_run, vec![format!("store:{}", a.name)], |gw| {
        upsert(&mut gw.stores, store, |s| s.name == a.name);
        Ok(())
    })
    .await
}

pub async fn delete_store(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec![format!("store:{}", a.name)], |gw| {
        let referrers = crate::admin::stores::store_referrers(gw, &a.name);
        if !referrers.is_empty() {
            let mut e = ToolError::invalid_config(referrers);
            e.message = format!("store '{}' is still referenced", a.name);
            e.hint = Some("remove or repoint the referrers first".into());
            return Err(e);
        }
        remove(&mut gw.stores, "store", &a.name, |s| s.name == a.name)
    })
    .await
}

pub async fn reload_config(state: &SharedState) -> Result<Value, ToolError> {
    state.reload_from_disk().await.map_err(ToolError::store_error)?;
    Ok(serde_json::json!({ "status": "reloaded" }))
}

#[cfg(test)]
mod tests {
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    const NEW_POLICY: &str = "nodes:\n  - {id: l, type: listener}\n  - {id: e, type: echo, config: {}}\n  - {id: c, type: client}\nedges:\n  - {from: l.out, to: e.in}\n  - {from: e.out, to: c.in}\n";

    #[tokio::test]
    async fn dry_run_validates_without_applying() {
        let s = state("{}", ECHO_GATEWAY);
        let v = call(&s, "put_policy", obj(serde_json::json!({"name": "p2", "definition": NEW_POLICY, "dry_run": true}))).await.unwrap();
        assert_eq!(v["applied"], false);
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["changed"][0], "policy:p2");
        assert!(s.gateway.read().await.policies.iter().all(|p| p.name != "p2"));
    }

    #[tokio::test]
    async fn put_then_route_then_delete_are_applied_and_hot_compiled() {
        let s = state("{}", ECHO_GATEWAY);
        let v = call(&s, "put_policy", obj(serde_json::json!({"name": "p2", "definition": NEW_POLICY}))).await.unwrap();
        assert_eq!(v["applied"], true);
        assert!(s.gateway.read().await.policies.iter().any(|p| p.name == "p2"));

        call(&s, "put_route", obj(serde_json::json!({"name": "r2", "definition": {"match": {"path": "/two"}, "policy": "p2"}}))).await.unwrap();
        assert_eq!(s.routes.read().await.len(), 2);

        // Deleting a policy still referenced by a route is rejected whole.
        let err = call(&s, "delete_policy", obj(serde_json::json!({"name": "p2"}))).await.unwrap_err();
        assert_eq!(err.code, "invalid_config");
        assert!(err.errors[0].contains("p2"), "{:?}", err.errors);

        call(&s, "delete_route", obj(serde_json::json!({"name": "r2"}))).await.unwrap();
        call(&s, "delete_policy", obj(serde_json::json!({"name": "p2"}))).await.unwrap();
        assert_eq!(s.routes.read().await.len(), 1);
        let err = call(&s, "delete_policy", obj(serde_json::json!({"name": "p2"}))).await.unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[tokio::test]
    async fn invalid_policy_is_rejected_with_engine_errors() {
        let s = state("{}", ECHO_GATEWAY);
        let bad = "nodes:\n  - {id: l, type: listener}\n  - {id: k, type: key-auth, config: {}}\n  - {id: c, type: client}\nedges:\n  - {from: l.out, to: k.in}\n  - {from: k.out, to: c.in}\n";
        let err = call(&s, "put_policy", obj(serde_json::json!({"name": "bad", "definition": bad}))).await.unwrap_err();
        assert_eq!(err.code, "invalid_config");
        assert!(err.hint.as_deref().unwrap().contains("wired"));
        assert!(s.gateway.read().await.policies.iter().all(|p| p.name != "bad"));
        let err = call(&s, "put_policy", obj(serde_json::json!({"name": "bad", "definition": "nodes: ["}))).await.unwrap_err();
        assert_eq!(err.code, "invalid_input");
    }

    #[tokio::test]
    async fn supernode_plugin_config_store_round_trip() {
        let s = state("{}", ECHO_GATEWAY);
        call(&s, "put_plugin_config", obj(serde_json::json!({"name": "shared-echo", "definition": {"type": "echo", "config": {}}}))).await.unwrap();
        assert_eq!(s.gateway.read().await.plugin_configs.len(), 1);
        call(&s, "delete_plugin_config", obj(serde_json::json!({"name": "shared-echo"}))).await.unwrap();

        // Stores: only exercised when the redis-store feature is compiled in
        // (without it, declaring a store fails validation by design).
        if cfg!(feature = "redis-store") {
            call(&s, "put_store", obj(serde_json::json!({"name": "st", "definition": {"type": "redis", "url": "redis://127.0.0.1:1"}}))).await.unwrap();
            call(&s, "put_plugin_config", obj(serde_json::json!({"name": "lc", "definition": {"type": "limit-count", "config": {"count": 1, "time_window": 1, "policy": "redis", "store": "st"}}}))).await.unwrap();
            let err = call(&s, "delete_store", obj(serde_json::json!({"name": "st"}))).await.unwrap_err();
            assert_eq!(err.code, "invalid_config");
            assert!(err.errors[0].contains("plugin_config 'lc'"), "{:?}", err.errors);
            call(&s, "delete_plugin_config", obj(serde_json::json!({"name": "lc"}))).await.unwrap();
            call(&s, "delete_store", obj(serde_json::json!({"name": "st"}))).await.unwrap();
        }
    }
}
```

If building a redis store client at apply time requires a live connection (check `src/stores/` — `StoreRegistry::rebuild`), and the test fails on connection rather than validation, replace the store round-trip with a `dry_run: true` `put_store` assertion only, and note it in the test.

In `src/mcp/tools/mod.rs`: `pub mod writes;`, `call` arms —

```rust
        "put_route" => writes::put_route(state, args(a)?).await,
        "delete_route" => writes::delete_route(state, args(a)?).await,
        "put_policy" => writes::put_policy(state, args(a)?).await,
        "delete_policy" => writes::delete_policy(state, args(a)?).await,
        "put_supernode" => writes::put_supernode(state, args(a)?).await,
        "delete_supernode" => writes::delete_supernode(state, args(a)?).await,
        "put_plugin_config" => writes::put_plugin_config(state, args(a)?).await,
        "delete_plugin_config" => writes::delete_plugin_config(state, args(a)?).await,
        "put_store" => writes::put_store(state, args(a)?).await,
        "delete_store" => writes::delete_store(state, args(a)?).await,
        "reload_config" => writes::reload_config(state).await,
```

— and `TOOLS` entries (→ `[ToolDef; 33]`), each with `scope: Write`:

```rust
    ToolDef { name: "put_route", scope: Write, description: "Create or replace a route {match: {path, methods?, host?, headers?}, policy}. Set dry_run=true first to validate the whole resulting config without applying.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_route", scope: Write, description: "Delete a route by name (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "put_policy", scope: Write, description: "Create or replace a policy {nodes, edges, error_handler?}. Every success/outcome port must be wired. Use dry_run=true first.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_policy", scope: Write, description: "Delete a policy by name; fails while a route still references it (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "put_supernode", scope: Write, description: "Create or replace a supernode definition {nodes, edges, description?} with input/output/error boundary nodes. Use dry_run=true first.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_supernode", scope: Write, description: "Delete a supernode by name; fails while a policy still uses it (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "put_plugin_config", scope: Write, description: "Create or replace a shared plugin config profile {type, config, description?} referenced by nodes via config_ref.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_plugin_config", scope: Write, description: "Delete a plugin config profile by name; fails while referenced (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "put_store", scope: Write, description: "Create or replace a redis/valkey store {type, url, password?, key_prefix?, tls?}. Keep secrets as ${ENV_VAR} placeholders.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_store", scope: Write, description: "Delete a store by name; fails with the list of referrers while in use (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "reload_config", scope: Write, description: "Re-read gateway.yaml from disk and apply it (file config source only).", input_schema: schema_of::<catalog::NoArgs> },
];
```

- [ ] **Step 2: Run and commit**

Run: `cargo test --lib mcp::tools && cargo test --lib --no-default-features --features ui mcp::tools`
Expected: green in both (the store round-trip self-skips without `redis-store`).

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
git add src/mcp/tools
git commit -m "feat(mcp): write tools with dry_run through the shared commit path"
```

---

### Task 7: Embedded documentation (`src/mcp/docs.rs`)

**Files:**
- Modify: `src/mcp/docs.rs` (replace the stub)

**Interfaces:**
- Produces: `pub enum DocSection { Plugins, Concepts, Reference }`; `pub struct DocPage { pub uri: String, pub title: String, pub description: String }`; `pub fn list_pages() -> Vec<DocPage>`; `pub fn read_uri(uri: &str) -> Option<String>` (cleaned Markdown for `featherbit://docs/{plugins|concepts|reference}/{name}`); `pub fn plugin_page(node_type: &str) -> Option<String>`; `pub fn concept_page(name: &str) -> Option<String>`; `pub fn reference_page(name: &str) -> Option<String>`; `pub fn clean(md: &str, section: DocSection) -> String`; `pub const DOCS_URI_PREFIX: &str = "featherbit://docs/"`.
- Mapping: `listener` and `client` → `listener-client.md`; every other type → `<type>.md`; `index.md` is excluded from listings.

- [ ] **Step 1: Tests**

Replace `src/mcp/docs.rs` with tests first (implementation in Step 3):

```rust
//! Documentation pages embedded in the binary and served to agents as MCP
//! resources (`featherbit://docs/...`). The docs site's Markdown is the
//! single source of truth for plugin config keys, so agents read the same
//! pages humans do — minus Docusaurus frontmatter and JSX.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_type_has_a_page() {
        for entry in crate::admin::policies::plugin_catalog() {
            let t = entry["type"].as_str().unwrap();
            assert!(plugin_page(t).is_some(), "no docs page for node type '{t}'");
        }
    }

    #[test]
    fn plugin_page_is_cleaned() {
        let md = plugin_page("limit-count").unwrap();
        assert!(md.starts_with("# limit-count\n"), "{}", &md[..80.min(md.len())]);
        assert!(!md.contains("---\ntitle:"));
        assert!(!md.contains("plugin-chip"));
        assert!(md.contains("| `count` |"));
        assert!(md.contains("featherbit://docs/plugins/rate-limit"), "links rewritten");
    }

    #[test]
    fn concept_page_drops_imports_and_jsx_blocks() {
        let md = concept_page("supernodes").unwrap();
        assert!(!md.contains("import UiShot"));
        assert!(!md.contains("<UiShot"));
        assert!(!md.contains("caption="));
        assert!(md.contains("featherbit://docs/concepts/policies-and-graphs"));
    }

    #[test]
    fn uri_mapping_and_listing() {
        assert!(read_uri("featherbit://docs/plugins/key-auth").is_some());
        assert!(read_uri("featherbit://docs/plugins/listener").is_some());
        assert!(read_uri("featherbit://docs/plugins/client").is_some());
        assert!(read_uri("featherbit://docs/reference/context-vars").is_some());
        assert!(read_uri("featherbit://docs/plugins/index").is_none());
        assert!(read_uri("featherbit://docs/nope/x").is_none());
        assert!(read_uri("featherbit://policies/x").is_none());
        let pages = list_pages();
        assert!(pages.iter().any(|p| p.uri == "featherbit://docs/plugins/limit-count" && p.title == "limit-count"));
        assert!(pages.iter().all(|p| !p.uri.ends_with("/index")));
        assert!(pages.iter().any(|p| p.uri == "featherbit://docs/concepts/supernodes" && !p.description.is_empty()));
    }

    #[test]
    fn clean_handles_edge_cases() {
        let raw = "---\ntitle: T\ndescription: D\n---\n\nimport X from 'y';\n\n<span className=\"plugin-chip\">t</span>\n\nBody [link](./other.md) and [c](../../concepts/stores.md#a).\n\n<UiShot\n  name=\"x\"\n/>\n\nEnd\n";
        let out = clean(raw, DocSection::Plugins);
        assert_eq!(out, "# T\n\nBody [link](featherbit://docs/plugins/other) and [c](featherbit://docs/concepts/stores).\n\nEnd\n");
    }
}
```

- [ ] **Step 2: Run to see failure**

Run: `cargo test --lib mcp::docs`
Expected: compile errors.

- [ ] **Step 3: Implement**

Above the tests in `src/mcp/docs.rs`:

```rust
use regex::Regex;
use rust_embed::Embed;
use std::sync::OnceLock;

/// `website/docs/` subset compiled into the binary (~300 KB).
#[derive(Embed)]
#[folder = "website/docs/"]
#[include = "reference/plugins/*.md"]
#[include = "concepts/*.md"]
#[include = "reference/context-vars.md"]
#[include = "reference/conditions.md"]
#[include = "reference/templates.md"]
struct DocsAssets;

/// URI prefix of every documentation resource.
pub const DOCS_URI_PREFIX: &str = "featherbit://docs/";

/// Which docs directory a page lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocSection {
    Plugins,
    Concepts,
    Reference,
}

impl DocSection {
    fn slug(self) -> &'static str {
        match self {
            DocSection::Plugins => "plugins",
            DocSection::Concepts => "concepts",
            DocSection::Reference => "reference",
        }
    }
    fn dir(self) -> &'static str {
        match self {
            DocSection::Plugins => "reference/plugins/",
            DocSection::Concepts => "concepts/",
            DocSection::Reference => "reference/",
        }
    }
    fn parse(slug: &str) -> Option<Self> {
        match slug {
            "plugins" => Some(DocSection::Plugins),
            "concepts" => Some(DocSection::Concepts),
            "reference" => Some(DocSection::Reference),
            _ => None,
        }
    }
}

/// A listable page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocPage {
    pub uri: String,
    pub title: String,
    pub description: String,
}

/// The file behind a node type's page (`listener`/`client` share one).
fn plugin_file(node_type: &str) -> String {
    match node_type {
        "listener" | "client" => "listener-client".to_string(),
        other => other.to_string(),
    }
}

fn raw(section: DocSection, name: &str) -> Option<String> {
    if name.is_empty() || name == "index" || name.contains('/') || name.contains("..") {
        return None;
    }
    let path = format!("{}{}.md", section.dir(), name);
    DocsAssets::get(&path).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}

/// Frontmatter `title`/`description` (both may be empty).
fn frontmatter(md: &str) -> (String, String) {
    let mut title = String::new();
    let mut description = String::new();
    if let Some(rest) = md.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some(v) = line.strip_prefix("title:") {
                    title = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = v.trim().trim_matches('"').to_string();
                }
            }
        }
    }
    (title, description)
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\]\(((?:\.\./|\./)*)([A-Za-z0-9_./-]*?)([a-z0-9-]+)\.md(#[^)]*)?\)").unwrap())
}

/// Strips frontmatter (keeping the title as an H1), `import` lines, and JSX
/// elements (single-line `<span …>…</span>` and multi-line `<Component … />`
/// blocks), and rewrites relative `.md` links to `featherbit://docs/…` URIs.
pub fn clean(md: &str, section: DocSection) -> String {
    let (title, _) = frontmatter(md);
    let body = if md.starts_with("---\n") {
        match md[4..].find("\n---") {
            Some(end) => &md[4 + end + 4..],
            None => md,
        }
    } else {
        md
    };

    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(&format!("# {title}\n"));
    }
    let mut in_jsx = false;
    for line in body.lines() {
        let t = line.trim_start();
        if in_jsx {
            if t.ends_with("/>") || t.starts_with("</") {
                in_jsx = false;
            }
            continue;
        }
        if t.starts_with("import ") && t.ends_with(';') {
            continue;
        }
        if t.starts_with('<') && t.chars().nth(1).is_some_and(|c| c.is_ascii_uppercase()) {
            // <UiShot … /> possibly spanning lines.
            if !(t.ends_with("/>") || t.contains("</")) {
                in_jsx = true;
            }
            continue;
        }
        if t.starts_with("<span className=") && t.ends_with("</span>") {
            continue;
        }
        let rewritten = link_re().replace_all(line, |c: &regex::Captures| {
            let dirs = &c[2];
            let name = &c[3];
            let target = if dirs.contains("concepts/") {
                DocSection::Concepts
            } else if dirs.contains("plugins/") {
                DocSection::Plugins
            } else if dirs.contains("reference/") {
                DocSection::Reference
            } else {
                section
            };
            format!("]({}{}/{})", DOCS_URI_PREFIX, target.slug(), name)
        });
        out.push_str(&rewritten);
        out.push('\n');
    }
    // Collapse runs of blank lines left by removed elements.
    let mut collapsed = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
    }
    collapsed
}

fn page(section: DocSection, name: &str) -> Option<String> {
    raw(section, name).map(|md| clean(&md, section))
}

/// The cleaned page for a node type.
pub fn plugin_page(node_type: &str) -> Option<String> {
    page(DocSection::Plugins, &plugin_file(node_type))
}

/// A concepts page by file stem (e.g. `supernodes`).
pub fn concept_page(name: &str) -> Option<String> {
    page(DocSection::Concepts, name)
}

/// A reference page by file stem (`context-vars`, `conditions`, `templates`).
pub fn reference_page(name: &str) -> Option<String> {
    page(DocSection::Reference, name)
}

/// Resolves a `featherbit://docs/{section}/{name}` URI.
pub fn read_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix(DOCS_URI_PREFIX)?;
    let (section, name) = rest.split_once('/')?;
    let section = DocSection::parse(section)?;
    match section {
        DocSection::Plugins => plugin_page(name),
        DocSection::Concepts => concept_page(name),
        DocSection::Reference => reference_page(name),
    }
}

/// Every page, for `resources/list`.
pub fn list_pages() -> Vec<DocPage> {
    let mut pages = Vec::new();
    for path in DocsAssets::iter() {
        let path = path.as_ref();
        let (section, stem) = if let Some(s) = path.strip_prefix("reference/plugins/") {
            (DocSection::Plugins, s)
        } else if let Some(s) = path.strip_prefix("concepts/") {
            (DocSection::Concepts, s)
        } else if let Some(s) = path.strip_prefix("reference/") {
            (DocSection::Reference, s)
        } else {
            continue;
        };
        let Some(stem) = stem.strip_suffix(".md") else { continue };
        if stem == "index" {
            continue;
        }
        let Some(file) = DocsAssets::get(path) else { continue };
        let md = String::from_utf8_lossy(&file.data);
        let (title, description) = frontmatter(&md);
        pages.push(DocPage {
            uri: format!("{}{}/{}", DOCS_URI_PREFIX, section.slug(), stem),
            title: if title.is_empty() { stem.to_string() } else { title },
            description,
        });
    }
    pages.sort_by(|a, b| a.uri.cmp(&b.uri));
    pages
}
```

Frontmatter end detection: the `find("\n---")` + `+ 4` skips `\n---`; if the file uses `\n---\n` the leading newline of the body is then dropped by the blank-line collapse, so the `clean_handles_edge_cases` expected string holds. If the assertion differs by one blank line, adjust the implementation (not the test's intent: title → H1, JSX/imports gone, links rewritten, no doubled blank lines).

`regex` is already a dependency. Also the `plugin_page` stub introduced in Task 4 is replaced by the real one here.

- [ ] **Step 4: Run and commit**

Run: `cargo test --lib mcp::docs && cargo test --lib mcp::tools::catalog`
Expected: pass; `get_node_type` now returns a non-null `docs` string.

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
git add src/mcp/docs.rs Cargo.toml Cargo.lock
git commit -m "feat(mcp): embed plugin/concept/reference docs pages as agent-readable resources"
```

---

### Task 8: Prompt templates (`src/mcp/prompts.rs`)

**Files:**
- Create: `src/mcp/prompts.rs`; Modify: `src/mcp/mod.rs` (`pub mod prompts;`)

**Interfaces:**
- Consumes: `tools::call`, `docs::{plugin_page, concept_page}`.
- Produces: `pub struct PromptArg { pub name: &'static str, pub description: &'static str, pub required: bool }`; `pub struct PromptDef { pub name: &'static str, pub description: &'static str, pub args: &'static [PromptArg] }`; `pub fn prompt_defs() -> &'static [PromptDef]`; `pub fn prompt_def(name) -> Option<&'static PromptDef>`; `pub struct RenderedPrompt { pub description: String, pub text: String }`; `pub async fn render(state: &SharedState, name: &str, args: &HashMap<String, String>) -> Result<RenderedPrompt, ToolError>` (`unknown_tool` code reused as `"unknown_prompt"`; missing required arg → `invalid_input`).
- Prompts: `explain_trace(trace_id)`, `why_this_port(trace_id, node_id)`, `why_this_response(trace_id)`, `review_policy(policy_name)`, `design_policy(goal, name?)`, `design_supernode(goal, name?)`, `design_route(goal)`, `diagnose_route(method, path, headers?)`.

- [ ] **Step 1: Implementation with tests**

Create `src/mcp/prompts.rs`:

```rust
//! Precompiled prompts: the "what is happening here?" / "why did this node
//! exit on `false`?" questions, rendered with live data so the agent's model
//! starts from the facts. The same renderer backs MCP `prompts/get` and the
//! Admin API's `GET /api/mcp/prompts/{name}` (the UI's "copy as agent
//! prompt"), so the two are byte-identical.

use std::collections::HashMap;

use serde_json::Value;

use crate::mcp::tools::{self, JsonObject, ToolError};
use crate::state::SharedState;

pub struct PromptArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

pub struct PromptDef {
    pub name: &'static str,
    pub description: &'static str,
    pub args: &'static [PromptArg],
}

const fn arg(name: &'static str, description: &'static str, required: bool) -> PromptArg {
    PromptArg { name, description, required }
}

static PROMPTS: [PromptDef; 8] = [
    PromptDef { name: "explain_trace", description: "What is happening in this request? Walk through a trace node by node.", args: &[arg("trace_id", "Trace id from list_traces or the Debug panel", true)] },
    PromptDef { name: "why_this_port", description: "Why did a node exit on this port (false/denied/error/limited…)?", args: &[arg("trace_id", "Trace id", true), arg("node_id", "Node id of the step to explain", true)] },
    PromptDef { name: "why_this_response", description: "Why did the client receive this status code?", args: &[arg("trace_id", "Trace id", true)] },
    PromptDef { name: "review_policy", description: "Review a stored policy for dead nodes, ordering problems and missing error handling.", args: &[arg("policy_name", "Policy name", true)] },
    PromptDef { name: "design_policy", description: "Design a new policy for a goal, validate it, and apply it (or hand back YAML).", args: &[arg("goal", "What the policy must do, in plain words", true), arg("name", "Policy name to create", false)] },
    PromptDef { name: "design_supernode", description: "Design a reusable supernode for a goal.", args: &[arg("goal", "What the supernode must do", true), arg("name", "Supernode name to create", false)] },
    PromptDef { name: "design_route", description: "Design a route (match rule + policy reference) for a goal.", args: &[arg("goal", "Which requests should match and which policy should handle them", true)] },
    PromptDef { name: "diagnose_route", description: "Which route would match this request, and what would its policy do?", args: &[arg("method", "HTTP method", true), arg("path", "Request path", true), arg("headers", "Optional headers as 'Name: value' lines", false)] },
];

pub fn prompt_defs() -> &'static [PromptDef] {
    &PROMPTS
}

pub fn prompt_def(name: &str) -> Option<&'static PromptDef> {
    PROMPTS.iter().find(|p| p.name == name)
}

/// A rendered prompt: one user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub description: String,
    pub text: String,
}

fn required<'a>(args: &'a HashMap<String, String>, name: &str) -> Result<&'a str, ToolError> {
    args.get(name)
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolError::invalid_input(format!("prompt argument '{name}' is required")))
}

fn obj(v: Value) -> JsonObject {
    v.as_object().cloned().unwrap_or_default()
}

fn block(title: &str, v: &Value) -> String {
    format!("## {title}\n\n```json\n{}\n```\n\n", serde_json::to_string_pretty(v).unwrap_or_default())
}

const MCP_HINT: &str = "You are connected to the gateway's `featherbit` MCP server: prefer its tools (get_trace_step, get_node_type, validate_policy, run_sandbox) for anything not inlined below.\n\n";

const WIRING_RULE: &str = "Rules of this gateway: a policy is a node graph. Every node's `success`/`out` port and every declared outcome port (e.g. `denied`, `redirect`, `limited`, `true`/`false`) MUST be wired to another node's `in` port or the policy fails to compile; only `error` ports may be left unwired (they fall back to the policy's `error_handler`). Every policy has exactly one `listener` (entry) and one `client` (exit). Plugin config keys are documented per node type — call get_node_type(type) before configuring a node.\n\n";

pub async fn render(state: &SharedState, name: &str, args: &HashMap<String, String>) -> Result<RenderedPrompt, ToolError> {
    let def = prompt_def(name).ok_or_else(|| {
        let mut e = ToolError::unknown_tool(name);
        e.code = "unknown_prompt";
        e.message = format!("no prompt named '{name}'");
        e
    })?;
    let text = match name {
        "explain_trace" => {
            let id = required(args, "trace_id")?;
            let trace = tools::call(state, "get_trace", obj(serde_json::json!({"id": id}))).await?;
            format!(
                "{MCP_HINT}# What is happening in this request?\n\nBelow is a debug trace of one request through policy `{}`. Walk through it node by node: for each step say what the node did (use its `changes`), which port it exited on and why. Finish with: which node produced the final response (status {}), and whether anything looks wrong.\n\n{}",
                trace["policy"].as_str().unwrap_or("?"),
                trace["status"],
                block("Trace", &trace)
            )
        }
        "why_this_port" => {
            let id = required(args, "trace_id")?;
            let node = required(args, "node_id")?;
            let step = tools::call(state, "get_trace_step", obj(serde_json::json!({"id": id, "node_id": node}))).await?;
            let node_type = step["step"]["node_type"].as_str().unwrap_or("").to_string();
            let port = step["step"]["port"].as_str().unwrap_or("error").to_string();
            let docs = crate::mcp::docs::plugin_page(&node_type).unwrap_or_default();
            format!(
                "{MCP_HINT}# Why did node `{node}` (`{node_type}`) exit on port `{port}`?\n\nExplain, pointing at the exact config keys and context values (headers, query, message, errors) that decided it. If it is an error, name the error code and the fix.\n\n{}{}## Documentation for `{node_type}`\n\n{docs}\n",
                block("Step (before / after / changes / node_config)", &step),
                if step["step"]["outcome"]["kind"] == "error" { "The step's `outcome` is an error — start from its `code` and `message`.\n\n" } else { "" }
            )
        }
        "why_this_response" => {
            let id = required(args, "trace_id")?;
            let trace = tools::call(state, "get_trace", obj(serde_json::json!({"id": id}))).await?;
            let status = trace["status"].clone();
            let setter = trace["steps"]
                .as_array()
                .and_then(|steps| {
                    steps.iter().rev().find(|s| {
                        s["changes"].as_array().is_some_and(|c| c.iter().any(|ch| ch["path"] == "response.status_code"))
                    })
                })
                .and_then(|s| s["node_id"].as_str())
                .unwrap_or("(no node changed the status — it is the upstream's or the default)");
            format!(
                "{MCP_HINT}# Why did the client receive status {status}?\n\nThe last node to change `response.status_code` was `{setter}`. Explain why it did, using the trace below, and say what would have to change for the request to succeed.\n\n{}",
                block("Trace", &trace)
            )
        }
        "review_policy" => {
            let pname = required(args, "policy_name")?;
            let policy = tools::call(state, "get_policy", obj(serde_json::json!({"name": pname}))).await?;
            let types: Vec<String> = policy["policy"]["nodes"]
                .as_array()
                .map(|n| n.iter().filter_map(|x| x["type"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let catalog = tools::call(state, "list_node_types", JsonObject::new()).await?;
            let used: Vec<&Value> = catalog["node_types"].as_array().map(|all| all.iter().filter(|e| types.iter().any(|t| e["type"] == *t)).collect()).unwrap_or_default();
            format!(
                "{MCP_HINT}{WIRING_RULE}# Review policy `{pname}`\n\nReview for: unreachable nodes; ordering problems (authentication or rate limiting after `upstream`; request rewrites after the proxy; response rewrites before it); outcome/error ports routed straight to `client` where a proper rejection or error handler is expected; redundant or contradictory nodes; missing `error_handler`. Propose concrete YAML edits.\n\n{}{}",
                block("Policy", &policy),
                block("Port declarations of the node types used", &Value::Array(used.into_iter().cloned().collect()))
            )
        }
        "design_policy" | "design_supernode" | "design_route" => {
            let goal = required(args, "goal")?;
            let target_name = args.get("name").cloned();
            let catalog = tools::call(state, "list_node_types", JsonObject::new()).await?;
            let existing = match name {
                "design_route" => tools::call(state, "list_routes", JsonObject::new()).await?,
                _ => tools::call(state, "list_policies", JsonObject::new()).await?,
            };
            let (what, put_tool, validate_tool, extra) = match name {
                "design_policy" => ("policy", "put_policy", "validate_policy", String::new()),
                "design_supernode" => (
                    "supernode",
                    "put_supernode",
                    "validate_supernode",
                    format!("## Supernode rules\n\n{}\n", crate::mcp::docs::concept_page("supernodes").unwrap_or_default()),
                ),
                _ => ("route", "put_route", "validate_policy", String::new()),
            };
            let named = target_name.map(|n| format!(" named `{n}`")).unwrap_or_default();
            format!(
                "{MCP_HINT}{WIRING_RULE}# Design a {what}{named}\n\nGoal: {goal}\n\nWorkflow: (1) pick node types from the catalog below and call get_node_type for each to learn its config keys and ports; (2) write the {what} as YAML; (3) validate with {validate_tool}; (4) call {put_tool} with dry_run=true, fix every reported error, then call it for real. If your token is read-only (write tools are missing or return `forbidden`), stop after validation and return the YAML for a human to apply.\n\n{extra}{}{}",
                block("Node type catalog (type, description, ports)", &catalog),
                block("Existing definitions (avoid name clashes; reuse where sensible)", &existing)
            )
        }
        "diagnose_route" => {
            let method = required(args, "method")?;
            let path = required(args, "path")?;
            let headers = args.get("headers").cloned().unwrap_or_default();
            let routes = tools::call(state, "list_routes", JsonObject::new()).await?;
            format!(
                "{MCP_HINT}# Diagnose `{method} {path}`\n\nDetermine which route matches this request (routes are evaluated in declaration order; the first match wins; `match.path` is a prefix unless the docs say otherwise — check `featherbit://docs/concepts/policies-and-graphs` if unsure). Then call run_sandbox with `policy` set to that route's policy and a `context` of {{method: \"{method}\", path: \"{path}\", headers: …}} and explain what the policy would do to it.\n\nHeaders:\n```\n{headers}\n```\n\n{}",
                block("Routes", &routes)
            )
        }
        _ => unreachable!("prompt_def guarantees a known name"),
    };
    Ok(RenderedPrompt { description: def.description.to_string(), text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defs_are_unique_and_documented() {
        let mut seen = std::collections::HashSet::new();
        for p in prompt_defs() {
            assert!(seen.insert(p.name));
            assert!(!p.description.is_empty());
            assert!(!p.args.is_empty());
        }
        assert!(prompt_def("explain_trace").is_some());
        assert!(prompt_def("nope").is_none());
    }

    #[tokio::test]
    async fn unknown_and_missing_args() {
        let s = state("{}", ECHO_GATEWAY);
        let err = render(&s, "nope", &args(&[])).await.unwrap_err();
        assert_eq!(err.code, "unknown_prompt");
        let err = render(&s, "explain_trace", &args(&[])).await.unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert!(err.message.contains("trace_id"));
    }

    #[tokio::test]
    async fn trace_prompts_render_from_a_sandbox_run() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        let run = tools::call(&s, "run_sandbox", obj(serde_json::json!({"policy": "echo-policy", "context": {"path": "/hello"}}))).await.unwrap();
        let id = run["stored_trace_id"].as_str().unwrap();

        let p = render(&s, "explain_trace", &args(&[("trace_id", id)])).await.unwrap();
        assert!(p.text.contains("# What is happening in this request?"));
        assert!(p.text.contains("echo-policy"));
        assert!(p.text.starts_with("You are connected"));

        let p = render(&s, "why_this_port", &args(&[("trace_id", id), ("node_id", "e")])).await.unwrap();
        assert!(p.text.contains("exit on port `success`"), "{}", &p.text[..200]);
        assert!(p.text.contains("# echo"), "docs page inlined");
        assert!(p.text.contains("node_config"));

        let p = render(&s, "why_this_response", &args(&[("trace_id", id)])).await.unwrap();
        assert!(p.text.contains("receive status"));

        let err = render(&s, "why_this_port", &args(&[("trace_id", id), ("node_id", "zz")])).await.unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[tokio::test]
    async fn authoring_prompts_render() {
        let s = state("{}", ECHO_GATEWAY);
        let p = render(&s, "review_policy", &args(&[("policy_name", "echo-policy")])).await.unwrap();
        assert!(p.text.contains("# Review policy `echo-policy`"));
        assert!(p.text.contains("\"type\": \"echo\""));
        let p = render(&s, "design_policy", &args(&[("goal", "rate limit by api key"), ("name", "rl")])).await.unwrap();
        assert!(p.text.contains("# Design a policy named `rl`"));
        assert!(p.text.contains("put_policy with dry_run=true"));
        assert!(p.text.contains("limit-count"));
        let p = render(&s, "design_supernode", &args(&[("goal", "auth guard")])).await.unwrap();
        assert!(p.text.contains("## Supernode rules"));
        let p = render(&s, "design_route", &args(&[("goal", "/v2 to the v2 policy")])).await.unwrap();
        assert!(p.text.contains("\"hello\""), "existing routes inlined");
        let p = render(&s, "diagnose_route", &args(&[("method", "GET"), ("path", "/hello")])).await.unwrap();
        assert!(p.text.contains("Diagnose `GET /hello`"));
        assert!(p.text.contains("run_sandbox"));
    }
}
```

The `why_this_port` docs assertion expects the echo page's H1 to be `# echo` (its frontmatter title). If the title differs, assert on a stable substring of that page instead.

- [ ] **Step 2: Run and commit**

Run: `cargo test --lib mcp::prompts`
Expected: pass.

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
git add src/mcp/prompts.rs src/mcp/mod.rs
git commit -m "feat(mcp): precompiled debugging and authoring prompts rendered with live data"
```

---

### Task 9: rmcp server handler, mounting, and end-to-end protocol tests

**Files:**
- Create: `src/mcp/server.rs` (feature `mcp`)
- Modify: `src/mcp/mod.rs` (`#[cfg(feature = "mcp")] pub mod server;`, `router`/`disabled_router`), `src/admin/mod.rs:142-185` (`build_router`)

**Interfaces:**
- Consumes: `auth::{McpAuthState, McpPrincipal, bearer_middleware}`, `tools::{tool_defs, tool_def, call, ToolError}`, `docs::{list_pages, read_uri}`, `prompts::{prompt_defs, render}`.
- Produces: `crate::mcp::router(cfg: &McpConfig, state: Arc<SharedState>) -> axum::Router` (feature `mcp`; mounts the authenticated service at `cfg.path`); `crate::mcp::disabled_router(path: &str) -> axum::Router` (404 + warn); `pub(crate) fn build_router` in `src/admin/mod.rs` (visibility only, for the server tests).
- Behavior: `tools/list` filtered by scope; `tools/call` on a write tool with a read token → tool error `forbidden`; unknown tool → JSON-RPC `invalid_params`; every call logged `INFO mcp tool call token=<name|unnamed> scope=<s> tool=<t> outcome=<ok|error:<code>> duration_ms=<n>`.

- [ ] **Step 1: Wire the routers**

In `src/mcp/mod.rs` add:

```rust
pub mod auth;
pub mod docs;
pub mod prompts;
#[cfg(feature = "mcp")]
pub mod server;
pub mod tools;

use std::sync::Arc;

use axum::routing::any;
use axum::Router;

/// Router fragment answering the MCP path with `404` when the feature is off
/// or `admin.mcp.enabled` is false (the `/api/debug/*` convention: never
/// advertise a disabled surface). Logs a warning naming the key.
pub fn disabled_router(path: &str) -> Router {
    async fn not_found() -> axum::response::Response {
        tracing::warn!(
            "MCP endpoint was requested but is disabled; set `admin.mcp.enabled: true` \
             (FEATHERBIT_MCP_ENABLED=true) with at least one token in system.yaml and restart"
        );
        (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response()
    }
    use axum::response::IntoResponse;
    Router::new().route(path, any(not_found))
}

/// The live MCP endpoint: bearer auth → rmcp Streamable HTTP service.
#[cfg(feature = "mcp")]
pub fn router(cfg: &crate::config::McpConfig, state: Arc<crate::state::SharedState>) -> Router {
    let auth_state = Arc::new(auth::McpAuthState::from_config(cfg));
    Router::new()
        .route_service(&cfg.path, server::build_service(state))
        .route_layer(axum::middleware::from_fn_with_state(auth_state, auth::bearer_middleware))
}
```

(Move the `use axum::response::IntoResponse;` to the top-level imports; it is shown inline only for locality.)

In `src/admin/mod.rs` change `fn build_router(` to `pub(crate) fn build_router(` and restructure its body so the MCP router is merged **after** the Basic-Auth layer and **before** the fallback:

```rust
pub(crate) fn build_router(admin_config: &AdminConfig, state: Arc<SharedState>) -> Router {
    let api = Router::new()
        .merge(routes::router())
        .merge(acme::router())
        .merge(policies::router())
        .merge(plugin_configs::router())
        .merge(supernodes::router())
        .merge(consumers::router())
        .merge(sessions::router())
        .merge(status::router())
        .merge(stores::router())
        .merge(debug::router())
        .merge(vars::router())
        .merge(env_vars::router())
        .merge(mcp::router())
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(auth::AuthState {
                username: admin_config.username.clone(),
                password: admin_config.password.clone(),
            }),
            auth::basic_auth_middleware,
        ))
        .with_state(state.clone());

    // MCP lives OUTSIDE the Basic Auth layer: its own bearer tokens, its own
    // 404-when-disabled route (an explicit route, so `ui_enabled` cannot turn
    // the path into the SPA index).
    let mcp_path = admin_config
        .mcp
        .as_ref()
        .map(|m| m.path.clone())
        .unwrap_or_else(|| "/mcp".to_string());
    #[cfg(feature = "mcp")]
    let mcp_router = match &admin_config.mcp {
        Some(cfg) if cfg.enabled => crate::mcp::router(cfg, state),
        _ => {
            tracing::info!("MCP server disabled (admin.mcp.enabled = false)");
            crate::mcp::disabled_router(&mcp_path)
        }
    };
    #[cfg(not(feature = "mcp"))]
    let mcp_router = {
        if admin_config.mcp.as_ref().is_some_and(|m| m.enabled) {
            tracing::warn!(
                "MCP server not compiled in (built without the \"mcp\" feature); admin.mcp ignored"
            );
        }
        let _ = &state;
        crate::mcp::disabled_router(&mcp_path)
    };
    let app = api.merge(mcp_router);

    // (existing fallback block unchanged: SPA / not_found)
    #[cfg(feature = "ui")]
    let app = if admin_config.ui_enabled {
        app.fallback(get(ui::serve_ui))
    } else {
        app.fallback(not_found)
    };
    #[cfg(not(feature = "ui"))]
    let app = app.fallback(not_found);

    app
}
```

Here `.merge(mcp::router())` inside `api` refers to the **Admin** module `src/admin/mcp.rs` created in Task 10 — for this task, leave that line out and add it in Task 10.

Add a test to `src/admin/mod.rs`'s existing test module:

```rust
    #[tokio::test]
    async fn test_mcp_path_is_404_when_disabled_even_with_ui() {
        let app = build_router(&admin_config(true), test_state());
        let resp = app
            .oneshot(Request::post("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], br#"{"error":"not_found"}"#);
    }
```

- [ ] **Step 2: The server handler**

Create `src/mcp/server.rs`:

```rust
//! `rmcp` adapter: exposes the tool/resource/prompt layer over MCP and builds
//! the Streamable HTTP tower service mounted on the admin router.

use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::ServerHandler;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer};

use crate::config::McpScope;
use crate::mcp::auth::McpPrincipal;
use crate::mcp::tools::{self, JsonObject, ToolError};
use crate::mcp::{docs, prompts};
use crate::state::SharedState;

/// Orientation sent to every client at `initialize`.
const INSTRUCTIONS: &str = "\
featherbit is an API gateway whose behavior is declared as node-graph POLICIES referenced by ROUTES.
- A policy is a graph of typed nodes (plugins) joined by edges `from: node.port` → `to: node.in`. It has exactly one `listener` (entry) and one `client` (exit).
- Every node's `success`/`out` port AND every declared outcome port (`denied`, `redirect`, `limited`, `broken`, `preflight`, `abort`, `routed`, `hit`, `true`/`false`, …) MUST be wired, or the policy fails to compile. Only `error` ports may be left unwired (they fall back to the policy's `error_handler`).
- Plugin config keys are documented per node type: call `get_node_type(<type>)` before writing a node's `config`. `list_node_types` lists them all; `list_vars` lists the `$var` names usable inside config.
- Authoring loop: get_node_type → write YAML → validate_policy → put_*(dry_run=true) → put_*. Payloads may be JSON objects or YAML strings.
- Debugging: list_traces / get_trace / get_trace_step (context before/after a node, its exit port, the diff) and run_sandbox (execute a policy against a synthetic request). They need `debug.enabled` in system.yaml; the tools tell you if it is off.
- Supernodes are reusable subgraphs with input/output/error boundary nodes; `featherbit://docs/concepts/supernodes` explains the rules.
- `${ENV_VAR}` placeholders in config are intentional and stay unresolved; never replace them with literal secrets.
- If your token is read-only, write tools are hidden (or return `forbidden`): finish by returning validated YAML for a human to apply.
Use the prompts (explain_trace, why_this_port, why_this_response, review_policy, design_policy, design_supernode, design_route, diagnose_route) for the common questions.";

/// The MCP server: a cheap handle over the shared state.
#[derive(Clone)]
pub struct McpServer {
    state: Arc<SharedState>,
}

impl McpServer {
    pub fn new(state: Arc<SharedState>) -> Self {
        Self { state }
    }
}

/// Builds the tower service to mount at `admin.mcp.path`. Host validation is
/// disabled (agents reach the admin listener by any name; our own middleware
/// enforces `Origin`), sessions are rmcp's default in-memory manager.
pub fn build_service(state: Arc<SharedState>) -> StreamableHttpService<McpServer, LocalSessionManager> {
    let server = McpServer::new(state);
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .disable_allowed_hosts()
            .disable_allowed_origins(),
    )
}

/// The principal the bearer middleware attached to this HTTP request.
fn principal(ctx: &RequestContext<RoleServer>) -> Result<McpPrincipal, McpError> {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<McpPrincipal>().cloned())
        .ok_or_else(|| McpError::invalid_request("request carries no MCP principal (auth middleware missing)", None))
}

fn tool_from_def(def: &tools::ToolDef) -> Tool {
    Tool::new(def.name, def.description, Arc::new((def.input_schema)()))
}

fn error_result(e: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_json().to_string())])
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("featherbit", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let p = principal(&ctx)?;
        let tools = tools::tool_defs()
            .iter()
            .filter(|d| p.scope.allows(d.scope))
            .map(tool_from_def)
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let p = principal(&ctx)?;
        let name = request.name.to_string();
        let def = tools::tool_def(&name)
            .ok_or_else(|| McpError::invalid_params(format!("tool not found: {name}"), None))?;
        let started = Instant::now();
        let outcome = if !p.scope.allows(def.scope) {
            Err(ToolError::forbidden(p.scope))
        } else {
            let args: JsonObject = request.arguments.unwrap_or_default();
            tools::call(&self.state, &name, args).await
        };
        tracing::info!(
            "mcp tool call token={} scope={} tool={} outcome={} duration_ms={}",
            p.name.as_deref().unwrap_or("unnamed"),
            p.scope.as_str(),
            name,
            match &outcome { Ok(_) => "ok".to_string(), Err(e) => format!("error:{}", e.code) },
            started.elapsed().as_millis()
        );
        let result = match outcome {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(v.to_string())]),
            Err(e) => error_result(&e),
        };
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        principal(&ctx)?;
        let mut resources: Vec<Resource> = docs::list_pages()
            .into_iter()
            .map(|p| {
                let mut r = Resource::new(p.uri, p.title);
                if !p.description.is_empty() {
                    r = r.with_description(p.description);
                }
                r
            })
            .collect();
        let gw = self.state.gateway.read().await;
        for r in &gw.routes {
            resources.push(Resource::new(format!("featherbit://routes/{}", r.name), format!("route {}", r.name)));
        }
        for p in &gw.policies {
            resources.push(Resource::new(format!("featherbit://policies/{}", p.name), format!("policy {}", p.name)));
        }
        for s in &gw.supernodes {
            resources.push(Resource::new(format!("featherbit://supernodes/{}", s.name), format!("supernode {}", s.name)));
        }
        Ok(ListResourcesResult { resources, ..Default::default() })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        principal(&ctx)?;
        let resource_templates = vec![
            ResourceTemplate::new("featherbit://docs/plugins/{type}", "Node type documentation"),
            ResourceTemplate::new("featherbit://docs/concepts/{name}", "Concept guide"),
            ResourceTemplate::new("featherbit://docs/reference/{name}", "Reference page (context-vars, conditions, templates)"),
            ResourceTemplate::new("featherbit://routes/{name}", "Route definition (YAML)"),
            ResourceTemplate::new("featherbit://policies/{name}", "Policy definition (YAML)"),
            ResourceTemplate::new("featherbit://supernodes/{name}", "Supernode definition (YAML)"),
            ResourceTemplate::new("featherbit://traces/{id}", "Debug trace (JSON, with snapshots)"),
        ];
        Ok(ListResourceTemplatesResult { resource_templates, ..Default::default() })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        principal(&ctx)?;
        let uri = request.uri.clone();
        let not_found = || McpError::resource_not_found("resource_not_found", Some(serde_json::json!({"uri": uri})));

        let (text, mime) = if let Some(md) = docs::read_uri(&uri) {
            (md, "text/markdown")
        } else if let Some(name) = uri.strip_prefix("featherbit://routes/") {
            let gw = self.state.gateway.read().await;
            let r = gw.routes.iter().find(|r| r.name == name).ok_or_else(not_found)?;
            (serde_yaml::to_string(r).map_err(|e| McpError::internal_error(e.to_string(), None))?, "application/yaml")
        } else if let Some(name) = uri.strip_prefix("featherbit://policies/") {
            let gw = self.state.gateway.read().await;
            let p = gw.policies.iter().find(|p| p.name == name).ok_or_else(not_found)?;
            (serde_yaml::to_string(p).map_err(|e| McpError::internal_error(e.to_string(), None))?, "application/yaml")
        } else if let Some(name) = uri.strip_prefix("featherbit://supernodes/") {
            let gw = self.state.gateway.read().await;
            let s = gw.supernodes.iter().find(|s| s.name == name).ok_or_else(not_found)?;
            (serde_yaml::to_string(s).map_err(|e| McpError::internal_error(e.to_string(), None))?, "application/yaml")
        } else if let Some(id) = uri.strip_prefix("featherbit://traces/") {
            let v = tools::call(&self.state, "get_trace", tools_obj(serde_json::json!({"id": id, "include_snapshots": true})))
                .await
                .map_err(|e| match e.code {
                    "not_found" => not_found(),
                    _ => McpError::internal_error(e.message, Some(e.to_json())),
                })?;
            (v.to_string(), "application/json")
        } else {
            return Err(not_found());
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri).with_mime_type(mime)]).into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        principal(&ctx)?;
        let prompts = prompts::prompt_defs()
            .iter()
            .map(|d| {
                Prompt::new(
                    d.name,
                    Some(d.description),
                    Some(
                        d.args
                            .iter()
                            .map(|a| PromptArgument::new(a.name).with_description(a.description).with_required(a.required))
                            .collect(),
                    ),
                )
            })
            .collect();
        Ok(ListPromptsResult { prompts, ..Default::default() })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        principal(&ctx)?;
        // Spec-compliant clients send string values; accept anything and stringify.
        let args: std::collections::HashMap<String, String> = request
            .arguments
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, match v { serde_json::Value::String(s) => s, other => other.to_string() }))
            .collect();
        let rendered = prompts::render(&self.state, &request.name, &args)
            .await
            .map_err(|e| match e.code {
                "unknown_prompt" => McpError::invalid_params(e.message, None),
                "invalid_input" => McpError::invalid_params(e.message, None),
                _ => McpError::internal_error(e.message, Some(e.to_json())),
            })?;
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, rendered.text)])
            .with_description(rendered.description)
            .into())
    }
}

fn tools_obj(v: serde_json::Value) -> JsonObject {
    v.as_object().cloned().unwrap_or_default()
}
```

`http` is already a direct dependency (`http = "1"`), so `http::request::Parts` resolves. The three `serde_yaml::to_string(..)` branches repeat on purpose (different concrete types); do not try to unify them behind a trait object.

- [ ] **Step 3: Protocol tests over a real socket**

Append to `src/mcp/server.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AdminConfig;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};
    use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, GetPromptRequestParams, ReadResourceRequestParams};
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;

    const READ: &str = "read-token-0123456789";
    const WRITE: &str = "write-token-0123456789";

    fn admin(enabled: bool) -> AdminConfig {
        serde_yaml::from_str(&format!(
            "username: u\npassword: p\nui_enabled: false\nmcp:\n  enabled: {enabled}\n  tokens:\n    - token: {READ}\n      scope: read\n      name: reader\n    - token: {WRITE}\n      scope: write\n"
        ))
        .unwrap()
    }

    /// Serves the admin router on a loopback port; returns the MCP URL.
    async fn serve(enabled: bool, debug: bool) -> (String, Arc<SharedState>) {
        let sys = if debug { "debug:\n  enabled: true\n" } else { "{}" };
        let st = state(sys, ECHO_GATEWAY);
        let app = crate::admin::build_router(&admin(enabled), st.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/mcp"), st)
    }

    async fn client(url: &str, token: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ClientInfo> {
        let cfg = StreamableHttpClientTransportConfig::with_uri(url.to_string()).auth_header(token);
        let transport = StreamableHttpClientTransport::from_config(cfg);
        ClientInfo::new(ClientCapabilities::default(), Implementation::new("test", "0"))
            .serve(transport)
            .await
            .expect("initialize")
    }

    fn text_of(r: &CallToolResult) -> serde_json::Value {
        let s = r.content.iter().find_map(|c| c.as_text().map(|t| t.text.clone())).expect("text content");
        serde_json::from_str(&s).unwrap()
    }

    #[tokio::test]
    async fn initialize_lists_and_filters_tools_by_scope() {
        let (url, _) = serve(true, false).await;
        let reader = client(&url, READ).await;
        let info = reader.peer_info().unwrap();
        assert_eq!(info.server_info.name, "featherbit");
        assert!(info.instructions.as_deref().unwrap().contains("MUST be wired"));
        let names: Vec<String> = reader.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"get_policy".to_string()));
        assert!(!names.iter().any(|n| n.starts_with("put_")), "{names:?}");
        reader.cancel().await.unwrap();

        let writer = client(&url, WRITE).await;
        let names: Vec<String> = writer.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"put_policy".to_string()));
        writer.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn read_parity_and_forbidden_write() {
        let (url, st) = serve(true, false).await;
        let reader = client(&url, READ).await;
        let r = reader.call_tool(CallToolRequestParams::new("get_policy").with_arguments(obj(serde_json::json!({"name": "echo-policy"})))).await.unwrap();
        assert_ne!(r.is_error, Some(true));
        let v = text_of(&r);
        let expected = serde_json::to_value(st.gateway.read().await.policies[0].clone()).unwrap();
        assert_eq!(v["policy"], expected);

        let r = reader.call_tool(CallToolRequestParams::new("put_policy").with_arguments(obj(serde_json::json!({"name": "x", "definition": {}})))).await.unwrap();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(text_of(&r)["code"], "forbidden");

        let err = reader.call_tool(CallToolRequestParams::new("no_such_tool")).await;
        assert!(err.is_err(), "unknown tool is a protocol error");
        reader.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn write_dry_run_then_apply_is_visible_and_routable() {
        let (url, st) = serve(true, false).await;
        let writer = client(&url, WRITE).await;
        let def = serde_json::json!({"nodes": [{"id": "l", "type": "listener"}, {"id": "e", "type": "echo", "config": {}}, {"id": "c", "type": "client"}],
                                     "edges": [{"from": "l.out", "to": "e.in"}, {"from": "e.out", "to": "c.in"}]});
        let r = writer.call_tool(CallToolRequestParams::new("put_policy").with_arguments(obj(serde_json::json!({"name": "p2", "definition": def, "dry_run": true})))).await.unwrap();
        assert_eq!(text_of(&r)["applied"], false);
        assert!(st.gateway.read().await.policies.iter().all(|p| p.name != "p2"));

        let r = writer.call_tool(CallToolRequestParams::new("put_policy").with_arguments(obj(serde_json::json!({"name": "p2", "definition": def})))).await.unwrap();
        assert_eq!(text_of(&r)["applied"], true);
        writer.call_tool(CallToolRequestParams::new("put_route").with_arguments(obj(serde_json::json!({"name": "r2", "definition": {"match": {"path": "/two"}, "policy": "p2"}})))).await.unwrap();
        assert_eq!(st.routes.read().await.len(), 2, "hot-applied to the route table");

        let bad = serde_json::json!({"nodes": [{"id": "l", "type": "listener"}, {"id": "k", "type": "key-auth", "config": {}}, {"id": "c", "type": "client"}],
                                     "edges": [{"from": "l.out", "to": "k.in"}, {"from": "k.out", "to": "c.in"}]});
        let r = writer.call_tool(CallToolRequestParams::new("put_policy").with_arguments(obj(serde_json::json!({"name": "bad", "definition": bad})))).await.unwrap();
        assert_eq!(r.is_error, Some(true));
        let v = text_of(&r);
        assert_eq!(v["code"], "invalid_config");
        assert!(v["errors"][0].as_str().unwrap().contains("bad"));
        writer.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn resources_and_prompts() {
        let (url, _) = serve(true, true).await;
        let c = client(&url, READ).await;
        let res = c.list_resources(None).await.unwrap();
        assert!(res.resources.iter().any(|r| r.uri == "featherbit://docs/plugins/limit-count"));
        assert!(res.resources.iter().any(|r| r.uri == "featherbit://policies/echo-policy"));
        let page = c.read_resource(ReadResourceRequestParams::new("featherbit://docs/plugins/limit-count")).await.unwrap();
        let text = match &page.contents[0] { ResourceContents::TextResourceContents { text, .. } => text.clone(), _ => panic!("text") };
        assert!(text.starts_with("# limit-count"));
        let pol = c.read_resource(ReadResourceRequestParams::new("featherbit://policies/echo-policy")).await.unwrap();
        let text = match &pol.contents[0] { ResourceContents::TextResourceContents { text, .. } => text.clone(), _ => panic!("text") };
        assert!(text.contains("name: echo-policy"));
        assert!(c.read_resource(ReadResourceRequestParams::new("featherbit://policies/nope")).await.is_err());

        let prompts = c.list_prompts(None).await.unwrap();
        assert!(prompts.prompts.iter().any(|p| p.name == "why_this_port"));
        let run = c.call_tool(CallToolRequestParams::new("run_sandbox").with_arguments(obj(serde_json::json!({"policy": "echo-policy", "context": {"path": "/hello"}})))).await.unwrap();
        let id = text_of(&run)["stored_trace_id"].as_str().unwrap().to_string();
        let p = c.get_prompt(GetPromptRequestParams::new("explain_trace").with_arguments(obj(serde_json::json!({"trace_id": id})))).await.unwrap();
        let msg = match &p.messages[0].content { ContentBlock::Text(t) => t.text.clone(), _ => panic!("text") };
        assert!(msg.contains("# What is happening in this request?"));
        assert!(c.get_prompt(GetPromptRequestParams::new("nope")).await.is_err());
        c.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn debug_disabled_surfaces_as_tool_error() {
        let (url, _) = serve(true, false).await;
        let c = client(&url, READ).await;
        let r = c.call_tool(CallToolRequestParams::new("list_traces")).await.unwrap();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(text_of(&r)["code"], "debug_disabled");
        c.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn raw_http_auth_and_disabled_behaviors() {
        let (url, _) = serve(true, false).await;
        let http = reqwest::Client::new();
        let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}});
        let resp = http.post(&url).header("accept", "application/json, text/event-stream").json(&init).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(resp.headers().get("www-authenticate").unwrap(), "Bearer realm=\"featherbit-mcp\"");
        let resp = http.post(&url).header("accept", "application/json, text/event-stream").header("origin", "http://evil.example").bearer_auth(READ).json(&init).send().await.unwrap();
        assert_eq!(resp.status(), 403);
        // Basic Auth does not open the MCP door.
        let resp = http.post(&url).header("accept", "application/json, text/event-stream").basic_auth("u", Some("p")).json(&init).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        // MCP tokens do not open the Admin API.
        let api = url.replace("/mcp", "/api/policies");
        let resp = http.get(&api).bearer_auth(WRITE).send().await.unwrap();
        assert_eq!(resp.status(), 401);

        let (url, _) = serve(false, false).await;
        let resp = http.post(&url).bearer_auth(READ).json(&init).send().await.unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(resp.json::<serde_json::Value>().await.unwrap(), serde_json::json!({"error": "not_found"}));
    }
}
```

Two API names to confirm at compile time (the rest are verified): `ContentBlock::as_text()` — if it does not exist, match `ContentBlock::Text(t) => t.text.clone()` instead (as the prompt test does); and `Implementation` fields on `peer_info().server_info` — `name` is public per the verified struct. The `reqwest` in this test is the dev-dependency version (0.12); the rmcp client pulls 0.13 separately — both compile side by side.

- [ ] **Step 4: Run**

Run: `cargo test --lib mcp::server && cargo test --lib admin::tests && cargo check --no-default-features --locked && cargo test --locked --no-default-features --features ui,redis-store --lib mcp::`
Expected: all green; the feature-off run compiles `src/mcp/{auth,tools,docs,prompts}` and skips `server`.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings && cargo clippy --all-targets --no-default-features --locked -- -D warnings
git add src/mcp src/admin/mod.rs
git commit -m "feat(mcp): rmcp Streamable HTTP server mounted on the admin listener behind scoped bearer tokens"
```

---

### Task 10: Admin endpoints for the UI (`/api/mcp/*`)

**Files:**
- Create: `src/admin/mcp.rs`; Modify: `src/admin/mod.rs` (`mod mcp;`, `.merge(mcp::router())` inside `api`)

**Interfaces:**
- Produces (Basic Auth, never gated):
  - `GET /api/mcp/status` → `{"compiled": bool, "enabled": bool, "path": string, "token_count": n, "scopes": ["read"|"write", …] (sorted, unique)}`
  - `GET /api/mcp/prompts` → `{"prompts": [{"name", "description", "arguments": [{"name", "description", "required"}]}]}`
  - `GET /api/mcp/prompts/{name}?<arg>=<value>…` → `{"name", "description", "text"}`; `404 {"error":"not_found"}` for unknown prompt or missing trace/policy; `400 {"error": "..."}` for a missing required argument or debug-off (`{"error":"debug_disabled", "hint": …}`).

- [ ] **Step 1: Tests + implementation**

Create `src/admin/mcp.rs`:

```rust
//! Admin API companions to the MCP server, for the web UI: connection status
//! (never token values) and the rendered prompt texts behind "Copy as agent
//! prompt". Basic-Auth like the rest of `/api`; answer whether or not MCP is
//! enabled — rendering a prompt exposes nothing a Basic Auth user cannot
//! already read.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::mcp::prompts;
use crate::state::SharedState;

pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/mcp/status", get(status))
        .route("/api/mcp/prompts", get(list_prompts))
        .route("/api/mcp/prompts/{name}", get(render_prompt))
}

async fn status(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let mcp = state.system.admin.as_ref().and_then(|a| a.mcp.as_ref());
    let mut scopes: Vec<&str> = mcp
        .map(|m| m.tokens.iter().map(|t| t.scope.as_str()).collect())
        .unwrap_or_default();
    scopes.sort_unstable();
    scopes.dedup();
    Json(serde_json::json!({
        "compiled": cfg!(feature = "mcp"),
        "enabled": cfg!(feature = "mcp") && mcp.is_some_and(|m| m.enabled),
        "path": mcp.map(|m| m.path.clone()).unwrap_or_else(|| "/mcp".to_string()),
        "token_count": mcp.map(|m| m.tokens.len()).unwrap_or(0),
        "scopes": scopes,
    }))
}

async fn list_prompts() -> impl IntoResponse {
    let prompts: Vec<_> = prompts::prompt_defs()
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "description": p.description,
                "arguments": p.args.iter().map(|a| serde_json::json!({
                    "name": a.name, "description": a.description, "required": a.required
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(serde_json::json!({ "prompts": prompts }))
}

async fn render_prompt(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Query(args): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    match prompts::render(&state, &name, &args).await {
        Ok(r) => Json(serde_json::json!({ "name": name, "description": r.description, "text": r.text })).into_response(),
        Err(e) => {
            let status = match e.code {
                "unknown_prompt" | "not_found" => StatusCode::NOT_FOUND,
                "invalid_input" | "debug_disabled" | "sandbox_disabled" => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            let body = if e.code == "unknown_prompt" || e.code == "not_found" {
                serde_json::json!({"error": "not_found"})
            } else {
                let mut v = serde_json::json!({"error": e.code, "message": e.message});
                if let Some(h) = e.hint { v["hint"] = serde_json::Value::String(h); }
                v
            };
            (status, Json(body)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::{state, ECHO_GATEWAY};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app(state: Arc<SharedState>) -> Router {
        router().with_state(state)
    }

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app.oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn status_reports_config_without_tokens() {
        let s = state(
            "admin:\n  username: u\n  password: p\n  mcp:\n    enabled: true\n    path: /agent\n    tokens:\n      - {token: rrrrrrrrrrrrrrrrrrrr, scope: read}\n      - {token: wwwwwwwwwwwwwwwwwwww, scope: write}\n      - {token: qqqqqqqqqqqqqqqqqqqq, scope: read}\n",
            ECHO_GATEWAY,
        );
        let (st, v) = get_json(app(s), "/api/mcp/status").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["compiled"], cfg!(feature = "mcp"));
        assert_eq!(v["enabled"], cfg!(feature = "mcp"));
        assert_eq!(v["path"], "/agent");
        assert_eq!(v["token_count"], 3);
        assert_eq!(v["scopes"], serde_json::json!(["read", "write"]));
        assert!(!v.to_string().contains("rrrrrrrr"));

        let (_, v) = get_json(app(state("{}", "{}")), "/api/mcp/status").await;
        assert_eq!(v["enabled"], false);
        assert_eq!(v["path"], "/mcp");
        assert_eq!(v["token_count"], 0);
    }

    #[tokio::test]
    async fn prompts_list_and_render() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        let (st, v) = get_json(app(s.clone()), "/api/mcp/prompts").await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["prompts"].as_array().unwrap().iter().any(|p| p["name"] == "why_this_port" && p["arguments"][1]["name"] == "node_id"));

        let (st, v) = get_json(app(s.clone()), "/api/mcp/prompts/review_policy?policy_name=echo-policy").await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["text"].as_str().unwrap().contains("# Review policy `echo-policy`"));
        assert_eq!(v["name"], "review_policy");

        let (st, v) = get_json(app(s.clone()), "/api/mcp/prompts/review_policy?policy_name=nope").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], "not_found");
        let (st, _) = get_json(app(s.clone()), "/api/mcp/prompts/nope").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, v) = get_json(app(s), "/api/mcp/prompts/explain_trace").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "invalid_input");

        let off = state("{}", ECHO_GATEWAY);
        let (st, v) = get_json(app(off), "/api/mcp/prompts/explain_trace?trace_id=x").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "debug_disabled");
    }
}
```

In `src/admin/mod.rs`: add `mod mcp;` (alphabetical) and `.merge(mcp::router())` to the `api` chain from Task 9.

- [ ] **Step 2: Run and commit**

Run: `cargo test --lib admin::mcp && cargo test --lib admin::tests`
Expected: pass.

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
git add src/admin/mcp.rs src/admin/mod.rs
git commit -m "feat(admin): /api/mcp/status and rendered prompt endpoints for the web UI"
```

---

### Task 11: CI matrix, example config, documentation

**Files:**
- Modify: `.github/workflows/ci.yml` (headless-check job), `config/system.yaml`, `website/docs/guides/mcp.md` (new), `website/sidebars.ts:30-39`, `website/docs/reference/roadmap.md`, `CLAUDE.md`, `docs/apisix-parity.md:53-57`

- [ ] **Step 1: CI — "everything but MCP" compiles**

In `.github/workflows/ci.yml`, in the `headless-check` job after the existing `cargo check --no-default-features --locked` step, add:

```yaml
      - name: check (ui + redis-store, no mcp)
        run: cargo check --no-default-features --features ui,redis-store --locked
```

(This job already builds `ui/dist`? — it does **not**; `ui` needs `ui/dist` for rust-embed. If the job has no UI build step, use `--features redis-store` only and rely on the `rust` job for `ui`: `cargo check --no-default-features --features redis-store --locked` is then redundant with `redis-live`; in that case add the step to the `rust` job instead, after `cargo test --locked`, as `cargo check --no-default-features --features ui,redis-store --locked`.)

- [ ] **Step 2: Example config**

Append to `config/system.yaml`'s `admin:` section (commented):

```yaml
  # Model Context Protocol server for AI agents (Claude Code, Cursor, …).
  # Off by default. Tokens are scoped: `read` = inspect config, traces and the
  # sandbox; `write` = also create/replace/delete routes, policies, supernodes,
  # plugin configs and stores. Generate tokens with `openssl rand -base64 32`.
  # mcp:
  #   enabled: ${FEATHERBIT_MCP_ENABLED:-false}
  #   path: /mcp
  #   tokens:
  #     - token: ${FEATHERBIT_MCP_READ_TOKEN}
  #       scope: read
  #       name: local-agent
  #     - token: ${FEATHERBIT_MCP_WRITE_TOKEN}
  #       scope: write
  #   allowed_origins: []
```

Verify `cargo run -- --system-config config/system.yaml` still parses (comments only).

- [ ] **Step 3: Guide**

Create `website/docs/guides/mcp.md`:

```markdown
---
title: MCP server for agents
description: Let an AI agent (Claude Code, Cursor, …) inspect traces, explain policy behavior, and author routes, policies and supernodes through the Model Context Protocol.
---

The gateway can serve a [Model Context Protocol](https://modelcontextprotocol.io) endpoint on the Admin listener. The gateway is the MCP *server*; the model lives in whatever agent you already run, so nothing here calls a third-party LLM, stores a provider key, or costs tokens.

## Enabling

```yaml
admin:
  mcp:
    enabled: ${FEATHERBIT_MCP_ENABLED:-false}
    path: /mcp
    tokens:
      - token: ${FEATHERBIT_MCP_READ_TOKEN}
        scope: read
        name: local-agent
      - token: ${FEATHERBIT_MCP_WRITE_TOKEN}
        scope: write
    allowed_origins: []
```

Restart-gated like everything in `system.yaml`. Rules enforced at load: `enabled: true` needs at least one token; tokens are at least 16 characters (use 32+ random bytes: `openssl rand -base64 32`); duplicates are rejected; `path` must be absolute and outside `/api`. Disabled, the path answers `404` and logs the key to set.

### Scopes

| Scope | Unlocks |
|---|---|
| `read` | `list_node_types`, `get_node_type`, `list_vars`, `get_status`, `export_config`, `list_/get_` for routes, policies, supernodes, plugin configs, stores, consumers (credentials masked), `validate_policy`, `validate_supernode`, `list_traces`, `get_trace`, `get_trace_step`, `run_sandbox` |
| `write` | everything above plus `put_/delete_` for routes, policies, supernodes, plugin configs, stores, and `reload_config`. Every `put_`/`delete_` accepts `dry_run: true`. |

Read tokens never see write tools in `tools/list`; a write call with a read token returns a `forbidden` tool error. Use a read token against production and a write token only where an agent should be allowed to change config.

## Connecting a client

Claude Code:

```bash
claude mcp add --transport http featherbit http://localhost:9090/mcp \
  --header "Authorization: Bearer $FEATHERBIT_MCP_READ_TOKEN"
```

Generic `mcpServers` JSON (Claude Desktop, Cursor, Windsurf):

```json
{ "mcpServers": { "featherbit": { "type": "http", "url": "http://localhost:9090/mcp",
    "headers": { "Authorization": "Bearer <TOKEN>" } } } }
```

Smoke test:

```bash
curl -s -X POST http://localhost:9090/mcp -H "Authorization: Bearer $TOKEN" \
  -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

The web UI's **Agent** panel (footer) shows these snippets with your actual endpoint.

## Tools, resources, prompts

Tools return JSON; failures come back as tool errors `{code, message, errors?, hint?}` with codes `not_found`, `invalid_input`, `invalid_config` (with the compiler's error list), `debug_disabled`, `sandbox_disabled`, `forbidden`, `store_error`, `internal`.

Resources: every plugin/concept/reference documentation page is embedded in the binary (`featherbit://docs/plugins/{type}`, `featherbit://docs/concepts/{name}`, `featherbit://docs/reference/{name}`) — `get_node_type` returns the page too — plus `featherbit://routes/{name}`, `featherbit://policies/{name}`, `featherbit://supernodes/{name}` (YAML) and `featherbit://traces/{id}`.

Prompts (the precompiled questions): `explain_trace`, `why_this_port`, `why_this_response`, `review_policy`, `design_policy`, `design_supernode`, `design_route`, `diagnose_route`. The same texts are available from the UI's trace viewer and policy editor as "Copy as agent prompt", with the data inlined so they work in any chat.

Trace and sandbox tools need [debug mode](./debugging.md); they say so when it is off.

## Security notes

- Two credentials, two surfaces: Basic Auth never works on the MCP path; MCP tokens never work on `/api/*`.
- Constant-time token comparison; disabled MCP is indistinguishable from a missing route.
- A request carrying an `Origin` header is refused unless listed in `allowed_origins` (DNS-rebinding defence). Non-browser agents send none.
- Writes go through the same validate → compile → commit path as the Admin API and are logged (`mcp tool call token=… scope=… tool=… outcome=…`). With the file config source, edits are live but not written back to `gateway.yaml` — the same as Admin API edits; with etcd they persist cluster-wide.
- `${ENV}` placeholders are served raw, never resolved. Consumer credentials are masked on read.
- Put TLS on the admin listener (`admin.tls`) when the agent is remote.
```

In `website/sidebars.ts` add `'guides/mcp',` after `'guides/debugging',`.

- [ ] **Step 4: Roadmap, CLAUDE.md, parity note**

`website/docs/reference/roadmap.md` — add a row after "Debug mode & plugin sandbox":

```markdown
| MCP server for agents | **Implemented** — a Model Context Protocol (Streamable HTTP) endpoint on the admin listener (`admin.mcp`, off by default) behind scoped `read`/`write` bearer tokens: tools over routes/policies/supernodes/plugin-configs/stores (writes with `dry_run`), traces and the sandbox, embedded docs pages as resources, and precompiled debugging/authoring prompts; the web UI gains an Agent panel and "copy as agent prompt" actions (see [MCP server for agents](../guides/mcp.md)). Follow-ups: stdio transport, consumer writes, `listChanged` notifications, an optional bring-your-own-LLM chat panel. |
```

`CLAUDE.md` — add a bullet to "Core features" after the templates bullet:

```markdown
- **MCP server for agents** — `admin.mcp` (off by default; `mcp` cargo feature, default-on) mounts an `rmcp` Streamable HTTP endpoint at `admin.mcp.path` on the admin listener, outside Basic Auth, behind scoped bearer tokens (`read`/`write`, constant-time compare, `Origin` allow-list). `src/mcp/`: `tools/` (typed tools over `SharedState`; writes via `commit_candidate` → `validate_gateway`/`ConfigStore::commit`), `docs.rs` (rust-embed'd `website/docs` pages as `featherbit://docs/...` resources), `prompts.rs` (precompiled prompts, also served by `GET /api/mcp/prompts/{name}` for the UI's "copy as agent prompt"), `server.rs` (the `rmcp` adapter, feature-gated). Debug/sandbox tools reuse `debug::render`, `debug::sandbox::run_sandbox`, `graph::prepare_policy`.
```

`docs/apisix-parity.md` — in the "AI / LLM suite — future epic" section append a sentence: "Unrelated to this suite, the gateway now ships a **control-plane** MCP server for agents operating the gateway itself (`admin.mcp`, see `website/docs/guides/mcp.md`); `mcp-bridge` here is the data-plane plugin exposing an upstream MCP server to clients and remains deferred."

- [ ] **Step 5: Website build, full suite, graph update, commit**

Run:
```bash
cd website && npm run build && cd ..
cargo test --locked && cargo check --no-default-features --locked && cargo clippy --all-targets --locked -- -D warnings && cargo fmt --check
graphify update .
```
Expected: docs build passes (broken-link check included), all green.

```bash
git add .github/workflows/ci.yml config/system.yaml website/docs/guides/mcp.md website/sidebars.ts website/docs/reference/roadmap.md CLAUDE.md docs/apisix-parity.md graphify-out
git commit -m "docs(mcp): guide, roadmap and CLAUDE.md entries; CI checks the build without the mcp feature"
```

Then report to Francesco: the backend is complete on `feature/mcp-server`; the UI companion plan can start (same branch or a second branch off it — their call), and the PR to `develop` waits for their go-ahead.

