# Named Output Ports Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the binary success/error port model with declared, named output ports so plugins route alternate outcomes (deny, redirect, throttle, preflight) explicitly instead of abusing the error path or being silently overwritten by upstream.

**Architecture:** A static `PortSpec` registry (one source of truth in the plugin factory) declares each node type's ports. `PluginOutput` gains a `port` field selecting the exit port on the `Ok` path; `Err` keeps meaning genuine failure with today's fallback chain. The engine compiles edges into a per-node port map and validates at compile time that every declared non-error port is wired. Supernode expansion, debug traces, the admin catalog, and the UI editor all consume the same spec.

**Tech Stack:** Rust (tokio, axum, async-trait), React + @xyflow/react (UI), Playwright (e2e), Docusaurus (docs).

**Spec:** `docs/superpowers/specs/2026-08-07-named-output-ports-design.md` — read it before starting any task.

## Global Constraints

- `Err`/error port is reserved for "the node could not do its job" (config/parse/infrastructure failures). "The node did its job and the result is an alternate route" is an `outcome` port. Apply this criterion verbatim in every audit decision.
- Standard port vocabulary — plugins must use these names, never invent synonyms: `denied`, `redirect`, `limited`, `broken`, `preflight`, `abort`.
- Reserved port names: `in`, `out`, `success`, `error`. `out` is an accepted YAML alias for `success`, normalized at parse time.
- Every declared output port of kind `success` or `outcome` must be wired in every policy; `error` keeps its optional fallback chain (per-node edge → policy catch-all → default 500). Unwired mandatory port = policy fails to compile.
- Clean break: no fallback routing for unwired named ports. Update every fixture/example config in the same task that adds ports to a plugin. Fixture files that reference swept plugins: `e2e/fixtures/gateway.yaml`, `tests/config/gateway.yaml`, `tests/oidc-test.yml`, `website/screenshots/gateway.yaml`, plus YAML snippets in `website/docs/reference/plugins/*.md`.
- Conventional Commits, no Co-Authored-By trailer. Branch: `feature/named-output-ports` (already created off `develop`).
- After code changes, run `graphify update .` (cheap, AST-only) before committing.
- Full gate for every task: `cargo test` green. Tasks touching e2e additionally run the Playwright suite (`cargo build --release && cd e2e && npm test`).
- Lua `script` nodes keep `success` + `error` (non-goal). Fan-out (two edges from one port) stays unsupported — now an explicit compile error.

---

### Task 1: Port declaration types and the `port_spec` registry

**Files:**
- Create: `src/plugins/ports.rs`
- Modify: `src/plugins/mod.rs` (add `pub mod ports;`, add `port_spec()` next to `create_plugin`)
- Test: inline `#[cfg(test)]` in `src/plugins/ports.rs` and `src/plugins/mod.rs`

**Interfaces:**
- Consumes: `KNOWN_PLUGIN_TYPES` (`src/plugins/mod.rs:79`).
- Produces (later tasks rely on these exact items):
  - `plugins::ports::{PortKind, PortDecl, PortSpec, DEFAULT_SPEC, LISTENER_SPEC, CLIENT_SPEC, RESERVED_PORT_NAMES}`
  - `plugins::port_spec(plugin_type: &str) -> Option<&'static PortSpec>`

- [ ] **Step 1: Write the failing test** (bottom of the new `src/plugins/ports.rs`)

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_every_known_type_has_a_valid_spec test_structural_specs`
Expected: compile FAILURE — `ports` module and `port_spec` do not exist.

- [ ] **Step 3: Implement `src/plugins/ports.rs`**

```rust
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
```

- [ ] **Step 4: Add the registry to `src/plugins/mod.rs`**

Add `pub mod ports;` to the module list, then next to `create_plugin`:

```rust
use ports::{PortDecl, PortKind, PortSpec};

