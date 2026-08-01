# Supernodes — Design Spec

Date: 2026-08-01
Status: approved (brainstormed and validated with Francesco)

## Context

Policies in the gateway are node graphs defined per-policy with **no reuse mechanism**: the same auth+upstream+error-handling patterns get copy-pasted across policies and drift apart. This feature adds **supernodes** — named, stored, reusable subgraphs with exactly one input, one success output, and one error output — usable in any policy as a single node. Editing a supernode updates every policy that uses it (true reuse, not copy-paste).

Decisions made during brainstorming:

- **Full-stack V1**: YAML config + Admin API CRUD + etcd + web UI (library page + editor + policy palette). "Extract selection to supernode" is a fast-follow, not V1.
- **Compile-time expansion** (macro/desugar): supernode instances are inlined into a flat `PolicyConfig` before validation-compile. Engine/metrics/trace code untouched.
- **Observability**: inner nodes visible, namespaced as `instance-id/inner-id`.
- **Fixed (no parameters) in V1**; env-var `${VAR}` interpolation still works inside definitions.
- **Boundary declared as virtual nodes** `input`/`output`/`error` inside the definition.
- **Black-box errors**: inner nodes with unwired error ports auto-route out the supernode's error output.
- **No nesting in V1** (supernodes cannot contain supernodes).
- Supernodes are exportable: they are part of `GatewayConfig`, so `GET /api/config/export` includes them and a supernodes-bearing `gateway.yaml` seeds new instances / etcd clusters.

## Design

### 1. Data model (`src/config/gateway.rs`)

New `SupernodeConfig { name: String, description: Option<String>, nodes: Vec<NodeConfig>, edges: Vec<EdgeConfig> }` reusing existing `NodeConfig`/`EdgeConfig`. `GatewayConfig` gains `#[serde(default)] pub supernodes: Vec<SupernodeConfig>` (old configs stay valid; export/etcd/seeding inherit it automatically). Re-export from `src/config/mod.rs`.

```yaml
supernodes:
  - name: secured-call
    description: "Key auth + upstream with unified error exit"
    nodes:
      - { id: input,  type: input }     # boundary nodes declared like listener/client,
      - { id: output, type: output }    # so the UI can persist canvas positions
      - { id: error,  type: error }
      - { id: auth,   type: key-auth }
      - { id: up,     type: upstream, config: { url: "http://svc" } }
    edges:
      - { from: input.out,    to: auth.in }
      - { from: auth.success, to: up.in }
      - { from: auth.error,   to: error.in }
      - { from: up.success,   to: output.in }
```

Usage in a policy — a plain node with `type: supernode`:

```yaml
- { id: sec, type: supernode, config: { name: secured-call } }
# edges wire sec.in / sec.success / sec.error like any node
```

Boundary node ports: `input` has only `out`; `output`/`error` have only `in`; fan-in to `output.in`/`error.in` is allowed.

### 2. Expansion (new `src/graph/expand.rs`)

`expand_policy(policy: &PolicyConfig, supernodes: &[SupernodeConfig]) -> Result<PolicyConfig, String>`, called in `SharedState::compile_routes` (`src/state.rs`) **after** `validate_policy`, **before** `compile_policy`. Also called from the debug sandbox synth path (`src/debug/sandbox.rs`) so sandbox runs support supernodes.

For each instance node `sec` referencing `secured-call`:

1. Inline each inner node `n` as id `sec/n` (same type/config; drop position). Separator `/` is safe: `parse_edge_endpoint` (`src/graph/engine.rs`) splits on the **last dot**, so `/` never confuses port parsing.
2. Rewire inner edges with the prefix on both ends.
3. Splice boundary:
   - outer `X.p -> sec.in` → `X.p -> sec/<target of input.out>.in` (exactly one `input.out` edge required by validation)
   - inner `Y.success -> output.in` → `sec/Y.success -> T.in` where `T` = target of outer `sec.success`; if the policy left `sec.success` unwired, drop the edge (end-of-chain behavior)
   - inner `Z.error -> error.in` → outer `sec.error` target, or dropped (policy catch-all) if unwired
4. Black-box guarantee: every inner node with **no** error edge gets implicit `sec/N.error -> <outer sec.error target>` when the policy wired `sec.error`; otherwise nothing (falls to policy `error_handler`/500).

Unknown supernode name → error `policy 'x': node 'sec' references unknown supernode 'y'`. Multiple instances (even of the same supernode) each get their own namespace. Expansion output is never persisted — stored config always keeps the compact reference form (UI, export, etcd all see un-expanded config).

### 3. Validation (`src/graph/validation.rs`)

New `validate_supernode(&SupernodeConfig) -> Result<(), Vec<String>>`, run for all definitions in `compile_routes` before expanding:

- exactly one node each of types `input`/`output`/`error`, id must equal type
- inner nodes: no `listener`/`client`/`supernode` types; ids must not be reserved (`input`/`output`/`error`) nor contain `/`
- exactly one edge from `input.out`; no edges into `input` or out of `output`/`error`
- fan-in allowed to `output.in`/`error.in` (exempt from single-edge-per-input rule, like `client`/`error-handler` today)
- reuse existing checks: dangling edge endpoints, orphan nodes, single edge per input port elsewhere

Additions elsewhere:

- `validate_policy`: node ids must not contain `/`
- duplicate supernode names rejected
- delete-while-referenced is rejected naturally: whole-config validate+compile before swap (last-good guarantee in `apply_gateway`) fails on the dangling reference

### 4. Admin API (new `src/admin/supernodes.rs`, mirror of `policies.rs`)

- `GET /api/supernodes`, `GET /api/supernodes/{name}`
- `PUT /api/supernodes/{name}` — upsert; commits candidate config via existing `config_store.commit()`, which re-expands/re-compiles all policies → breaking edits return 400 before any change
- `DELETE /api/supernodes/{name}` — 400 if still referenced (commit validation fails), 204 otherwise
- Mount in `build_router` (`src/admin/mod.rs`)
- `/api/plugins` hardcoded catalog **untouched** (`supernode` is not a factory type; expansion removes instances before `create_plugin`) → the three catalog drift-guard tests in `policies.rs` keep passing. UI palette section fed from `GET /api/supernodes`.

### 5. etcd (`src/config_store/etcd.rs`)

New key family `<prefix>/supernodes/<name>`; add `supernode_key()` and branches in `write_all`, `commit`, `is_empty`, `gateway_from_kvs`. Seeding from YAML and cluster convergence follow the existing three kinds. Doc note: older builds sharing a prefix would GC the unknown kind on their next commit (pre-existing store behavior for any schema addition).

### 6. Web UI (`ui/`)

- `ui/src/types/index.ts`: `Supernode` type; `ui/src/api/client.ts`: CRUD methods
- `App.tsx`: load supernodes in the parallel initial fetch; "Supernodes" library section in the sidebar (create/select/delete)
- `GraphCanvas.tsx`: mode flag — supernode-editing mode renders fixed boundary nodes (`input`: source handle only; `output`/`error`: target handle only), palette hides `listener`/`client`/supernodes; save serializes to `SupernodeConfig`
- Policy editor: palette gains a "Supernodes" section from the API; dropped instance = distinct node (own color/icon in `pluginMeta.tsx`, shows the supernode name, ports `in`/`success`/`error`); `NodeInspector` shows the reference + an "edit definition" jump link
- `TraceViewer.tsx`: step `node_id` containing `/` groups under the instance; canvas highlight maps prefix `sec/...` → policy node `sec`

### 7. Observability

No structural change (the point of expansion): metrics get `node_id="sec/auth"` labels; traces record inner steps individually; `GatewayError.node_id` carries the namespaced id so error-handler templates show exactly which inner node failed.

### 8. Testing

- Unit — `expand.rs`: happy path, unwired-error auto-routing, unknown supernode, two instances of same supernode, unwired outer success/error splicing; `validation.rs`: every new rule; serde round-trip for `SupernodeConfig`
- Admin API — CRUD tests incl. delete-while-referenced → 400
- etcd — extend store tests for the new kind (seed, commit, load)
- E2E (Playwright) — create supernode in UI → use in policy → request through data plane → trace shows namespaced inner steps; add to `e2e/E2E_TESTBOOK.md`
- Docs — concept page under `website/docs/` + Admin API reference entries

## Critical files

| Area | Files |
|---|---|
| Config model | `src/config/gateway.rs`, `src/config/mod.rs` |
| Expansion | `src/graph/expand.rs` (new), `src/graph/mod.rs`, `src/state.rs` (`compile_routes`), `src/debug/sandbox.rs` |
| Validation | `src/graph/validation.rs` |
| Admin API | `src/admin/supernodes.rs` (new), `src/admin/mod.rs` |
| etcd | `src/config_store/etcd.rs` |
| UI | `ui/src/types/index.ts`, `ui/src/api/client.ts`, `ui/src/App.tsx`, `ui/src/components/GraphCanvas.tsx`, `ui/src/components/NodeInspector.tsx`, `ui/src/components/PluginDrawer.tsx`, `ui/src/pluginMeta.tsx`, `ui/src/components/TraceViewer.tsx` |
| E2E/docs | `e2e/`, `e2e/E2E_TESTBOOK.md`, `website/docs/` |

## Verification

- `cargo test` — all new unit/integration tests green, catalog drift-guard tests still green
- `cargo build --release && cd e2e && npm test` — new Playwright scenario passes
- Manual: `docker compose up`, build a supernode in the UI, wire into a policy, hit the route, inspect `/metrics` (namespaced node ids) and the Debug trace panel
- `GET /api/config/export` includes the `supernodes:` section (seed-a-new-instance workflow)
