# Validation Conditions — Design

**Date:** 2026-08-17
**Status:** Approved
**Scope:** Extend the shared condition-expression engine (`src/vars`) with
JSONPath body subjects and presence/null/contains operators, expose a
`conditions` field on the `request-validation` plugin, and add a UI
condition-builder control wired into `request-validation`, `fault-injection`,
and `response-rewrite`.

## Problem

The `request-validation` plugin validates headers and bodies only through JSON
Schema (`header_schema` / `body_schema`). Common gate checks are awkward or
impossible to express that way, and none of it is editable through the UI:

- "the `Authorization` header is present and contains `Bearer`"
- "either `$.user.email` exists in the JSON body, or `$.user.id` is not null"
- arbitrary boolean combinations (NOT / AND / OR, with grouping) of such checks

The repo already has a condition engine — `Expr` in `src/vars/mod.rs`, a port
of APISIX's `lua-resty-expr` triple-array dialect, used by `fault-injection`
and `response-rewrite` via `vars` config. It supports `AND`/`OR` nesting,
per-rule `!` negation, and operators `==`, `~=`/`!=`, `>`, `>=`, `<`, `<=`,
`~~`, `~*`, `in`, `has`, `ipmatch` over flat string context vars. It lacks:

- JSONPath queries into JSON request/response bodies
- an explicit present/absent check (absent vars coerce to `""`, so a missing
  header is indistinguishable from an empty one)
- null-awareness (`key: null` vs key missing)
- a first-class substring/element `contains` operator
- group-level negation (only rules can carry `!`)
- any UI: the `vars` fields are YAML-only today

## Decisions (from brainstorming)

1. **Placement:** extend `request-validation` with a `conditions` field; no
   new node type. Failure exits through the existing `denied` port.
2. **Format:** extend the existing APISIX triple-array dialect rather than
   invent a structured tree format — one engine, one dialect, one docs page.
3. **Null semantics:** three-state. `absent` ≠ present-but-null ≠
   present-with-value. `is_null` is JSONPath-only (config error on flat vars).
4. **Multi-match:** ANY-semantics — a rule over a multi-node JSONPath match is
   true if at least one matched node passes. ALL-semantics is expressed by
   negating the opposite rule.
5. **UI scope:** build a reusable ConditionBuilder control and wire it into
   all three plugins now (`request-validation` conditions,
   `fault-injection` abort/delay `vars`, `response-rewrite` `vars`).

## 1. Engine (`src/vars/mod.rs`, new `src/vars/jsonpath.rs`)

### Subjects

A rule's first element today is a flat var name (`http_authorization`,
`arg_name`, ...). It will additionally accept JSONPath subjects:

- `$.user.name` — JSONPath over the **request** JSON body (bare `$...` is
  shorthand for the request body)
- `request_body:$.user.name` — explicit request-body form
- `response_body:$.data.items[*].id` — JSONPath over the response JSON body

Detection: a subject starting with `$` (JSONPath root), `request_body:`, or
`response_body:` is a JSONPath subject; anything else stays a flat var name.
JSONPath syntax is RFC 9535 via the `serde_json_path` crate (new dependency,
pure Rust). Paths are compiled at config load — malformed paths fail policy
compilation, not requests.

### Evaluation

- The target body is JSON-parsed **lazily, once per `eval` call**, not per
  rule: `eval` wraps the context in an internal eval-state struct holding one
  `OnceCell<Option<serde_json::Value>>` per body (request, response). A body
  that is empty or fails to parse as JSON yields `None` → every JSONPath over
  it matches zero nodes.
- `Expr::eval(&self, ctx: &Context) -> bool` keeps its public signature;
  existing callers (`fault-injection`, `response-rewrite`) are untouched.
- Flat-var rules keep their current behavior exactly (absent coerces to `""`
  for the existing operators).

### New operators

| Operator | Arity | Flat var subject | JSONPath subject |
|---|---|---|---|
| `present` | unary | subject resolves at all (empty string counts as present) | path matches ≥ 1 node (a matched `null` node counts as present) |
| `absent` | unary | subject does not resolve | path matches 0 nodes |
| `is_null` | unary | **config error at parse time** | path matched and ≥ 1 matched node is JSON `null` |
| `contains` | binary | substring match on the resolved string | string node: substring; array node: any element equals the value; other node kinds: false |

Unary rules are written `[subject, "present"]` (no value element); the parser
accepts and the negation form `[subject, "!", "present"]` still works.

### Existing operators over JSONPath nodes

A rule is true if **any** matched node passes (ANY-semantics; `absent` is the
only exception since it asserts zero matches):

- `==` / `~=`/`!=` — native scalar comparison: string↔string, number↔number,
  bool↔bool. `null`, objects, and arrays never equal a scalar config value.
- `>` `>=` `<` `<=` — numeric; non-number nodes fail the comparison.
- `~~` / `~*` — regex over string nodes only.
- `in` — node equals any element of the config array (same scalar rules as `==`).
- `has` — array node contains an element equal to the value.
- `ipmatch` — string nodes parsed as IPs, matched against the CIDR list.

### Group negation

New node form `["NOT", <rule-or-group>]` — exactly one child, which may be a
rule or an `AND`/`OR`/`NOT` group. This completes the boolean algebra at every
level. It is a featherbit extension over APISIX's dialect (APISIX only has
per-rule `!`) and is documented as such.