/// Static port declaration for a node type. `None` for unknown types.
///
/// This match is the port registry: sweep tasks add arms here as plugins
/// gain outcome ports. Keep in sync with `KNOWN_PLUGIN_TYPES`
/// (enforced by `test_every_known_type_has_a_valid_spec`).
pub fn port_spec(plugin_type: &str) -> Option<&'static PortSpec> {
    match plugin_type {
        "listener" => Some(&ports::LISTENER_SPEC),
        "client" => Some(&ports::CLIENT_SPEC),
        _ if KNOWN_PLUGIN_TYPES.contains(&plugin_type) => Some(&ports::DEFAULT_SPEC),
        _ => None,
    }
}
```

(Outcome-port arms are added in Tasks 6–10; at this point every non-structural type resolves to `DEFAULT_SPEC`.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test test_every_known_type_has_a_valid_spec test_structural_specs`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/plugins/ports.rs src/plugins/mod.rs
git commit -m "feat(plugins): static PortSpec registry with port_spec() lookup"
```

---

### Task 2: New plugin contract — `PluginOutput.port`, drop dead named-I/O plumbing

Purely mechanical, behavior-preserving sweep. No routing changes yet.

**Files:**
- Modify: `src/plugins/mod.rs` (PluginOutput, Plugin trait)
- Modify: every file under `src/plugins/native/`, `src/plugins/script/mod.rs`, `src/graph/engine.rs:115-119` (the `execute` call site), and any other `impl Plugin` (search `impl Plugin for`)
- Test: existing suite (no new tests — the compiler is the test)

**Interfaces:**
- Consumes: `ports::DEFAULT_SPEC`, `port_spec()` from Task 1.
- Produces (all later tasks rely on these):
  - `PluginOutput { context: Context, port: Option<&'static str> }`
  - `PluginOutput::success(context) -> Self` (port: None)
  - `PluginOutput::on_port(context, port: &'static str) -> Self`
  - `Plugin::execute(&self, ctx: Context) -> PluginResult` (no `named_inputs`)
  - `Plugin::ports(&self) -> &'static PortSpec` (default impl, never overridden)

- [ ] **Step 1: Rewrite the contract in `src/plugins/mod.rs`**

```rust
/// The result of a successful plugin execution.
#[derive(Debug)]
pub struct PluginOutput {
    /// The (possibly mutated) context, passed on to the next node in the graph.
    pub context: Context,
    /// Declared output port this result leaves on. `None` = `success`.
    /// Must name a port of kind `outcome` in the node type's [`PortSpec`].
    pub port: Option<&'static str>,
}

impl PluginOutput {
    /// The normal exit: continue through the `success` port.
    pub fn success(context: Context) -> Self {
        Self { context, port: None }
    }

    /// Exit through a declared named `outcome` port (e.g. `"denied"`).
    pub fn on_port(context: Context, port: &'static str) -> Self {
        Self { context, port: Some(port) }
    }
}
```

In the `Plugin` trait: remove the `named_inputs` parameter from `execute` (and its doc bullet), and add:

```rust
    /// Static declaration of this node type's ports. Never overridden —
    /// resolved through the factory registry so the engine, catalog, and
    /// plugins can't drift.
    fn ports(&self) -> &'static PortSpec {
        port_spec(self.plugin_type()).unwrap_or(&ports::DEFAULT_SPEC)
    }
```

- [ ] **Step 2: Mechanically migrate all plugin files**

From the repo root (Git Bash / Bash tool):

```bash
# struct-literal successes -> constructor (two frequent shapes)
grep -rl "named_outputs: HashMap::new()" src/ | xargs sed -i \
  -e 's/Ok(PluginOutput {\s*context: ctx,\s*named_outputs: HashMap::new(),\s*})/Ok(PluginOutput::success(ctx))/g'
# execute signatures
grep -rl "_named_inputs" src/plugins | xargs sed -i \
  -e '/_named_inputs: &HashMap<String, serde_json::Value>,/d' \
  -e 's/named_inputs: &HashMap<String, serde_json::Value>,//'
```

Then iterate `cargo check` and fix by hand what the regexes missed: multi-line
struct literals with other variable names (`context: out_ctx`, …), test
call-sites passing `&HashMap::new()` to `execute`, and unused `HashMap`
imports. Every `Ok(PluginOutput { ... })` becomes `Ok(PluginOutput::success(<ctx>))`;
every test call `plugin.execute(ctx, &HashMap::new())` becomes `plugin.execute(ctx)`.
Update the engine call site (`src/graph/engine.rs:115-119`): delete the
`named_inputs` local and call `node.execute(ctx).await`. The returned `port`
field is **ignored by the engine in this task** (routing lands in Task 3).

- [ ] **Step 3: Run the full suite**

Run: `cargo test`
Expected: PASS — identical behavior, no test semantics changed. If a test fails, the migration changed behavior somewhere: inspect the diff for that plugin; nothing in this task may alter logic.

- [ ] **Step 4: Commit**

```bash
git add -A src/
git commit -m "refactor(plugins): PluginOutput carries an exit port; drop dead named_inputs/named_outputs plumbing"
```

---

### Task 3: Engine — per-port edge map, compile validation, port-aware traces

**Files:**
- Modify: `src/graph/engine.rs` (CompiledGraph struct, `run`, `compile_policy`, tests)
- Modify: `src/debug/trace.rs` (`NodeStep`, `TraceRecorder::record_step`, `EdgeKind`)
- Modify: `src/debug/sandbox.rs` (`synthesize_policy` must wire every mandatory port)
- Test: inline tests in `engine.rs`, `trace.rs`, `sandbox.rs`

**Interfaces:**
- Consumes: `PluginOutput.port`, `port_spec()`, `PortKind`.
- Produces:
  - `CompiledGraph.edges: HashMap<String, HashMap<String, String>>` (node → port → target; private)
  - `EdgeKind::Outcome` (new unit variant; `Copy` preserved)
  - `NodeStep.port: Option<String>` (serialized only when set)
  - `TraceRecorder::record_step(&mut self, node_id, node_type, outcome, elapsed, edge, port: Option<&str>, next, ctx)` — note the new `port` parameter inserted before `next`
  - Compile-error message formats (asserted by admin/e2e tests later):
    - `policy '<p>': node '<n>' (type '<t>') has no output port '<port>'`
    - `policy '<p>': output port '<port>' of node '<n>' (type '<t>') must be wired — add an edge from '<n>.<port>'`
    - `policy '<p>': duplicate edge from '<n>.<port>' — fan-out is not supported`
    - `policy '<p>': edge references unknown node '<n>'`

- [ ] **Step 1: Write the failing engine tests** (in `engine.rs` `mod tests`, alongside the existing helpers — reuse the test plugin/policy builders already present there)

```rust
    /// A named outcome port routes to its wired target, not to success.
    #[tokio::test]
    async fn test_outcome_port_routes_to_its_edge() {
        // Register a test plugin type that emits on_port("denied") when the
        // request has header x-deny, else success. Follow the pattern of the
        // existing test plugins in this module; declare its spec by adding a
        // match arm in port_spec is NOT possible for test-only types, so use
        // the two real types that will exist by Task 8 — for THIS task,
        // exercise routing through a stub: build a CompiledGraph directly
        // (the struct is private to the module, tests here construct it
        // as the existing tests at engine.rs:600+ already do) with
        // edges = {("n", "denied") -> "deny-client", ("n", "success") -> "client"}.
        // Execute with the stub returning on_port("denied") and assert the
        // walk ends at deny-client's effect on the context.
    }

    /// Unwired mandatory port fails compilation with the exact message.
    #[test]
    fn test_compile_rejects_unwired_mandatory_port() {
        // Policy: listener -> cors -> client, NO cors.preflight edge.
        // (Runs after Task 6 lands cors's spec; in THIS task use a policy
        // where a node's *success* edge is missing instead:)
        // nodes: listener, proxy-rewrite "rw", client; edges: listener.out->rw.in only.
        let err = compile_policy(&policy, resources()).unwrap_err();
        assert_eq!(
            err,
            "policy 'p': output port 'success' of node 'rw' (type 'proxy-rewrite') must be wired — add an edge from 'rw.success'"
        );
    }

    /// Duplicate (node, port) edge is an explicit error, not a silent overwrite.
    #[test]
    fn test_compile_rejects_fanout() { /* two edges from rw.success; assert message */ }

    /// Edge from a port the type does not declare.
    #[test]
    fn test_compile_rejects_undeclared_port() {
        // edge "rw.banana" -> client.in
        // assert: "policy 'p': node 'rw' (type 'proxy-rewrite') has no output port 'banana'"
    }

    /// error port stays optional: policy with no error edges still compiles.
    #[test]
    fn test_error_port_wiring_is_optional() { /* listener->rw->client, no error edges: Ok */ }
```

Write these as real code against the existing test helpers in `engine.rs` (there are policy/`CompiledGraph` builders around line 600 — mirror them; the stub plugin for the routing test implements `Plugin` inline returning `PluginOutput::on_port(ctx, "denied")`).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib graph::engine`
Expected: FAIL/compile error (no `edges` map, no validation).

- [ ] **Step 3: Implement the engine rework**

In `CompiledGraph`, replace `success_edges`/`error_edges` with:

```rust
    /// node_id -> (port -> target node_id). Ports normalized (`out` -> `success`).
    edges: HashMap<String, HashMap<String, String>>,
```

In `run`'s `Ok` branch (replacing engine.rs:130-163):

```rust
                Ok(output) => {
                    ctx = output.context;
                    let port = output.port.unwrap_or("success");

                    let terminal = self.terminal_node_ids.contains(&current_node_id);
                    let next = if terminal {
                        None
                    } else {
                        self.edges.get(&current_node_id).and_then(|m| m.get(port))
                    };
                    if let Some(r) = recorder.as_mut() {
                        let edge = if terminal {
                            EdgeKind::Terminal
                        } else if next.is_none() {
                            EdgeKind::EndOfChain // defensive: validation makes this unreachable
                        } else if port == "success" {
                            EdgeKind::Success
                        } else {
                            EdgeKind::Outcome
                        };
                        r.record_step(
                            &current_node_id,
                            &node_type,
                            StepOutcome::Success,
                            elapsed,
                            edge,
                            (port != "success").then_some(port),
                            next.map(String::as_str),
                            &ctx,
                        );
                    }
                    match next {
                        Some(next_id) => current_node_id = next_id.clone(),
                        None => break,
                    }
                }
```

The `Err` branch: replace `self.error_edges.get(&current_node_id)` with `self.edges.get(&current_node_id).and_then(|m| m.get("error"))` and pass `None` for the new `port` argument of `record_step` (as do the NodeNotFound and unhandled paths).

In `compile_policy`, replace the edge-parsing block (engine.rs:295-311) with:

```rust
    let node_types: HashMap<String, String> = policy
        .nodes
        .iter()
        .map(|n| (n.id.clone(), n.node_type.clone()))
        .collect();

    let mut edges: HashMap<String, HashMap<String, String>> = HashMap::new();
    for edge in &policy.edges {
        let (from_node, from_port) = parse_edge_endpoint(&edge.from)?;
        let (to_node, _to_port) = parse_edge_endpoint(&edge.to)?;
        let from_port = if from_port == "out" { "success".to_string() } else { from_port };

        let node_type = node_types.get(&from_node).ok_or_else(|| {
            format!("policy '{}': edge references unknown node '{}'", policy.name, from_node)
        })?;
        let spec = plugins::port_spec(node_type)
            .ok_or_else(|| format!("Unknown plugin type: {}", node_type))?;
        if !spec.outputs.iter().any(|p| p.name == from_port) {
            return Err(format!(
                "policy '{}': node '{}' (type '{}') has no output port '{}'",
                policy.name, from_node, node_type, from_port
            ));
        }
        if edges
            .entry(from_node.clone())
            .or_default()
            .insert(from_port.clone(), to_node)
            .is_some()
        {
            return Err(format!(
                "policy '{}': duplicate edge from '{}.{}' — fan-out is not supported",
                policy.name, from_node, from_port
            ));
        }
    }

    // Mandatory wiring: every success/outcome port of every node must have an edge.
    for node_config in &policy.nodes {
        let spec = plugins::port_spec(&node_config.node_type)
            .ok_or_else(|| format!("Unknown plugin type: {}", node_config.node_type))?;
        for p in spec.outputs {
            if matches!(p.kind, plugins::ports::PortKind::Error) {
                continue;
            }
            let wired = edges
                .get(&node_config.id)
                .is_some_and(|m| m.contains_key(p.name));
            if !wired {
                return Err(format!(
                    "policy '{}': output port '{}' of node '{}' (type '{}') must be wired — add an edge from '{}.{}'",
                    policy.name, p.name, node_config.id, node_config.node_type, node_config.id, p.name
                ));
            }
        }
    }
```

`entry_node_id` becomes `edges.get(&listener_node_id).and_then(|m| m.get("success")).cloned().unwrap_or_else(|| listener_node_id.clone())`.

In `src/debug/trace.rs`: add `Outcome` to `EdgeKind` (keep existing derives — it stays a unit variant so `Copy` survives) with doc `/// Followed a named outcome port (see NodeStep::port).`; add to `NodeStep`:

```rust
    /// Set when the step left on a named outcome port (e.g. "denied").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
```

and thread the new `port: Option<&str>` parameter through `record_step` into the struct (`port: port.map(String::from)`), updating every existing `record_step` call site with `None`.

In `src/debug/sandbox.rs` `synthesize_policy`: when wiring the node under test, wire **every** declared non-error port to the client, so sandboxed plugins with outcome ports still compile:

```rust
    let spec = crate::plugins::port_spec(&node_type).unwrap_or(&crate::plugins::ports::DEFAULT_SPEC);
    for p in spec.outputs {
        if !matches!(p.kind, crate::plugins::ports::PortKind::Error) {
            edges.push(EdgeConfig {
                from: format!("{node_id}.{}", p.name),
                to: "client.in".to_string(),
            });
        }
    }
```

(match the actual local variable names in `synthesize_policy`; the error-edge handling driven by `on_error` stays as-is).

- [ ] **Step 4: Fix every fixture and test the new validation breaks**

Run `cargo test` and repair fixtures — this is the deliberate clean break. Expected repairs: engine/expand/sandbox test policies with dangling `success` edges (add `<node>.success -> client.in`), and `tests/config/gateway.yaml` / `tests/oidc-test.yml` if any node lacks a success edge. Do NOT weaken the validation to make a fixture pass; fix the fixture.

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: PASS, including the five new tests from Step 1.

- [ ] **Step 6: Commit**

```bash
git add -A src/ tests/
git commit -m "feat(engine): per-port edge routing with mandatory-wiring compile validation and port-aware traces"
```

---

### Task 4: Supernode expansion — port passthrough and kind-based boundaries

**Files:**
- Modify: `src/graph/expand.rs`
- Test: inline tests in `expand.rs`

**Interfaces:**
- Consumes: `port_spec()`, `PortKind`.
- Produces: expansion that preserves named ports through prefixing; the existing black-box error-boundary guarantee; validation happens post-expansion in `compile_policy` (Task 3) — expansion itself only rewires.

- [ ] **Step 1: Write the failing tests** (follow the existing `edge()`/`edge_set()` helpers in `expand.rs` tests)

```rust
    /// A named outcome port on an inner node survives expansion with the
    /// instance prefix, targeting the node the definition wired it to.
    #[test]
    fn test_inner_outcome_port_is_prefixed_and_preserved() {
        // definition: input -> auth -> output; auth.denied -> output
        // (use port name "denied" on a node type that declares it — until
        // Task 8 lands, expansion must not care whether the port is declared;
        // it rewires syntactically. Use any custom port name.)
        // outer: listener -> inst.in, inst.success -> client, inst.error -> eh
        // expect edge "sec/auth.denied" -> whatever output maps to (client)
    }

    /// An inner outcome port wired to the `output` boundary follows the outer
    /// success edge, same as inner success ports do today.
    #[test]
    fn test_inner_outcome_port_to_output_boundary() { /* denied -> output; expect -> client.in */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib graph::expand`
Expected: FAIL — today `expand.rs:146` matches only `success|out|error` on inner-edge source ports and errors on anything else (`unknown port '...' on supernode node`).

- [ ] **Step 3: Implement**

In the inner-edge handling (`expand.rs` around lines 144-151 and 290-320): treat any port that is neither `error` nor a boundary special-case exactly like `success` for *rewiring purposes* — i.e. keep the port name in the prefixed `from` endpoint (`format!("{}.{}", prefix(from_node), from_port)` — the code at line 295 already does this) and route to-the-boundary targets the same way inner success edges route (`output` boundary → outer success target). Remove the `unknown port` rejection for custom names; expansion is syntactic and Task 3's post-expansion validation catches genuinely undeclared ports. The unwired-inner-error black-box rule (lines 325-345) is unchanged.

- [ ] **Step 4: Run the full suite**

Run: `cargo test`
Expected: PASS, including both new tests and all pre-existing expand tests.

- [ ] **Step 5: Commit**

```bash
git add src/graph/expand.rs
git commit -m "feat(supernodes): pass named outcome ports through expansion"
```

---

### Task 5: Plugin audit ledger

**Files:**
- Create: `docs/superpowers/specs/2026-08-07-port-audit.md`

**Interfaces:**
- Consumes: the criterion from Global Constraints; the spec's known-adopters table.
- Produces: one row per entry in `KNOWN_PLUGIN_TYPES` with columns `type | verdict (ports to add, or "default") | outcomes moved off Err | evidence (file:line)`. Tasks 6–10 treat this ledger as their authoritative worklist; if the ledger and this plan's task lists disagree, the ledger wins and the executor notes the delta in the task's commit message.

- [ ] **Step 1: Enumerate and audit**

For every type in `KNOWN_PLUGIN_TYPES` (`src/plugins/mod.rs:79`), open its plugin file and record a verdict. Fast triage: `grep -n "status_code = \|Err(PluginExecutionError" src/plugins/native/<file>.rs`. Apply the criterion. Expected shape (from the spec's audit, verify each): auth/restriction family → `denied`; interactive SSO (`openid-connect`, `cas-auth`, `authz-casdoor`) → additionally `redirect`; `rate-limit`, `limit-conn`, `limit-count` → `limited`; `api-breaker` → `broken`; `cors` → `preflight`; `redirect` → `redirect`; `fault-injection` → `abort`; loggers/tracers/transformers/proxy/upstream/serverless → default (their failures are genuine errors). Pay attention to plugins the spec flagged as unaudited: `traffic-split`, `workflow` (respond action → `denied`), `opentelemetry` (its 503 is an overload/infra failure → stays `error` unless reading the code says otherwise), `forward-auth`, `opa`, `wolf-rbac`, `error-page`, `echo`, mocking-style plugins (a mock response is that plugin's *success*, not an outcome).

- [ ] **Step 2: Sanity-check the ledger**

Every row has a verdict and evidence; no port name outside the standard vocabulary; every `outcome` verdict cites the deliberate response write (file:line).

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-08-07-port-audit.md
git commit -m "docs(specs): per-plugin port audit ledger"
```

---

### Task 6: cors → `preflight` port (closes the roadmap bug)

**Files:**
- Modify: `src/plugins/native/cors.rs`, `src/plugins/mod.rs` (registry arm)
- Modify: `e2e/fixtures/gateway.yaml` (cors nodes at ~line 138 and ~line 747: add `preflight` edges)
- Modify: `e2e/tests/data-plane.spec.ts` (E2E-DP-09 at line 119: `test.fail` → `test`)
- Test: `cors.rs` inline tests; e2e suite

**Interfaces:**
- Consumes: `PluginOutput::on_port`, registry from Task 1.
- Produces: `port_spec("cors")` = success/preflight/error. Pattern for every sweep task that follows: **(a)** add a `PortSpec` const + registry arm, **(b)** switch the outcome branch to `on_port`, **(c)** update plugin tests to assert the port, **(d)** wire fixtures, **(e)** full suite green.

- [ ] **Step 1: Update the plugin test** (in `cors.rs`, replace `test_preflight_prepares_204`)

```rust
    /// APISIX TEST 14: an OPTIONS preflight on an allowed origin exits on the
    /// `preflight` port with the 204 fully prepared — the engine routes it
    /// away from upstream (E2E-DP-09 covers the end-to-end short-circuit).
    #[tokio::test]
    async fn test_preflight_exits_on_preflight_port() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"],
            "allowed_methods": ["GET", "POST"],
            "max_age": 50
        }))
        .execute(ctx("OPTIONS", Some("http://sub.domain.com")))
        .await
        .unwrap();
        assert_eq!(out.port, Some("preflight"));
        assert_eq!(out.context.response.status_code, 204);
        assert_eq!(hdr(&out.context, "access-control-allow-methods"), Some("GET, POST"));
        assert_eq!(hdr(&out.context, "access-control-max-age"), Some("50"));
        assert!(out.context.response.body.is_empty());
    }

    /// Non-preflight requests and disallowed origins stay on success.
    #[tokio::test]
    async fn test_non_preflight_stays_on_success() {
        let out = plugin(serde_json::json!({}))
            .execute(ctx("GET", Some("http://x.example")))
            .await
            .unwrap();
        assert_eq!(out.port, None);
    }
```

Also update `test_preflight_disallowed_origin_untouched` to assert `out.port == None`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib cors`
Expected: FAIL — `out.port` is `None` on preflight.

- [ ] **Step 3: Implement**

In `cors.rs` `execute`, the preflight branch (cors.rs:178-204) returns early:

```rust
            if is_preflight {
                // ... existing header/method/max-age writes ...
                ctx.response.status_code = 204;
                ctx.response.body = Bytes::new();
                return Ok(PluginOutput::on_port(ctx, "preflight"));
            }
```

In `src/plugins/mod.rs` registry (`port_spec`), add before the catch-all arm:

```rust
        "cors" => Some(&ports::CORS_SPEC),
```

and in `src/plugins/ports.rs`:

```rust
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
```

Update the module doc (`cors.rs:1-5`) and struct doc to describe the port instead of the old "engine then follows success" caveat.

- [ ] **Step 4: Wire fixtures**

In `e2e/fixtures/gateway.yaml`, for each cors node (`type: cors` near lines 138 and 747) add to that policy's `edges`:

```yaml
      - from: <cors-node-id>.preflight
        to: <client-node-id>.in
```

(read the surrounding policy for the actual node ids). Same for `website/screenshots/gateway.yaml` (~line 29).

- [ ] **Step 5: Flip E2E-DP-09 and run everything**