### Parse-time errors (fail policy compilation)

- malformed JSONPath
- `is_null` on a flat var subject
- unary operator given a value / binary operator missing a value
- `["NOT"]` with zero or ≥ 2 children
- everything `Expr::parse` already rejects (unknown ops, bad regex/CIDR, ...)

## 2. Plugin (`src/plugins/native/request_validation.rs`)

New config key `conditions`: **one** triple-array expression — a JSON/YAML
array of rules ANDed at top level, with nested `AND`/`OR`/`NOT` groups
(exactly the `Expr::parse` shape; not the OR-of-expressions list
`fault-injection` uses).

- Compiled at config load; compile errors fail policy compilation with the
  `request-validation: 'conditions' ...` prefix.
- Config requirement becomes: at least one of `header_schema`, `body_schema`,
  `conditions`.
- Execution order: `header_schema` → `body_schema` → `conditions`. On
  condition failure, the existing `reject` path runs: `denied` port,
  `rejected_code` status, body
  `{"error": "validation_failed", "message": <msg>}` where `<msg>` is
  `rejected_msg` (template-rendered) if set, else the fixed string
  `"request conditions not satisfied"`.
- Per-rule blame in the rejection detail is **out of scope** (deferred):
  `Expr::eval` returns a bool; failure attribution is a separable follow-up.
- `conditions` sees the request as-is at the node's position in the graph;
  since `request-validation` normalizes a schema-validated JSON body before
  `conditions` run, both checks see the same parsed document semantics.

Example:

```yaml
type: request-validation
config:
  rejected_code: 401
  conditions:
    - ["http_authorization", "present"]
    - ["http_authorization", "contains", "Bearer"]
    - ["OR",
        ["$.user.email", "present"],
        ["NOT", ["$.user.id", "is_null"]]]
```

## 3. UI

### ConditionBuilder (`ui/src/components/ConditionBuilder.tsx`)

A reusable control, plus a new `FieldType: 'conditions'` rendered by
`SchemaForm` (`ui/src/pluginConfig.ts`, `ui/src/components/SchemaForm.tsx`):

- **Groups:** nested cards with an AND/OR toggle, "add rule" / "add group"
  buttons, and a NOT toggle (group NOT serializes as `["NOT", [...]]`).
- **Rule rows:**
  - subject picker: Header / Query param / Cookie / Context var /
    JSONPath (request body) / JSONPath (response body). Header, query, and
    cookie pickers emit the `http_` / `arg_` / `cookie_` var forms; Context
    var uses the existing `VarInput` catalog suggestions; JSONPath subjects
    emit `$...` (request shorthand) or `response_body:$...`.
  - NOT toggle (serializes as the rule-level `"!"`).
  - operator dropdown, filtered by subject kind (`is_null` hidden for flat
    subjects; `ipmatch` shown for flat/JSONPath both).
  - value input: hidden for unary ops; list editor for `in` / `ipmatch`;
    plain text/number otherwise.
- **Raw-JSON escape hatch:** a toggle showing the underlying triple-array for
  hand editing with parse-error feedback. Any expression the builder cannot
  represent (or fails to parse) stays in raw mode rather than being mangled —
  round-trip fidelity is a hard requirement.

### Wiring (`ui/src/pluginConfig.ts`)

- `request-validation`: new `conditions` field (single expression shape).
- `fault-injection`: `abort.vars` and `delay.vars` sub-fields; these are
  OR-of-expressions in the Rust config, rendered as a fixed top-level OR
  whose children are AND groups (one per expression in the list).
- `response-rewrite`: its `vars` field, same OR-of-expressions rendering.

## 4. Testing

- **Engine unit tests** (`src/vars/`): each new operator on flat and JSONPath
  subjects; absent vs present-null vs present-value; ANY-semantics over
  multi-node matches; negation of multi-node rules (ALL via NOT); `NOT`
  groups incl. nesting; non-JSON body → absent; body parsed once per eval;
  parse errors (bad path, `is_null` on flat var, unary with value, bare NOT).
- **Plugin unit tests** (`request_validation.rs`): conditions accept/reject,
  denied port + status + message, `rejected_msg` override, conditions-only
  config accepted, config rejections.
- **UI tests** (vitest, alongside `policyGraph.test.ts`): builder-model ↔
  triple-array round-trip, including hand-authored expressions; unrepresentable
  expressions stay raw; operator filtering by subject kind.
- **E2E** (`e2e/`): one scenario — build a conditions rule through the UI on a
  `request-validation` node, save, then hit the data plane with passing and
  failing requests.

## 5. Documentation

- Website: a condition-expression reference page (subjects, operators,
  three-state semantics table, ANY-match rule, NOT extension) under
  `website/docs/`; update the request-validation plugin page and cross-link
  from fault-injection / response-rewrite docs.
- `docs/apisix-parity.md`: note the dialect extensions
  (`present` / `absent` / `is_null` / `contains`, JSONPath subjects, `NOT`
  groups) as featherbit-specific.

## Out of scope

- Per-rule failure attribution in rejection messages (follow-up).
- A general-purpose condition/branching node type.
- JSONPath over non-JSON structured bodies (XML, form-encoded).
- Exposing `conditions` on any plugin beyond the three named here.