In `e2e/tests/data-plane.spec.ts:119` change `test.fail(...)` to `test(...)` and delete the "Neither exists today" comment block (lines ~110-118), replacing it with a one-liner pointing at the `preflight` port. Run:

```bash
cargo test && cargo build --release && cd e2e && npm test
```

Expected: all green; DP-09 now passes for real (204 + `access-control-allow-origin` reach the client).

- [ ] **Step 6: Commit**

```bash
git add -A src/ e2e/ website/screenshots/
git commit -m "feat(cors): preflight exits on a dedicated port — OPTIONS short-circuits to client (fixes E2E-DP-09)"
```

---

### Task 7: `redirect` and `fault-injection` — fix the same latent overwrite

**Files:**
- Modify: `src/plugins/native/redirect.rs`, `src/plugins/native/fault_injection.rs`, `src/plugins/ports.rs`, `src/plugins/mod.rs` (registry arms)
- Modify: any fixture using these types (audit ledger + `grep -rn "type: redirect\|type: fault-injection" --include="*.yaml" --include="*.yml" --include="*.md" .`)
- Test: inline tests in both plugin files

**Interfaces:**
- Consumes: pattern from Task 6.
- Produces: `port_spec("redirect")` = success/redirect/error (success remains for requests the plugin passes through un-redirected, e.g. `http_to_https` on an already-https request); `port_spec("fault-injection")` = success/abort/error (success = no fault injected this request; delay-only faults also exit success after sleeping).

- [ ] **Step 1: Write failing tests**

In `redirect.rs` (mirror the existing test helpers at redirect.rs:200+): a test asserting a matching redirect returns `port == Some("redirect")` with the 3xx + `location` intact, and a test asserting a non-matching request returns `port == None`. In `fault_injection.rs`: abort-mode fault → `Some("abort")` with the configured status; delay-only / not-sampled → `None`.

```rust
    #[tokio::test]
    async fn test_redirect_exits_on_redirect_port() {
        let (result, _) = run_plugin(/* existing helper/config producing a 302 */);
        let out = result.unwrap();
        assert_eq!(out.port, Some("redirect"));
        assert_eq!(out.context.response.status_code, 302);
    }
```

(adapt to each file's existing helper names — both test modules already build plugins from JSON config and call `execute`).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib redirect fault_injection`
Expected: FAIL on the new port assertions.

- [ ] **Step 3: Implement**

In each plugin, at the point the deliberate response is fully written (redirect.rs:177 sets `ret_code`; fault_injection.rs:335 sets `abort.http_status`), return `Ok(PluginOutput::on_port(ctx, "redirect"))` / `Ok(PluginOutput::on_port(ctx, "abort"))`. Add to `ports.rs`:

```rust
/// `redirect`: emits prepared 3xx responses on their own port.
pub const REDIRECT_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl { name: "redirect", kind: PortKind::Outcome,
            description: "A 3xx redirect response is prepared; wire to client." },
        ERROR,
    ],
};

/// `fault-injection`: injected abort responses exit on their own port.
pub const FAULT_INJECTION_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl { name: "abort", kind: PortKind::Outcome,
            description: "An injected fault response is prepared; wire to client." },
        ERROR,
    ],
};
```

plus the two registry arms (`"redirect" => Some(&ports::REDIRECT_SPEC)`, `"fault-injection" => Some(&ports::FAULT_INJECTION_SPEC)`). Wire any fixtures found by the grep.

- [ ] **Step 4: Full suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A src/ e2e/ tests/ website/
git commit -m "feat(plugins): redirect and fault-injection exit on dedicated outcome ports"
```

---

### Task 8: Auth family → `denied` (+ `redirect` for cas-auth)

**Files:**
- Modify (per audit ledger; expected list): `key_auth.rs`, `basic_auth.rs`, `jwt_auth.rs`, `hmac_auth.rs`, `jwe_decrypt.rs`, `multi_auth.rs`, `ldap_auth.rs`, `dingtalk_auth.rs`, `feishu_auth.rs`, `cas_auth.rs`, plus `forward_auth.rs` / `opa.rs` / `wolf_rbac.rs` if the ledger says so — all under `src/plugins/native/`
- Modify: `src/plugins/ports.rs` (`AUTH_SPEC`, `INTERACTIVE_AUTH_SPEC`), `src/plugins/mod.rs` (registry arms)
- Modify fixtures: `e2e/fixtures/gateway.yaml` (key-auth ~186, basic-auth ~640/711), `website/screenshots/gateway.yaml` (~36)
- Test: each plugin's inline tests; e2e suite

**Interfaces:**
- Consumes: pattern from Task 6; audit ledger from Task 5.
- Produces: `AUTH_SPEC` (success/denied/error) shared by all credential-auth types; `INTERACTIVE_AUTH_SPEC` (success/denied/redirect/error) for `cas-auth`. **The `Err` path remains** in every auth plugin for infrastructure failures (consumer store unavailable, LDAP unreachable, IdP HTTP errors).

- [ ] **Step 1: Add the shared specs** (in `ports.rs`)

```rust
/// Credential-auth plugins: deliberate 401/403 rejections exit on `denied`.
pub const AUTH_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl { name: "denied", kind: PortKind::Outcome,
            description: "Authentication or authorization was denied; a 4xx response is prepared. Wire to client (or a custom denial handler)." },
        ERROR,
    ],
};

/// Interactive SSO plugins: denied rejections plus browser redirects.
pub const INTERACTIVE_AUTH_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl { name: "denied", kind: PortKind::Outcome,
            description: "Authentication was denied; a 4xx response is prepared. Wire to client." },
        PortDecl { name: "redirect", kind: PortKind::Outcome,
            description: "The browser must move (login/logout/callback 3xx); response is prepared. Wire to client." },
        ERROR,
    ],
};
```

Registry arms: every ledger-listed credential-auth type → `Some(&ports::AUTH_SPEC)`; `"cas-auth"` → `Some(&ports::INTERACTIVE_AUTH_SPEC)`.

- [ ] **Step 2: Migrate each plugin — worked example (`key_auth.rs`), apply identically to every file in the list**

The `reject` helper (key_auth.rs:127-145) currently builds the 401 and returns `Err(PluginExecutionError { code: "UNAUTHORIZED", ... })`. Replace with:

```rust
    /// Builds the 401 rejection and exits on the `denied` port.
    fn reject(ctx: Context) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 401;
        ctx.response.body =
            Bytes::from(r#"{"error": "unauthorized", "message": "Invalid or missing API key"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Ok(PluginOutput::on_port(ctx, "denied"))
    }
```

Per file: (a) find every deliberate 4xx rejection currently returned as `Err` (the audit ledger cites the lines) and convert it to `on_port(ctx, "denied")`; (b) leave genuine failures as `Err` (e.g. key-auth's consumer-store failure, ldap-auth's connection errors); (c) `cas-auth`: its 302-to-CAS (cas_auth.rs:212 area) becomes `on_port(ctx, "redirect")`, its 401 becomes `denied`; (d) update the file's tests: rejection tests change from `let err = ...unwrap_err(); assert_eq!(err.context.response.status_code, 401)` to `let out = ...unwrap(); assert_eq!(out.port, Some("denied")); assert_eq!(out.context.response.status_code, 401)` — keep asserting the full response body/headers; infrastructure-failure tests keep asserting `Err`.

- [ ] **Step 3: Wire fixtures**

For every auth node in `e2e/fixtures/gateway.yaml` and `website/screenshots/gateway.yaml`, add `.denied` (and `.redirect` for cas-auth if present) edges to that policy's client. Check the e2e expectations that assert 401 flows (key-auth and basic-auth scenarios in `e2e/tests/` — grep `401`) still pass: the response is identical, only the internal route changed.

- [ ] **Step 4: Full suite + e2e**

Run: `cargo test && cargo build --release && cd e2e && npm test`
Expected: PASS. In particular the mock-auth-backed scenarios behave identically at the HTTP level.

- [ ] **Step 5: Commit**

```bash
git add -A src/ e2e/ website/
git commit -m "feat(auth): credential-auth rejections exit on the denied port (cas-auth adds redirect)"
```

---

### Task 9: openid-connect + authz family

**Files:**
- Modify: `src/plugins/native/openid_connect.rs`, `authz_casbin.rs`, `authz_casdoor.rs`, `authz_keycloak.rs`; `ports.rs` + registry arms
- Modify fixtures: `e2e/fixtures/gateway.yaml` (oidc ~235/282), `tests/config/gateway.yaml` (~64), `tests/oidc-test.yml` (~64)
- Test: plugin inline tests; oidc e2e scenarios (`E2E-OIDC-*`)

**Interfaces:**
- Consumes: `INTERACTIVE_AUTH_SPEC` and `AUTH_SPEC` from Task 8.
- Produces: `port_spec("openid-connect")` = `INTERACTIVE_AUTH_SPEC`; `authz-casdoor` = `INTERACTIVE_AUTH_SPEC`; `authz-casbin`/`authz-keycloak` = `AUTH_SPEC`.

- [ ] **Step 1: Write failing tests**

In `openid_connect.rs` tests: the interactive-flow test that today asserts `unwrap_err().error.code == "OIDC_REDIRECT"` changes to assert `out.port == Some("redirect")`, `status == 302`, and a `location` header pointing at the IdP; the reject test asserts `Some("denied")` + 401 + `www-authenticate`. Genuine failures (discovery non-200 at openid_connect.rs:356, JWKS at :386, introspection at :480) keep asserting `Err`. Mirror for the three authz plugins (403 bodies → `denied`; casdoor's 302 → `redirect`).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib openid_connect authz_`
Expected: FAIL on port assertions.

- [ ] **Step 3: Implement**

- `redirect()` (openid_connect.rs:1037): keep the 302/location/set-cookie writes, end with `Ok(PluginOutput::on_port(ctx, "redirect"))`; delete the `OIDC_REDIRECT` GatewayError.
- `reject()` (openid_connect.rs:548): keep the 401 writes, end with `Ok(PluginOutput::on_port(ctx, "denied"))`.
- `authz_casbin.rs` (403 at :177/:262), `authz_keycloak.rs` (403 at :166): `on_port(ctx, "denied")`. `authz_casdoor.rs`: 403 (:252) → `denied`, 302 (:272) → `redirect`.
- Registry arms: `"openid-connect" | "authz-casdoor" => Some(&ports::INTERACTIVE_AUTH_SPEC)`, `"authz-casbin" | "authz-keycloak" => Some(&ports::AUTH_SPEC)`.
- Update the module docs in `openid_connect.rs` (the header comment at :26 explains the 302s — reword around the `redirect` port).

- [ ] **Step 4: Wire fixtures and check oidc e2e**

Add `.denied` and `.redirect` edges to client for every oidc/authz node in the three fixture files. The oidc e2e scenarios drive a real browser through the mock IdP — the HTTP behavior is unchanged, but read `e2e/tests/` oidc specs for any assertion on debug traces or metrics that counted the redirect as an error, and update it to the new honest shape.

- [ ] **Step 5: Full suite + e2e**

Run: `cargo test && cargo build --release && cd e2e && npm test`
Expected: PASS, including the full `E2E-OIDC-*` set.

- [ ] **Step 6: Commit**

```bash
git add -A src/ e2e/ tests/
git commit -m "feat(oidc,authz): login redirects and authz denials exit on dedicated ports"
```

---

### Task 10: Restrictions, traffic control, and the long tail

**Files:**
- Modify (per audit ledger; expected): `acl.rs`, `ip_restriction.rs`, `ua_restriction.rs`, `referer_restriction.rs`, `consumer_restriction.rs`, `uri_blocker.rs`, `csrf.rs`, `request_size_limit.rs`, `workflow.rs` (→ `denied`); `rate_limit.rs`, `limit_conn.rs`, `limit_count.rs` (→ `limited`); `api_breaker.rs` (→ `broken`); plus anything else the ledger flags
- Modify: `ports.rs` (`LIMIT_SPEC`, `BREAKER_SPEC`; restrictions reuse `AUTH_SPEC`'s shape via a `DENY_SPEC`), registry arms
- Modify fixtures: `e2e/fixtures/gateway.yaml` (rate-limit ~191), `website/screenshots/gateway.yaml` (~42)
- Test: each plugin's inline tests; e2e `E2E-DP-07` (429 flow)

**Interfaces:**
- Consumes: pattern from Task 6; audit ledger.
- Produces: `DENY_SPEC` (success/denied/error — same shape as AUTH_SPEC but its own const so descriptions can differ), `LIMIT_SPEC` (success/limited/error), `BREAKER_SPEC` (success/broken/error).

- [ ] **Step 1: Add specs + registry arms** (in `ports.rs`; descriptions follow the pattern of Task 8's — `limited`: "The request exceeded a traffic limit; a 429 response is prepared. Wire to client."; `broken`: "The circuit breaker is open; the break response is prepared. Wire to client."; `denied` as in Task 8)

- [ ] **Step 2: Migrate each plugin, tests-first, exactly as the Task 8 worked example** — per file: rejection tests flip from `unwrap_err()` to `unwrap()` + `assert_eq!(out.port, Some("<port>"))` while keeping full response assertions; deliberate response writes (`acl.rs:107`, `ip_restriction.rs:129/:150`, `ua_restriction.rs:138`, `uri_blocker.rs:135`, `referer_restriction.rs:170`, `consumer_restriction.rs:202`, `csrf.rs:247`, `request_size_limit.rs:60`, `workflow.rs:261` respond action, `rate_limit.rs:162`, `api_breaker.rs:276`, and the limit-conn/limit-count equivalents) become `on_port`. Genuine failures (redis/store errors in rate limiting, breaker state-store failures) stay `Err`.

- [ ] **Step 3: Wire fixtures** — `.limited`/`.denied`/`.broken` edges to client in both fixture files; confirm `E2E-DP-07` (429 past burst) still passes unchanged at the HTTP level.

- [ ] **Step 4: Close the sweep** — walk the audit ledger top to bottom and verify every `outcome` verdict now has a registry arm and a migrated plugin; every `default` verdict compiles against `DEFAULT_SPEC` untouched. Fix any ledger rows discovered wrong during migration (update the ledger in the same commit).

- [ ] **Step 5: Full suite + e2e**

Run: `cargo test && cargo build --release && cd e2e && npm test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A src/ e2e/ website/ docs/
git commit -m "feat(plugins): restriction denials, traffic-limit 429s, and breaker opens exit on dedicated ports"
```

---

### Task 11: Admin catalog serves port metadata

**Files:**
- Modify: `src/admin/policies.rs` (`plugin_catalog()` ~line 174, tests ~line 374+)
- Test: inline tests in `policies.rs`

**Interfaces:**
- Consumes: `port_spec()`, `PortSpec` (it derives `Serialize` since Task 1).
- Produces: each `GET /api/plugins` catalog entry gains `"ports": {"input": <string|null>, "outputs": [{"name","kind","description"}]}` — the exact JSON shape the UI (Task 12) consumes. `kind` serializes lowercase (`"success"|"outcome"|"error"`).

- [ ] **Step 1: Write the failing test**

```rust
    /// Every catalog entry carries its port spec, and outcome ports match the registry.
    #[test]
    fn test_catalog_entries_carry_ports() {
        for p in plugin_catalog() {
            let ty = p["type"].as_str().unwrap();
            let ports = &p["ports"];
            assert!(ports["outputs"].is_array(), "'{ty}' catalog entry lacks ports.outputs");
            let spec = crate::plugins::port_spec(ty).unwrap();
            let names: Vec<&str> = ports["outputs"].as_array().unwrap()
                .iter().map(|o| o["name"].as_str().unwrap()).collect();
            assert_eq!(names, spec.outputs.iter().map(|o| o.name).collect::<Vec<_>>());
        }
        // spot-check kind serialization
        let cors = plugin_catalog().into_iter()
            .find(|p| p["type"] == "cors").unwrap();
        assert_eq!(cors["ports"]["outputs"][1]["kind"], "outcome");
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test --lib test_catalog_entries_carry_ports` → FAIL (no `ports` key).

- [ ] **Step 3: Implement** — in `plugin_catalog()`, for each entry add `"ports": serde_json::to_value(crate::plugins::port_spec(ty).expect("catalog type is registered")).unwrap()` where the `PortSpec` serialization is `{"input": ..., "outputs": [...]}` (rename the struct field in JSON via `#[serde(rename = ...)]` only if the existing struct fields don't already produce exactly `input`/`outputs` — they do).

- [ ] **Step 4: Run the full suite** — `cargo test` → PASS (including the existing catalog drift tests).

- [ ] **Step 5: Commit**

```bash
git add src/admin/policies.rs
git commit -m "feat(admin): plugin catalog advertises per-type port specs"
```

---

### Task 12: UI — dynamic port handles from the catalog

**Files:**
- Modify: `ui/src/types/index.ts` (catalog types), `ui/src/api/client.ts` (if the catalog response type is declared there), `ui/src/pluginMeta.tsx` or a new `ui/src/portSpecs.ts` (spec lookup from the fetched catalog), `ui/src/components/PluginNode.tsx` (dynamic handles), `ui/src/components/GraphCanvas.tsx` (edge styling by kind + unwired-port warning on save)
- Test: `cd ui && npm run build` (typecheck) — behavioral coverage lands in Task 13's e2e

**Interfaces:**
- Consumes: the catalog JSON shape from Task 11 (already fetched — the palette/Sidebar lists plugin types from `GET /api/plugins`; extend that existing fetch's type rather than adding a second request).
- Produces:
  - `PortDecl`/`PortSpec` TS types: `{ name: string; kind: 'success' | 'outcome' | 'error'; description: string }`, `{ input: string | null; outputs: PortDecl[] }`
  - `PluginNodeData.ports?: PortSpec` — GraphCanvas writes it when building nodes (from the catalog, keyed by `pluginType`), PluginNode reads it
  - Edge color rule used by GraphCanvas: `error` → `var(--error)`, `success` → `var(--success)`, outcome ports → `var(--accent)`

- [ ] **Step 1: Types + plumbing** — add the TS types; thread the catalog's `ports` into `PluginNodeData` when GraphCanvas builds nodes. Fallback when a type is missing from the catalog (stale UI vs older gateway): synthesize the default success+error spec so the editor stays usable.

- [ ] **Step 2: Dynamic handles in `PluginNode.tsx`** — replace the hard-coded success/error handles (PluginNode.tsx:157-178) with a render over `ports.outputs`:

```tsx
      {outputs.map((p, i) => (
        <Handle
          key={p.name}
          type="source"
          position={Position.Right}
          id={p.name}
          title={`${p.name} — ${p.description}`}
          style={{
            ...handleStyle(
              p.kind === 'error' ? 'var(--error)'
              : p.kind === 'outcome' ? 'var(--accent)'
              : 'var(--success)'
            ),
            top: `${outputs.length === 1 ? 50 : 25 + (i * 50) / (outputs.length - 1)}%`,
          }}
        />
      ))}
```

where `outputs = nodeData.ports?.outputs ?? [default pair]`; keep the entry/terminal special cases (listener/input have no `in`; client/output/error boundary pseudo-nodes render no outputs — the catalog gives client an empty `outputs`, and the supernode boundary pseudo-nodes keep their current hard-coded treatment since they are not catalog types). Also render a small port-name label next to each handle when a node has more than two outputs (a `<div>` absolutely positioned at the same `top`, `right: -8px` translated outward, `fontSize: var(--text-2xs)`) — three unlabeled dots are not distinguishable.

- [ ] **Step 3: Edge styling + save-time warning in `GraphCanvas.tsx`** — the `isError` binary at GraphCanvas.tsx:174-179 and :338 becomes a three-way lookup of the source node's port kind (falling back to `success` styling for unknown). Before save (`toPolicy` around :241), compute unwired mandatory ports: for each node, `ports.outputs.filter(p => p.kind !== 'error')` minus the ports present among its outgoing edges; if non-empty, show the existing toast/notification mechanism (grep GraphCanvas for how save errors are surfaced — reuse it) listing `node.port` pairs, and still allow the save attempt (the server rejects with the Task 3 message, which the UI already displays).

- [ ] **Step 4: Debug panel port label** — grep `ui/src` for the component rendering trace steps (`EdgeKind`/`edge` from `/api/debug` responses); display `step.port` when present (e.g. badge `→ denied` next to the edge indicator). Small, presentational.

- [ ] **Step 5: Build** — `cd ui && npm run build` → typecheck + build PASS. Then `cargo build` (rust-embed picks up the fresh `ui/dist`) and eyeball the editor: `cargo run` + open the admin UI, confirm a cors node shows three labeled handles.

- [ ] **Step 6: Commit**

```bash
git add ui/
git commit -m "feat(ui): node editor renders declared ports with kind colors, labels, and unwired-port warnings"
```

---

### Task 13: New e2e scenarios + testbook

**Files:**
- Modify: `e2e/E2E_TESTBOOK.md` (scenario catalog), `e2e/tests/admin-api.spec.ts` (or the existing admin spec file — match the suite's layout), `e2e/tests/data-plane.spec.ts`, the UI spec file (`e2e/tests/ui*.spec.ts`)
- Test: the e2e suite itself

**Interfaces:**
- Consumes: everything landed in Tasks 3–12.
- Produces: three new catalogued scenarios (ids continue each file's existing numbering — check the testbook for the next free ids):
  1. **Admin rejects unwired mandatory port** — `PUT` a policy containing a cors node with no `preflight` edge; expect 4xx whose body contains `must be wired — add an edge from` (the Task 3 message).
  2. **OIDC redirect is not an error** — drive the interactive-login scenario, then `GET /metrics` and assert the `node_errors` counter for the oidc node did **not** increment (and did increment after forcing a genuine failure, e.g. pointing discovery at a dead port — reuse the existing oidc failure fixture if one exists, else assert only the non-increment).
  3. **UI renders declared ports and surfaces unwired-port save failure** — load the editor on the cors policy; assert three source handles exist on the cors node (locator on `[data-handleid]` / the `Handle` DOM) and that the `preflight` handle's title contains "preflight". Then delete the `preflight` edge in the editor, attempt save, and assert the save fails with the unwired-port warning/message visible (client warning from Task 12 plus the server's `must be wired` rejection).

- [ ] **Step 1: Write the three scenarios** following the suite's existing helpers (`dataPlane()`, admin client, `test.describe` structure — copy the idioms of the neighboring tests in each file).
- [ ] **Step 2: Run the suite** — `cargo build --release && cd e2e && npm test` → all green, including the three new scenarios.
- [ ] **Step 3: Update `e2e/E2E_TESTBOOK.md`** — add the three rows to the catalog tables; update the prose block at lines ~54-58 that lists E2E-DP-09 among "expected failures": it is no longer one.
- [ ] **Step 4: Commit**

```bash
git add e2e/
git commit -m "test(e2e): unwired-port rejection, redirect-is-not-an-error metrics, and UI port rendering scenarios"
```

---

### Task 14: Docs, migration guide, and ledger closeout

**Files:**
- Create: `website/docs/reference/migration-named-ports.md` (or the docs site's conventions for such pages — check `website/docs/reference/` layout and sidebar config)
- Modify: `website/docs/reference/roadmap.md` (the "Terminal nodes / CORS preflight" row), the plugin pages for every swept plugin (`website/docs/reference/plugins/{cors,openid-connect,redirect,fault-injection,key-auth,basic-auth,jwt-auth,hmac-auth,jwe-decrypt,multi-auth,ldap-auth,cas-auth,authz-casbin,authz-casdoor,authz-keycloak,acl,ip-restriction,ua-restriction,referer-restriction,consumer-restriction,uri-blocker,csrf,request-size-limit,workflow,rate-limit,limit-conn,limit-count,api-breaker}.md` — final list from the audit ledger), the node-graph concepts page (grep `website/docs` for the page describing edges/ports), `docs/apisix-parity.md` (note the port model divergence), `CLAUDE.md` (architecture bullets describing "success/error port routing")
- Test: `cd website && npm run build`

**Interfaces:**
- Consumes: the audit ledger (authoritative list), Task 3's validation messages, the port vocabulary table from the spec.

- [ ] **Step 1: Migration guide** — one page: why (the criterion), the vocabulary table from the spec, the exact compile-error message users will see on upgrade, and a per-plugin table of `port → add this edge`, e.g. `key-auth | denied | {from: "<node>.denied", to: "<client>.in"}`. State plainly this is a config-breaking release.
- [ ] **Step 2: Plugin pages** — each swept plugin's page gains a **Ports** section (name, kind, when it fires, what to wire it to) and its YAML example gains the new edges. Unswept plugins are untouched (their pages inherit the concepts-page default).
- [ ] **Step 3: Concepts + roadmap + parity + CLAUDE.md** — concepts page documents the port model (kinds, mandatory wiring, error fallback chain, `out` alias); roadmap's known-bug row moves to a "fixed in <version>" note (follow how other fixed items are recorded there, or delete the row if that's the convention); `docs/apisix-parity.md` gets a paragraph that rejection/redirect outcomes route via ports here vs terminating inline in APISIX; CLAUDE.md's request-flow and engine-module bullets change "success/error port routing" to "declared-port routing (success/outcome/error)".
- [ ] **Step 4: Build the site** — `cd website && npm run build` → PASS (catches broken links/anchors).
- [ ] **Step 5: Commit**

```bash
git add website/ docs/ CLAUDE.md
git commit -m "docs: named-port model — migration guide, per-plugin port sections, concepts and roadmap updates"
```

---

## Execution order & checkpoints

Tasks are strictly ordered 1 → 14; each leaves `cargo test` green. Tasks 6–10 (the sweep) additionally keep the e2e suite green — the suite boots the release binary, so run `cargo build --release` first. After Task 10, `grep -rn "named_outputs\|named_inputs" src/` must return nothing and every `Err(PluginExecutionError` remaining in `src/plugins/native/` must be a genuine failure per the audit ledger — spot-check five random files. After Task 14, re-read the spec end-to-end against the codebase as a final acceptance pass.
