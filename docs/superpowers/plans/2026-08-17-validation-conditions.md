# Validation Conditions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Boolean predicate conditions (NOT/AND/OR with grouping) over headers, context vars, and JSONPath queries into JSON bodies — evaluated by the shared `Expr` engine, exposed as a `conditions` field on `request-validation`, and editable in the UI through a new ConditionBuilder control also wired into `fault-injection` and `response-rewrite`.

**Architecture:** Extend the existing APISIX triple-array condition engine (`src/vars/mod.rs`, `Expr`) with JSONPath subjects (compiled at config load via `serde_json_path`, body parsed lazily once per eval), new operators (`present`, `absent`, `is_null`, `contains`), and a `["NOT", ...]` group node. The `request-validation` plugin gains a `conditions` key that rejects through its existing `denied` port. The UI gains a pure serialization module (`ui/src/conditions.ts`), a `ConditionBuilder` React component, and two new `SchemaForm` field types (`conditions`, `object`).

**Tech Stack:** Rust (serde_json, new dep `serde_json_path` 0.7, regex, ipnet), React + TypeScript (vitest), Playwright e2e, Docusaurus docs.

**Spec:** `docs/superpowers/specs/2026-08-17-validation-conditions-design.md`

## Global Constraints

- Conventional Commits, **no Co-Authored-By trailer** (project CLAUDE.md).
- Branch: all work on `feature/validation-conditions` (already created off `develop`).
- `Expr::eval(&self, ctx: &Context) -> bool` public signature must not change — `fault-injection` and `response-rewrite` call it as-is.
- Flat-var rules keep exact current behavior (absent coerces to `""` for existing operators); all pre-existing tests in `src/vars/mod.rs` must keep passing unmodified except where a task explicitly says otherwise.
- ANY-match semantics for JSONPath multi-node results (only `absent` asserts zero matches).
- Three-state semantics: absent ≠ present-null ≠ present-value. `is_null` on a flat var subject is a **parse-time error**.
- UI round-trip fidelity: any expression the builder cannot represent stays editable as raw JSON — never silently mangled.
- Run tests with `cargo test` (Rust), `cd ui && npm test` (vitest), `cd e2e && npm test` (Playwright, needs `cargo build --release` first).

---

### Task 1: Operator values become typed scalars (pure refactor)

Prepares `Op` to compare JSON nodes natively later: `Eq`/`Ne`/`In`/`Has`/`Contains` need the config value's original JSON type, but flat-var evaluation must keep stringifying exactly as today.

**Files:**
- Modify: `src/vars/mod.rs` (enum `Op` ~line 195, `parse_node` ~line 234, `eval_op` ~line 359)

**Interfaces:**
- Produces: `enum Op { Eq(serde_json::Value), Ne(serde_json::Value), Gt(f64), Ge(f64), Lt(f64), Le(f64), Regex(Regex), In(Vec<serde_json::Value>), Has(serde_json::Value), IpMatch(Vec<IpNet>) }` and helper `fn scalar_str(v: &serde_json::Value) -> Option<String>` (Some for String/Number/Bool, None otherwise). Task 2/3 rely on these exact shapes.

- [ ] **Step 1: Run the existing engine tests to establish the baseline**

Run: `cargo test vars:: -- --nocapture` (or `cargo test test_expr_`)
Expected: PASS (note the count — it must not drop).

- [ ] **Step 2: Refactor `Op` to store `serde_json::Value` scalars**

In `src/vars/mod.rs`:

```rust
enum Op {
    Eq(serde_json::Value),
    Ne(serde_json::Value),
    Gt(f64),
    Ge(f64),
    Lt(f64),
    Le(f64),
    Regex(Regex),
    In(Vec<serde_json::Value>),
    Has(serde_json::Value),
    IpMatch(Vec<IpNet>),
}

/// Stringifies a scalar config value the way the legacy parser did
/// (String as-is, Number/Bool via to_string). None for null/array/object.
fn scalar_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
```

In `parse_node`, replace the `scalar` closure with one returning the validated `serde_json::Value`:

```rust
let scalar = || -> Result<serde_json::Value, String> {
    scalar_str(value)
        .map(|_| value.clone())
        .ok_or_else(|| format!("rule for '{}' needs a scalar value", var))
};
```

`"=="` → `Op::Eq(scalar()?)`, `"~="|"!="` → `Op::Ne(scalar()?)`. `"~~"`/`"~*"` build their `Regex` from `scalar_str(value).ok_or_else(...)?` (the pattern string). `"has"` → `Op::Has(scalar()?)`. For `"in"`, keep validating items as scalars but store the raw values:

```rust
"in" => {
    let items = value
        .as_array()
        .ok_or_else(|| format!("rule for '{}' (in) needs an array value", var))?;
    for item in items {
        if scalar_str(item).is_none() {
            return Err(format!("rule for '{}': array items must be scalars", var));
        }
    }
    Op::In(items.clone())
}
```

`"ipmatch"` keeps using the existing `string_list()` closure (it still needs strings to parse CIDRs).

In `eval_op`, stringify at comparison time so flat behavior is bit-identical:

```rust
Op::Eq(expected) => scalar_str(expected).as_deref() == Some(v),
Op::Ne(expected) => scalar_str(expected).as_deref() != Some(v),
Op::In(list) => list.iter().any(|item| scalar_str(item).as_deref() == Some(v)),
Op::Has(needle) => {
    let needle = scalar_str(needle).unwrap_or_default();
    v.split(',').map(str::trim).any(|part| part == needle)
}
```

- [ ] **Step 3: Run the engine tests again**

Run: `cargo test vars::`
Expected: PASS, same test count as Step 1. Also run `cargo test test_fault_injection test_response_rewrite` — PASS.

- [ ] **Step 4: Commit**

```bash
git add src/vars/mod.rs
git commit -m "refactor(vars): store condition operator values as typed JSON scalars"
```

---

### Task 2: JSONPath subjects with lazy body parsing

**Files:**
- Modify: `Cargo.toml` (add `serde_json_path = "0.7"` near `jsonschema`)
- Create: `src/vars/jsonpath.rs`
- Modify: `src/vars/mod.rs` (add `pub mod jsonpath;`, `Subject` enum, `EvalState`, `eval_op_json`)
- Test: inline `#[cfg(test)]` in both files

**Interfaces:**
- Consumes: `Op`, `scalar_str` from Task 1.
- Produces:
  - `src/vars/jsonpath.rs`: `pub enum BodyTarget { Request, Response }`, `pub struct JsonSubject { pub target: BodyTarget, pub path: serde_json_path::JsonPath, pub raw: String }`, `pub fn parse_json_subject(subject: &str) -> Option<Result<JsonSubject, String>>` (None = not a JSONPath subject, i.e. a flat var).
  - `src/vars/mod.rs`: internal `enum Subject { Var(String), Json(jsonpath::JsonSubject) }`; `Node::Rule` field `var: String` replaced by `subject: Subject`. Rule syntax accepted by `Expr::parse`: subject strings starting with `$` (request body), `request_body:$...`, `response_body:$...`.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, next to the `jsonschema` line:

```toml
serde_json_path = "0.7"
```

Run: `cargo build` — Expected: compiles, dependency resolves.

- [ ] **Step 2: Write failing tests for subject detection (new file)**

Create `src/vars/jsonpath.rs`:

```rust
//! JSONPath subjects for condition expressions: `$.user.name` (request
//! body), `request_body:$...`, `response_body:$...`. Paths are compiled at
//! config load (RFC 9535 via serde_json_path); malformed paths fail policy
//! compilation, not requests.

use serde_json_path::JsonPath;

/// Which body a JSONPath subject queries.
pub enum BodyTarget {
    Request,
    Response,
}

/// A compiled JSONPath subject.
pub struct JsonSubject {
    pub target: BodyTarget,
    pub path: JsonPath,
    /// The subject string as written in config, for error messages.
    pub raw: String,
}

/// Recognizes and compiles a JSONPath subject. `None` means the subject is
/// a plain var name; `Some(Err)` means it looked like a JSONPath subject
/// but the path is malformed.
pub fn parse_json_subject(subject: &str) -> Option<Result<JsonSubject, String>> {
    let (target, path_str) = if let Some(p) = subject.strip_prefix("request_body:") {
        (BodyTarget::Request, p)
    } else if let Some(p) = subject.strip_prefix("response_body:") {
        (BodyTarget::Response, p)
    } else if subject.starts_with('$') {
        (BodyTarget::Request, subject)
    } else {
        return None;
    };
    Some(
        JsonPath::parse(path_str)
            .map(|path| JsonSubject {
                target,
                path,
                raw: subject.to_string(),
            })
            .map_err(|e| format!("invalid JSONPath '{}': {}", subject, e)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonpath_subject_detection() {
        assert!(parse_json_subject("http_authorization").is_none());
        assert!(parse_json_subject("arg_name").is_none());
        assert!(matches!(
            parse_json_subject("$.user.name"),
            Some(Ok(JsonSubject { target: BodyTarget::Request, .. }))
        ));
        assert!(matches!(
            parse_json_subject("request_body:$.a"),
            Some(Ok(JsonSubject { target: BodyTarget::Request, .. }))
        ));
        assert!(matches!(
            parse_json_subject("response_body:$.a[*].b"),
            Some(Ok(JsonSubject { target: BodyTarget::Response, .. }))
        ));
        // looked like JSONPath, malformed path -> hard error
        assert!(matches!(parse_json_subject("$.["), Some(Err(_))));
        assert!(matches!(parse_json_subject("response_body:nope"), Some(Err(_))));
    }
}
```

- [ ] **Step 3: Register the module and run**

In `src/vars/mod.rs` next to `pub mod catalog;`:

```rust
pub mod jsonpath;
```

Run: `cargo test vars::jsonpath` — Expected: PASS (the module is self-contained).

- [ ] **Step 4: Write failing engine tests for JSONPath rules**

In the `#[cfg(test)] mod tests` of `src/vars/mod.rs` (reuse the file's existing `ctx()`-style helper — the test module already builds Contexts; add a JSON body variant next to it):

```rust
fn json_body_ctx(body: &str) -> Context {
    let mut ctx = ctx(); // the module's existing context factory
    ctx.request.body = bytes::Bytes::from(body.to_string());
    ctx
}

#[test]
fn test_expr_jsonpath_subjects() {
    let ctx = json_body_ctx(r#"{"user":{"name":"jack","age":30,"tags":["a","b"],"admin":true},"items":[{"price":5},{"price":0}]}"#);

    let truthy = [
        serde_json::json!([["$.user.name", "==", "jack"]]),
        serde_json::json!([["request_body:$.user.name", "==", "jack"]]),
        serde_json::json!([["$.user.age", ">", 18]]),
        serde_json::json!([["$.user.admin", "==", true]]),
        serde_json::json!([["$.user.name", "~~", "^ja"]]),
        serde_json::json!([["$.user.age", "in", [30, 40]]]),
        serde_json::json!([["$.user.tags", "has", "a"]]),
        // ANY-match: one item has price > 1
        serde_json::json!([["$.items[*].price", ">", 1]]),
        // ALL via negation: NOT(any price < 0)
        serde_json::json!([["$.items[*].price", "!", "<", 0]]),
    ];
    for case in &truthy {
        assert!(Expr::parse(case).unwrap().eval(&ctx), "should be true: {case}");
    }

    let falsy = [
        // number node never equals a string scalar
        serde_json::json!([["$.user.age", "==", "30"]]),
        serde_json::json!([["$.user.name", "==", "jill"]]),
        // absent path matches nothing -> comparison false
        serde_json::json!([["$.missing", "==", "x"]]),
        // object node never equals a scalar
        serde_json::json!([["$.user", "==", "jack"]]),
    ];
    for case in &falsy {
        assert!(!Expr::parse(case).unwrap().eval(&ctx), "should be false: {case}");
    }
}

#[test]
fn test_expr_jsonpath_response_body_and_non_json() {
    let mut ctx = ctx();
    ctx.response.body = bytes::Bytes::from(r#"{"ok":true}"#.to_string());
    assert!(Expr::parse(&serde_json::json!([["response_body:$.ok", "==", true]]))
        .unwrap()
        .eval(&ctx));

    // non-JSON request body: every request-body path matches zero nodes
    let ctx = json_body_ctx("plain text");
    assert!(!Expr::parse(&serde_json::json!([["$.a", "==", "x"]])).unwrap().eval(&ctx));
}

#[test]
fn test_expr_jsonpath_parse_errors() {
    assert!(Expr::parse(&serde_json::json!([["$.[", "==", "x"]])).is_err());
}
```

Run: `cargo test test_expr_jsonpath` — Expected: FAIL (compile error: `Subject` doesn't exist yet).

- [ ] **Step 5: Implement `Subject`, `EvalState`, and JSON evaluation**

In `src/vars/mod.rs`:

```rust
use std::cell::OnceCell;
use crate::vars::jsonpath::{BodyTarget, JsonSubject};

enum Subject {
    Var(String),
    Json(JsonSubject),
}

enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Rule { subject: Subject, negate: bool, op: Op },
}
```

In `parse_node`, right after extracting the subject string (the current `var`), build the `Subject` (keep the string around for error messages):

```rust
let subject = match jsonpath::parse_json_subject(&var) {
    None => Subject::Var(var.clone()),
    Some(compiled) => Subject::Json(compiled?),
};
```

…and construct `Node::Rule { subject, negate, op }` at the end. The closures' error messages keep using `var`.

Per-eval lazy body cache (single-threaded within one `eval` call, hence `std::cell::OnceCell`):

```rust
/// Per-evaluation state: the context plus lazily-parsed JSON bodies, so a
/// multi-rule expression parses each body at most once per eval.
struct EvalState<'a> {
    ctx: &'a Context,
    request_json: OnceCell<Option<serde_json::Value>>,
    response_json: OnceCell<Option<serde_json::Value>>,
}

impl<'a> EvalState<'a> {
    fn new(ctx: &'a Context) -> Self {
        Self { ctx, request_json: OnceCell::new(), response_json: OnceCell::new() }
    }

    /// The parsed JSON body, or None when empty/not valid JSON.
    fn body_json(&self, target: &BodyTarget) -> Option<&serde_json::Value> {
        let (cell, bytes) = match target {
            BodyTarget::Request => (&self.request_json, &self.ctx.request.body),
            BodyTarget::Response => (&self.response_json, &self.ctx.response.body),
        };
        cell.get_or_init(|| serde_json::from_slice(bytes).ok()).as_ref()
    }
}
```

`Expr::eval` wraps (public signature unchanged):

```rust
pub fn eval(&self, ctx: &Context) -> bool {
    let state = EvalState::new(ctx);
    eval_node(&self.root, &state)
}
```

`eval_node` takes `&EvalState` instead of `&Context`:

```rust
fn eval_node(node: &Node, state: &EvalState) -> bool {
    match node {
        Node::And(children) => children.iter().all(|c| eval_node(c, state)),
        Node::Or(children) => children.iter().any(|c| eval_node(c, state)),
        Node::Rule { subject, negate, op } => {
            let result = match subject {
                Subject::Var(name) => eval_op(op, resolve(state.ctx, name).as_deref()),
                Subject::Json(js) => {
                    let nodes: Vec<&serde_json::Value> = match state.body_json(&js.target) {
                        Some(doc) => js.path.query(doc).all(),
                        None => Vec::new(),
                    };
                    eval_op_json(op, &nodes)
                }
            };
            if *negate { !result } else { result }
        }
    }
}
```

JSON-node evaluation (ANY-match) and native scalar comparison:

```rust
/// ANY-match: the rule holds if at least one matched node passes.
fn eval_op_json(op: &Op, nodes: &[&serde_json::Value]) -> bool {
    nodes.iter().any(|n| eval_op_json_node(op, n))
}

fn eval_op_json_node(op: &Op, node: &serde_json::Value) -> bool {
    match op {
        Op::Eq(expected) => json_scalar_eq(node, expected),
        Op::Ne(expected) => !json_scalar_eq(node, expected),
        Op::Gt(n) => node.as_f64().is_some_and(|x| x > *n),
        Op::Ge(n) => node.as_f64().is_some_and(|x| x >= *n),
        Op::Lt(n) => node.as_f64().is_some_and(|x| x < *n),
        Op::Le(n) => node.as_f64().is_some_and(|x| x <= *n),
        Op::Regex(re) => node.as_str().is_some_and(|s| re.is_match(s)),
        Op::In(list) => list.iter().any(|e| json_scalar_eq(node, e)),
        Op::Has(v) => node
            .as_array()
            .is_some_and(|arr| arr.iter().any(|e| json_scalar_eq(e, v))),
        Op::IpMatch(nets) => node
            .as_str()
            .and_then(|s| s.parse::<IpAddr>().ok())
            .is_some_and(|ip| nets.iter().any(|net| net.contains(&ip))),
    }
}

/// Native scalar equality: string↔string, number↔number, bool↔bool.
/// null, arrays, and objects never equal a scalar config value.
fn json_scalar_eq(node: &serde_json::Value, expected: &serde_json::Value) -> bool {
    use serde_json::Value as V;
    match (node, expected) {
        (V::String(a), V::String(b)) => a == b,
        (V::Number(a), V::Number(b)) => a.as_f64() == b.as_f64(),
        (V::Bool(a), V::Bool(b)) => a == b,
        _ => false,
    }
}
```

- [ ] **Step 6: Run all engine tests**

Run: `cargo test vars::`
Expected: PASS — new tests and every pre-existing test.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/vars/jsonpath.rs src/vars/mod.rs
git commit -m "feat(vars): JSONPath body subjects in condition expressions"
```

---

### Task 3: `present`, `absent`, `is_null`, `contains` operators

**Files:**
- Modify: `src/vars/mod.rs` (`Op`, `parse_node` arity handling, `eval_op`, `eval_op_json`, `eval_op_json_node`)
- Test: inline tests in `src/vars/mod.rs`

**Interfaces:**
- Consumes: `Subject`, `scalar_str`, `json_scalar_eq` from Tasks 1–2.
- Produces: `Op::Present`, `Op::Absent`, `Op::IsNull`, `Op::Contains(serde_json::Value)`. Rule arities: `[subject, "present"]`, `[subject, "!", "present"]` (unary, no value; value present = parse error); `is_null` on `Subject::Var` = parse error.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn test_expr_present_absent() {
    // ctx() has header x-api-version (adapt to the module's factory);
    // build one with a known header + JSON body:
    let mut ctx = json_body_ctx(r#"{"a": null, "b": 1}"#);
    ctx.request.headers.insert("x-empty".to_string(), vec!["".to_string()]);

    let truthy = [
        serde_json::json!([["http_x_empty", "present"]]),      // empty value still present
        serde_json::json!([["http_x_missing", "absent"]]),
        serde_json::json!([["http_x_missing", "!", "present"]]),
        serde_json::json!([["$.a", "present"]]),                 // null node counts as present
        serde_json::json!([["$.missing", "absent"]]),
        serde_json::json!([["$.a", "is_null"]]),
        serde_json::json!([["$.b", "!", "is_null"]]),
    ];
    for case in &truthy {
        assert!(Expr::parse(case).unwrap().eval(&ctx), "should be true: {case}");
    }

    let falsy = [
        serde_json::json!([["http_x_empty", "absent"]]),
        serde_json::json!([["$.missing", "present"]]),
        serde_json::json!([["$.missing", "is_null"]]),           // absent is NOT null
        serde_json::json!([["$.b", "is_null"]]),
    ];
    for case in &falsy {
        assert!(!Expr::parse(case).unwrap().eval(&ctx), "should be false: {case}");
    }
}

#[test]
fn test_expr_contains() {
    let mut ctx = json_body_ctx(r#"{"tags":["a","b"],"nums":[1,2],"name":"hello world"}"#);
    ctx.request.headers.insert(
        "authorization".to_string(),
        vec!["Bearer abc123".to_string()],
    );

    let truthy = [
        serde_json::json!([["http_authorization", "contains", "Bearer"]]),
        serde_json::json!([["$.name", "contains", "lo wo"]]),
        serde_json::json!([["$.tags", "contains", "a"]]),   // array element equality
        serde_json::json!([["$.nums", "contains", 2]]),
    ];
    for case in &truthy {
        assert!(Expr::parse(case).unwrap().eval(&ctx), "should be true: {case}");
    }
    let falsy = [
        serde_json::json!([["http_authorization", "contains", "Basic"]]),
        serde_json::json!([["http_x_missing", "contains", "x"]]),  // absent -> false
        serde_json::json!([["$.nums", "contains", "2"]]),          // "2" != 2 in arrays
        serde_json::json!([["$.nums", "contains", 3]]),
    ];
    for case in &falsy {
        assert!(!Expr::parse(case).unwrap().eval(&ctx), "should be false: {case}");
    }
}

#[test]
fn test_expr_new_operator_parse_errors() {
    let cases = [
        // is_null on a flat var
        serde_json::json!([["http_x", "is_null"]]),
        // unary op given a value
        serde_json::json!([["http_x", "present", "y"]]),
        // binary op missing a value (already an error; pin it stays one)
        serde_json::json!([["http_x", "contains"]]),
    ];
    for case in &cases {
        assert!(Expr::parse(case).is_err(), "should fail to parse: {case}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_expr_present test_expr_contains test_expr_new_operator`
Expected: FAIL (unknown operator errors / compile).

- [ ] **Step 3: Implement**

Add variants:

```rust
    Present,
    Absent,
    IsNull,
    Contains(serde_json::Value),
```

In `parse_node`, restructure around arity. After computing `op_str`, handle unary operators before the `value` extraction:

```rust
let is_unary = matches!(op_str, "present" | "absent" | "is_null");
if is_unary {
    if arr.len() > op_idx + 1 {
        return Err(format!("rule for '{}': '{}' takes no value", var, op_str));
    }
    let op = match op_str {
        "present" => Op::Present,
        "absent" => Op::Absent,
        "is_null" => {
            if matches!(subject, Subject::Var(_)) {
                return Err(format!(
                    "rule for '{}': 'is_null' requires a JSONPath subject — headers and vars cannot be null (use 'absent')",
                    var
                ));
            }
            Op::IsNull
        }
        _ => unreachable!(),
    };
    return Ok(Node::Rule { subject, negate, op });
}
```

(This means the `Subject` construction from Task 2 must happen before the op match — move it up if needed.) Add `"contains" => Op::Contains(scalar()?)` to the binary match, and extend the unknown-operator error text: `supported: ==, ~=, >, >=, <, <=, ~~, ~*, in, has, ipmatch, present, absent, is_null, contains`.

Flat-var evaluation — `eval_op` receives `Option<&str>`; `Present`/`Absent` need the Option, others the coerced `v`:

```rust
Op::Present => value.is_some(),
Op::Absent => value.is_none(),
Op::IsNull => false, // parse-time guarded; a flat var is never null
Op::Contains(needle) => {
    value.is_some_and(|v| {
        scalar_str(needle).is_some_and(|n| v.contains(&n))
    })
}
```

JSON evaluation — `Present`/`Absent`/`IsNull` are match-set operators, not per-node; special-case them in `eval_op_json`:

```rust
fn eval_op_json(op: &Op, nodes: &[&serde_json::Value]) -> bool {
    match op {
        Op::Present => !nodes.is_empty(),
        Op::Absent => nodes.is_empty(),
        Op::IsNull => nodes.iter().any(|n| n.is_null()),
        _ => nodes.iter().any(|n| eval_op_json_node(op, n)),
    }
}
```

…and in `eval_op_json_node`:

```rust
Op::Contains(v) => match node {
    serde_json::Value::String(s) => {
        scalar_str(v).is_some_and(|needle| s.contains(&needle))
    }
    serde_json::Value::Array(arr) => arr.iter().any(|e| json_scalar_eq(e, v)),
    _ => false,
},
Op::Present | Op::Absent | Op::IsNull => unreachable!("handled in eval_op_json"),
```

- [ ] **Step 4: Run all engine tests**

Run: `cargo test vars::`
Expected: PASS (new + all pre-existing).

- [ ] **Step 5: Commit**

```bash
git add src/vars/mod.rs
git commit -m "feat(vars): present/absent/is_null/contains condition operators"
```

---

### Task 4: `["NOT", ...]` group node

**Files:**
- Modify: `src/vars/mod.rs` (`Node`, `parse_node`, `eval_node`)
- Test: inline tests in `src/vars/mod.rs`; module doc comment update

**Interfaces:**
- Consumes: `Node`, `parse_node`, `eval_node` from Task 2.
- Produces: `Node::Not(Box<Node>)`; syntax `["NOT", <rule-or-group>]` (exactly one child, case-insensitive keyword). Featherbit extension over APISIX — documented in the module doc.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn test_expr_not_group() {
    let mut ctx = json_body_ctx(r#"{"user":{"id":null}}"#);
    ctx.request.headers.insert(
        "authorization".to_string(),
        vec!["Bearer tok".to_string()],
    );

    // NOT over a rule
    assert!(!Expr::parse(&serde_json::json!([
        ["NOT", ["http_authorization", "present"]]
    ])).unwrap().eval(&ctx));

    // NOT over a group, nested logic (spec example shape)
    let e = Expr::parse(&serde_json::json!([
        ["http_authorization", "present"],
        ["http_authorization", "contains", "Bearer"],
        ["OR",
            ["$.user.email", "present"],
            ["NOT", ["$.user.id", "is_null"]]]
    ])).unwrap();
    // email absent AND id is null -> OR arm: (false OR NOT(true)) = false
    assert!(!e.eval(&ctx));

    // nested NOT(NOT(x)) == x
    assert!(Expr::parse(&serde_json::json!([
        ["NOT", ["NOT", ["http_authorization", "present"]]]
    ])).unwrap().eval(&ctx));
}

#[test]
fn test_expr_not_parse_errors() {
    // zero children
    assert!(Expr::parse(&serde_json::json!([["NOT"]])).is_err());
    // two children
    assert!(Expr::parse(&serde_json::json!([
        ["NOT", ["http_a", "present"], ["http_b", "present"]]
    ])).is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_expr_not`
Expected: FAIL.

- [ ] **Step 3: Implement**

`Node` gains `Not(Box<Node>)`. In `parse_node`, in the same block that recognizes `and`/`or` (a first element that is the string `"not"`, case-insensitive):

```rust
if first.eq_ignore_ascii_case("not") {
    if arr.len() != 2 {
        return Err("NOT takes exactly one rule or group".to_string());
    }
    return Ok(Node::Not(Box::new(parse_node(&arr[1])?)));
}
```

In `eval_node`:

```rust
Node::Not(child) => !eval_node(child, state),
```

Update the module doc comment (lines 14–18) to document the new subjects and operators:

```
//! Rules in a top-level list are ANDed. A rule is `[subject, op, value]`,
//! the negated `[subject, "!", op, value]`, or unary `[subject, "present"|"absent"|"is_null"]`.
//! Nested logic uses `["AND", rule...]`, `["OR", rule...]`, and `["NOT", rule-or-group]`
//! (NOT is a featherbit extension over APISIX's dialect). Subjects are var
//! names or JSONPath queries over JSON bodies: `$.user.name` (request body),
//! `request_body:$...`, `response_body:$...` — multi-node matches use
//! ANY-semantics. Operators: `==`, `~=`, `>`, `>=`, `<`, `<=`, `~~` (regex),
//! `~*` (case-insensitive regex), `in`, `has`, `ipmatch`, `present`,
//! `absent`, `is_null` (JSONPath only), `contains`.
```

- [ ] **Step 4: Run all engine tests**

Run: `cargo test vars::` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/vars/mod.rs
git commit -m "feat(vars): NOT group node for condition expressions"
```

---

### Task 5: `conditions` field on request-validation

**Files:**
- Modify: `src/plugins/native/request_validation.rs`
- Test: inline tests in the same file

**Interfaces:**
- Consumes: `crate::vars::Expr` (`parse`, `eval`) with all Task 2–4 syntax.
- Produces: config key `conditions` (one triple-array expression). At least one of `header_schema`/`body_schema`/`conditions` required. Failure → `denied` port, `rejected_code`, message `"request conditions not satisfied"` (unless `rejected_msg`).

- [ ] **Step 1: Write failing tests**

Add to the file's test module (reuse the existing `test_context` helper):

```rust
fn conditions_plugin(conditions: serde_json::Value) -> RequestValidationPlugin {
    let mut config = HashMap::new();
    config.insert("conditions".to_string(), conditions);
    RequestValidationPlugin::from_config(&config).unwrap()
}

#[tokio::test]
async fn test_request_validation_conditions_accept() {
    let p = conditions_plugin(serde_json::json!([
        ["http_authorization", "present"],
        ["http_authorization", "contains", "Bearer"],
        ["OR", ["$.user.email", "present"], ["NOT", ["$.user.id", "is_null"]]]
    ]));
    let mut ctx = test_context(r#"{"user":{"email":"a@b.c"}}"#, Some("application/json"));
    ctx.request.headers.insert(
        "authorization".to_string(),
        vec!["Bearer tok".to_string()],
    );
    let out = p.execute(ctx).await.unwrap();
    assert!(out.port.is_none());
}

#[tokio::test]
async fn test_request_validation_conditions_reject() {
    let p = conditions_plugin(serde_json::json!([
        ["http_authorization", "contains", "Bearer"]
    ]));
    let out = p
        .execute(test_context("", None))
        .await
        .unwrap();
    assert_eq!(out.port, Some("denied"));
    assert_eq!(out.context.response.status_code, 400);
    let body: serde_json::Value = serde_json::from_slice(&out.context.response.body).unwrap();
    assert_eq!(body["error"], "validation_failed");
    assert_eq!(body["message"], "request conditions not satisfied");
}

#[tokio::test]
async fn test_request_validation_conditions_after_schema() {
    // both body_schema and conditions: schema normalizes, conditions still run
    let mut config = HashMap::new();
    config.insert(
        "body_schema".to_string(),
        serde_json::json!({ "type": "object" }),
    );
    config.insert(
        "conditions".to_string(),
        serde_json::json!([["$.name", "==", "jack"]]),
    );
    let p = RequestValidationPlugin::from_config(&config).unwrap();

    let ctx = test_context(r#"{"name": "jack"}"#, Some("application/json"));
    assert!(p.execute(ctx).await.unwrap().port.is_none());

    let ctx = test_context(r#"{"name": "jill"}"#, Some("application/json"));
    assert_eq!(p.execute(ctx).await.unwrap().port, Some("denied"));
}

#[test]
fn test_request_validation_conditions_config() {
    // conditions alone satisfies the at-least-one requirement
    let mut config = HashMap::new();
    config.insert(
        "conditions".to_string(),
        serde_json::json!([["http_x", "present"]]),
    );
    assert!(RequestValidationPlugin::from_config(&config).is_ok());

    // malformed conditions fail at config load, with plugin-prefixed message
    let mut config = HashMap::new();
    config.insert("conditions".to_string(), serde_json::json!([["$.a", "bogus_op", 1]]));
    let err = RequestValidationPlugin::from_config(&config).unwrap_err();
    assert!(err.starts_with("request-validation: invalid 'conditions'"), "{err}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_request_validation_conditions`
Expected: FAIL (`conditions_plugin` panics: "at least one of 'header_schema' or 'body_schema' is required").

- [ ] **Step 3: Implement**

Struct field:

```rust
    conditions: Option<crate::vars::Expr>,
```

In `from_config`, after `body_schema`:

```rust
let conditions = match config.get("conditions") {
    None => None,
    Some(v) => Some(
        crate::vars::Expr::parse(v)
            .map_err(|e| format!("request-validation: invalid 'conditions': {}", e))?,
    ),
};

if header_schema.is_none() && body_schema.is_none() && conditions.is_none() {
    return Err(
        "request-validation: at least one of 'header_schema', 'body_schema', or 'conditions' is required"
            .to_string(),
    );
}
```

Update the `from_config` doc comment's accepted-keys list with:

```
/// - `conditions` (array): a condition expression (see [`crate::vars::Expr`])
///   — rules ANDed at top level, nested `AND`/`OR`/`NOT` groups, JSONPath
///   body subjects. Evaluated after the schemas; failure rejects like a
///   schema failure with message "request conditions not satisfied".
```

In `execute`, after the body-schema block (so conditions see the normalized JSON body) and before the final `Ok`:

```rust
if let Some(expr) = &self.conditions {
    if !expr.eval(&ctx) {
        return self.reject(ctx, "request conditions not satisfied".to_string());
    }
}
```

Update the struct doc comment to mention conditions. Also update `Self { ... }` construction with `conditions`.

- [ ] **Step 4: Run the plugin tests**

Run: `cargo test test_request_validation` — Expected: PASS (all, including pre-existing).

- [ ] **Step 5: Run the full Rust suite**

Run: `cargo test` — Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/plugins/native/request_validation.rs
git commit -m "feat(request-validation): boolean 'conditions' predicates alongside JSON Schemas"
```

---

### Task 6: UI condition model + triple-array serialization (pure TS)

**Files:**
- Create: `ui/src/conditions.ts`
- Test: `ui/src/conditions.test.ts` (vitest, alongside `policyGraph.test.ts`)

**Interfaces:**
- Produces (consumed by Task 7's component and tests):

```ts
export type SubjectKind =
  | 'header' | 'query' | 'cookie' | 'var'
  | 'jsonpath-request' | 'jsonpath-response';
export type ValueType = 'string' | 'number' | 'boolean';
export interface ConditionRule {
  kind: 'rule';
  subject: SubjectKind;
  name: string;          // header/query/cookie/var name, or the JSONPath ($...)
  negate: boolean;
  op: string;
  value: string;         // scalar-op operand (as typed)
  valueType: ValueType;  // how `value` serializes for JSONPath subjects
  values: string[];      // operand for in / ipmatch
}
export interface ConditionGroup {
  kind: 'group';
  logic: 'AND' | 'OR';
  negate: boolean;
  children: ConditionNode[];
}
export type ConditionNode = ConditionRule | ConditionGroup;

export const UNARY_OPS: readonly string[]; // ['present','absent','is_null']
export const LIST_OPS: readonly string[];  // ['in','ipmatch']
/** Operators valid for the subject kind (is_null only for jsonpath-*). */
export function opsFor(subject: SubjectKind): string[];
/** Model -> triple-array. Root group serializes as the top-level rule list. */
export function toExpr(root: ConditionGroup): unknown[];
/** Triple-array -> model; null when not representable in the builder. */
export function fromExpr(v: unknown): ConditionGroup | null;
/** For fault-injection-style `vars`: OR-of-expressions <-> root OR group of AND groups. */
export function toVarsList(root: ConditionGroup): unknown[];
export function fromVarsList(v: unknown): ConditionGroup | null;
/** Fresh empty models. */
export function emptyExpr(): ConditionGroup;      // {kind:'group',logic:'AND',negate:false,children:[]}
export function emptyVarsList(): ConditionGroup;  // {kind:'group',logic:'OR',negate:false,children:[]}
```

**Serialization rules (implement exactly):**
- Subject mapping: `header` → `http_` + name lowercased with `-`→`_`; `query` → `arg_` + name; `cookie` → `cookie_` + name; `var` → name verbatim; `jsonpath-request` → name verbatim (starts with `$`); `jsonpath-response` → `response_body:` + name.
- Reverse mapping in `fromExpr`: `response_body:` prefix → `jsonpath-response`; `request_body:` prefix → `jsonpath-request` (strip prefix); leading `$` → `jsonpath-request`; `http_` → `header` (strip, `_`→`-`); `arg_` → `query`; `cookie_` → `cookie`; else `var`.
- Rule → array: `[subject]`, then `'!'` if `negate`, then `op`; unary ops stop there; list ops append `values` (as strings); scalar ops append `value` coerced by `valueType` (`number` → `Number(value)`, `boolean` → `value === 'true'`, else the string). A `number` coercion producing `NaN` serializes the raw string instead (round-trip safe).
- Array → rule: numbers → `valueType:'number'`, `value:String(n)`; booleans → `'boolean'`; strings → `'string'`. Any non-scalar operand where a scalar is expected → return `null` (unrepresentable).
- Group → array: `['AND'|'OR', ...children]`; if `negate`, wrap: `['NOT', inner]` where `inner` is the child itself when the group is a single-child AND (so `["NOT", <rule>]` round-trips), else the `['AND'|'OR', ...]` array.
- `toExpr(root)`: root with `logic:'AND'`, `negate:false` → plain array of serialized children (the implicit top-level AND). Otherwise → `[serializeGroup(root)]`.
- `fromExpr(v)`: `v` must be an array; each element parses as rule or group (`AND`/`OR`/`NOT` head, case-insensitive; `NOT` with one child → the parsed child wrapped in / marked as a negated group — for `["NOT", <rule>]` produce `{kind:'group', logic:'AND', negate:true, children:[rule]}`). Result is the root AND group. Anything unrecognized → `null`.
- `toVarsList` / `fromVarsList`: the list `[expr, expr, ...]` maps to a root OR group whose children are AND groups (one per expr, each expr's rules as children). `fromVarsList` returns `null` if any element is not an array-of-rule-arrays.

- [ ] **Step 1: Write the failing tests**

Create `ui/src/conditions.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import {
  type ConditionGroup,
  emptyExpr,
  fromExpr,
  fromVarsList,
  opsFor,
  toExpr,
  toVarsList,
} from './conditions';

const rule = (over: Partial<import('./conditions').ConditionRule>) => ({
  kind: 'rule' as const,
  subject: 'header' as const,
  name: '',
  negate: false,
  op: '==',
  value: '',
  valueType: 'string' as const,
  values: [],
  ...over,
});

describe('conditions serialization', () => {
  it('serializes header/query/cookie/var/jsonpath subjects', () => {
    const root: ConditionGroup = {
      ...emptyExpr(),
      children: [
        rule({ subject: 'header', name: 'X-Api-Key', op: 'present' }),
        rule({ subject: 'query', name: 'page', op: '>', value: '1', valueType: 'number' }),
        rule({ subject: 'cookie', name: 'session', op: 'present' }),
        rule({ subject: 'var', name: 'remote_addr', op: 'ipmatch', values: ['10.0.0.0/8'] }),
        rule({ subject: 'jsonpath-request', name: '$.user.id', op: 'is_null', negate: true }),
        rule({ subject: 'jsonpath-response', name: '$.ok', op: '==', value: 'true', valueType: 'boolean' }),
      ],
    };
    expect(toExpr(root)).toEqual([
      ['http_x_api_key', 'present'],
      ['arg_page', '>', 1],
      ['cookie_session', 'present'],
      ['remote_addr', 'ipmatch', ['10.0.0.0/8']],
      ['$.user.id', '!', 'is_null'],
      ['response_body:$.ok', '==', true],
    ]);
  });

  it('round-trips builder models', () => {
    const root: ConditionGroup = {
      ...emptyExpr(),
      children: [
        rule({ subject: 'header', name: 'authorization', op: 'contains', value: 'Bearer' }),
        {
          kind: 'group',
          logic: 'OR',
          negate: false,
          children: [
            rule({ subject: 'jsonpath-request', name: '$.user.email', op: 'present' }),
            {
              kind: 'group',
              logic: 'AND',
              negate: true,
              children: [rule({ subject: 'jsonpath-request', name: '$.user.id', op: 'is_null' })],
            },
          ],
        },
      ],
    };
    expect(fromExpr(toExpr(root))).toEqual(root);
  });

  it('round-trips hand-authored expressions', () => {
    const exprs: unknown[] = [
      [['http_authorization', 'present']],
      [['arg_name', '==', 'jack'], ['OR', ['$.a', 'present'], ['NOT', ['$.b', 'is_null']]]],
      [['$.items[*].price', '!', '<=', 0]],
      [['remote_addr', 'ipmatch', ['10.0.0.0/8', '192.168.1.1']]],
    ];
    for (const e of exprs) {
      const model = fromExpr(e);
      expect(model, JSON.stringify(e)).not.toBeNull();
      expect(toExpr(model!)).toEqual(e);
    }
  });

  it('returns null for unrepresentable expressions', () => {
    expect(fromExpr('not an array')).toBeNull();
    expect(fromExpr([['http_a', '==', { nested: 'object' }]])).toBeNull();
    expect(fromExpr([['NOT']])).toBeNull();
    expect(fromExpr([[42, '==', 'x']])).toBeNull();
  });

  it('maps vars lists to an OR root of AND groups and back', () => {
    const varsList = [
      [['arg_name', '==', 'jack'], ['http_x', 'present']],
      [['arg_name', '==', 'rose']],
    ];
    const model = fromVarsList(varsList);
    expect(model).not.toBeNull();
    expect(model!.logic).toBe('OR');
    expect(model!.children).toHaveLength(2);
    expect(toVarsList(model!)).toEqual(varsList);
  });

  it('filters operators by subject kind', () => {
    expect(opsFor('header')).not.toContain('is_null');
    expect(opsFor('jsonpath-request')).toContain('is_null');
    expect(opsFor('var')).toContain('ipmatch');
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ui && npx vitest run src/conditions.test.ts`
Expected: FAIL ("Cannot find module './conditions'").

- [ ] **Step 3: Implement `ui/src/conditions.ts`**

Implement per the interface + serialization rules above. Full module doc comment at top (mirror the style of `policyGraph.ts`), noting it is the TypeScript counterpart of `src/vars/mod.rs::Expr::parse`. Keep it a pure module: no React imports.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ui && npx vitest run src/conditions.test.ts` — Expected: PASS.
Also: `cd ui && npm test` — Expected: all UI tests PASS.

- [ ] **Step 5: Commit**

```bash
git add ui/src/conditions.ts ui/src/conditions.test.ts
git commit -m "feat(ui): condition-expression model and triple-array serialization"
```

---

### Task 7: ConditionBuilder component + `conditions` field type + request-validation wiring

**Files:**
- Create: `ui/src/components/ConditionBuilder.tsx`
- Modify: `ui/src/pluginConfig.ts` (`FieldType` union, `FieldSchema.shape`, `request-validation` entry)
- Modify: `ui/src/components/SchemaForm.tsx` (render `conditions` fields)

**Interfaces:**
- Consumes: everything Task 6 exports.
- Produces:

```ts
// ConditionBuilder.tsx
export function ConditionBuilder(props: {
  value: unknown;                       // triple-array (shape 'expr') or list of them ('or-of-exprs')
  shape: 'expr' | 'or-of-exprs';
  onChange: (v: unknown) => void;
}): JSX.Element;
```

- `pluginConfig.ts`: `FieldType` gains `'conditions'`; `FieldSchema` gains `shape?: 'expr' | 'or-of-exprs'` (only meaningful on `conditions` fields). `conditions` fields are structurally `template: 'none'` (never `VarInput`).

- [ ] **Step 1: Implement the ConditionBuilder component**

Create `ui/src/components/ConditionBuilder.tsx`. Requirements (follow the visual style of SchemaForm.tsx — reuse its patterns: `inputStyle`-like inline styles, dashed AddButton, X RemoveButton, segmented RadioGroup-style toggles):

- **State:** `mode: 'builder' | 'raw'`. On first render, parse `value` with `fromExpr` / `fromVarsList` (per `shape`); empty/undefined value → `emptyExpr()` / `emptyVarsList()`; parse failure → start in `raw` mode. Keep the parsed model in component state; every edit calls `onChange(toExpr(model))` (or `toVarsList`).
- **Group card:** bordered card (`--surface-sunken`, like `objects` rows) with: AND/OR segmented toggle (hidden for the root of `or-of-exprs`, which is fixed OR, and its direct children, fixed AND — label them "Condition set N" instead), NOT toggle (small button, active style like RadioGroup's active state; hidden on the root of shape `'expr'` — the top level is the implicit AND), "Add rule" and "Add group" dashed buttons, X remove (not on root).
- **Rule row:** one flex row (wraps on narrow width) with:
  - subject `<select>`: Header / Query param / Cookie / Context var / JSONPath (request body) / JSONPath (response body) → `SubjectKind`.
  - name input (mono, placeholder per subject: `authorization`, `page`, `session`, `remote_addr`, `$.user.name`, `$.data.id`).
  - NOT toggle button (`!`), aria-label "Negate rule".
  - operator `<select>` from `opsFor(rule.subject)`; if the current op becomes invalid after a subject change, reset to `'=='`.
  - value input: hidden when `UNARY_OPS` contains op; when `LIST_OPS` contains op, a compact add/remove string list (same pattern as SchemaForm's `list` type); otherwise a text input plus — only for `jsonpath-*` subjects and ops `==`/`!=`/`in`/`has`/`contains` — a small `valueType` select (`str`/`num`/`bool`).
  - X remove button.
- **Raw mode:** monospace textarea seeded with `JSON.stringify(value ?? [], null, 2)`, Apply button that `JSON.parse`s (array required; inline "Invalid JSON" error on failure, mirroring NodeInspector's JsonConfigEditor), then re-attempts `fromExpr`/`fromVarsList` — success returns to builder mode, failure stays raw but still `onChange`s the parsed JSON (valid config the builder just can't display).
- **Mode toggle:** a small text button "Edit as JSON" / "Edit visually" above the card. Switching to visual is disabled (with hint text "expression not representable in the builder") when the current value doesn't parse into a model.
- **Empty state:** shape `'expr'` root with no children shows the Add rule / Add group buttons only (serializes to `[]`; note: the Rust side treats `[]` as vacuously true — the field is optional, an empty conditions array should be treated by the form as "unset": `onChange(undefined)` when the root has no children, so the key is dropped).

- [ ] **Step 2: Register the field type**

`ui/src/pluginConfig.ts`:
- `FieldType` union: add `| 'conditions'`.
- `FieldSchema`: add

```ts
  /** For `conditions` fields: single expression or an OR-ed list of them. */
  shape?: 'expr' | 'or-of-exprs';
```

- `request-validation` entry: append

```ts
    { key: 'conditions', label: 'Conditions', type: 'conditions', shape: 'expr',
      hint: 'boolean predicates over headers, vars, and JSONPath body queries; all must hold or the request is rejected' },
```

`ui/src/components/SchemaForm.tsx` — in `renderField`'s switch:

```tsx
      case 'conditions':
        return (
          <ConditionBuilder
            value={current}
            shape={field.shape ?? 'expr'}
            onChange={(v) => set(field.key, v)}
          />
        );
```

with `import { ConditionBuilder } from './ConditionBuilder';`. Update SchemaForm's doc comment (the per-field rendering list) with the new type. Also update `resolveTemplateMode`'s doc note only if it mentions an exhaustive type list.

- [ ] **Step 3: Type-check and run UI tests**

Run: `cd ui && npx tsc --noEmit && npm test`
Expected: clean type-check, all tests PASS.

- [ ] **Step 4: Manual smoke check in the dev UI**

Run the gateway with the UI enabled (`cargo run` with default config, or per `run` skill) and open the editor; add a `request-validation` node; verify: builder renders, a header-present + contains rule saves, reload round-trips, raw-JSON toggle shows the triple-array. (No commit gate on this — it's a sanity pass; the e2e task automates it.)

- [ ] **Step 5: Commit**

```bash
git add ui/src/components/ConditionBuilder.tsx ui/src/components/SchemaForm.tsx ui/src/pluginConfig.ts
git commit -m "feat(ui): ConditionBuilder control and request-validation conditions field"
```

---

### Task 8: `object` field type; wire fault-injection and response-rewrite

Today fault-injection's `abort`/`delay` render as JSON **textareas** that serialize strings (which the Rust plugin rejects — it requires objects), and response-rewrite's `vars` isn't in the UI at all. Replace with a structured single-object card type; nested `vars` become ConditionBuilder fields.

**Files:**
- Modify: `ui/src/pluginConfig.ts` (`FieldType` union, `fault-injection` + `response-rewrite` entries)
- Modify: `ui/src/components/SchemaForm.tsx` (render `object` fields via recursive `SchemaForm`)

**Interfaces:**
- Consumes: `ConditionBuilder` + `shape` from Task 7.
- Produces: `FieldType` gains `'object'` — an optional single nested record: unset → dashed "Add <itemLabel>" button; set → a card with an X remove (stores `undefined`) rendering `field.fields` through a nested `<SchemaForm>`. Unknown keys inside the object (e.g. fault-injection `abort.headers`) survive round-trips because SchemaForm spreads the existing value.

- [ ] **Step 1: Implement the `object` case in SchemaForm**

`FieldType` union in pluginConfig.ts: add `| 'object'`. In SchemaForm's `renderField`:

```tsx
      case 'object': {
        const obj = (current ?? undefined) as Record<string, unknown> | undefined;
        if (obj === undefined) {
          return (
            <AddButton
              label={field.itemLabel ?? field.label}
              onClick={() => set(field.key, Object.fromEntries(
                (field.fields ?? [])
                  .filter((f) => f.default !== undefined)
                  .map((f) => [f.key, f.default])
              ))}
            />
          );
        }
        return (
          <div
            style={{
              padding: 10,
              borderRadius: 'var(--radius-sm)',
              border: '1px solid var(--border-subtle)',
              background: 'var(--surface-sunken)',
            }}
          >
            <div className="flex items-center justify-between" style={{ marginBottom: 6 }}>
              <span className="eyebrow">{field.itemLabel ?? field.label}</span>
              <RemoveButton
                label={`Remove ${field.itemLabel ?? field.label}`}
                onClick={() => set(field.key, undefined)}
              />
            </div>
            <SchemaForm
              schema={field.fields ?? []}
              value={obj}
              onChange={(v) => set(field.key, v)}
              varContext={varContext}
            />
          </div>
        );
      }
```

(Recursion is safe: `SchemaForm` is a plain controlled component.) Document the new type in the component's doc comment: "`object` — optional single nested record; absent renders an Add button, present renders sub-fields in a card and serializes as one object; unknown keys in the object are preserved."

- [ ] **Step 2: Replace the fault-injection entry**

In `pluginConfig.ts` (current entry at ~line 727 uses two JSON textareas — delete both):

```ts
  'fault-injection': [
    { key: 'abort', label: 'Abort', type: 'object', itemLabel: 'Abort rule',
      fields: [
        { key: 'http_status', label: 'HTTP status', type: 'number', default: 503 },
        { key: 'body', label: 'Body', type: 'text', placeholder: 'injected fault', template: 'full', legacyDollar: true },
        { key: 'percentage', label: 'Percentage', type: 'number', hint: '0-100; empty = always' },
        { key: 'vars', label: 'Conditions', type: 'conditions', shape: 'or-of-exprs',
          hint: 'condition sets are OR-ed; the abort only triggers when one matches (headers stay YAML-only)' },
      ] },
    { key: 'delay', label: 'Delay', type: 'object', itemLabel: 'Delay rule',
      fields: [
        { key: 'duration', label: 'Duration (s)', type: 'number', default: 0.5 },
        { key: 'percentage', label: 'Percentage', type: 'number', hint: '0-100; empty = always' },
        { key: 'vars', label: 'Conditions', type: 'conditions', shape: 'or-of-exprs',
          hint: 'condition sets are OR-ed; the delay only applies when one matches' },
      ] },
  ],
```

Cross-check the sub-field keys and the `body` template mode against `src/plugins/native/fault_injection.rs::from_config` (`abort.http_status` required, `abort.body`/`abort.headers` templated, `percentage`, `vars`) before committing; `headers` is intentionally not in the form — it's preserved as an unknown key.

- [ ] **Step 3: Add response-rewrite `vars` and confirm its shape**

`response-rewrite` in Rust holds a **single** `Expr` (`src/plugins/native/response_rewrite.rs:52` — `vars: Option<vars::Expr>`), so:

```ts
    { key: 'vars', label: 'Conditions', type: 'conditions', shape: 'expr',
      hint: 'gate — the rewrite applies only when the conditions hold' },
```

appended to the `response-rewrite` entry (~line 699).

- [ ] **Step 4: Type-check, run UI tests**

Run: `cd ui && npx tsc --noEmit && npm test` — Expected: PASS.

- [ ] **Step 5: Manual smoke check**

In the dev UI: add a fault-injection node → "Add Abort rule" → set status 503, add an OR'd condition set with `arg_debug == 1`; save the policy; confirm the gateway accepts it (no compile error toast) and the saved YAML contains `abort: {http_status: 503, vars: [[["arg_debug", "==", "1"]]]}`-shaped config.

- [ ] **Step 6: Commit**

```bash
git add ui/src/pluginConfig.ts ui/src/components/SchemaForm.tsx
git commit -m "feat(ui): structured fault-injection/response-rewrite editors with condition builder"
```

---

### Task 9: E2E scenario

**Files:**
- Create: `e2e/tests/validation-conditions.spec.ts`
- Modify: `e2e/E2E_TESTBOOK.md` (add the scenarios to the catalog, matching its existing entry format)

**Interfaces:**
- Consumes: helpers in `e2e/helpers/admin.ts` (`adminApi`, `dataPlane`) — read that file first and mirror how `data-plane.spec.ts` and `editor.spec.ts` create routes/policies and drive the browser.

- [ ] **Step 1: Study conventions**

Read `e2e/helpers/admin.ts`, `e2e/tests/data-plane.spec.ts`, `e2e/tests/editor.spec.ts`, and the E2E_TESTBOOK.md header to copy the ID scheme (e.g. `E2E-VC-01`) and setup/teardown patterns (create policy + route via admin API in `beforeAll`, delete in `afterAll`).

- [ ] **Step 2: Write the data-plane scenarios (API-created policy)**

`e2e/tests/validation-conditions.spec.ts` — create via admin API a policy whose graph is `listener → validate (request-validation) → upstream (echo backend) → client`, with `validate.denied → client.in` wired, and config:

```ts
config: {
  rejected_code: 401,
  conditions: [
    ['http_authorization', 'present'],
    ['http_authorization', 'contains', 'Bearer'],
    ['OR', ['$.user.email', 'present'], ['NOT', ['$.user.id', 'is_null']]],
  ],
}
```

plus a route matching `POST /conditions/*`. Scenarios:

- `E2E-VC-01`: POST with `authorization: Bearer tok` and body `{"user":{"email":"a@b.c"}}` → 200, echo confirms the request reached the upstream.
- `E2E-VC-02`: POST without `authorization` → 401, body `{"error":"validation_failed","message":"request conditions not satisfied"}`.
- `E2E-VC-03`: POST with `authorization: Basic xyz` → 401 (contains fails).
- `E2E-VC-04`: POST with Bearer auth and body `{"user":{"id":null}}` → 401 (email absent AND id null → OR arm false).
- `E2E-VC-05`: POST with Bearer auth and body `{"user":{"id":7}}` → 200 (NOT is_null holds).

- [ ] **Step 3: Write the UI scenario**

- `E2E-VC-06`: open the editor on the policy from Step 2, select the `validate` node, assert the ConditionBuilder shows 3 top-level entries (2 rules + 1 OR group); change the `contains` rule's value from `Bearer` to `Token` via the builder's value input; save the policy; then via the data plane: `authorization: Bearer tok` → 401 and `authorization: Token tok` (+ passing body) → 200. Use the selector/save patterns from `editor.spec.ts` (add `aria-label`s or `data-testid`s to ConditionBuilder inputs in Task 7's component if the existing patterns need them — if you must add them, do it as part of this task and note it in the commit).

- [ ] **Step 4: Run the suite**

Run: `cargo build --release && cd e2e && npm test -- validation-conditions`
Expected: all E2E-VC-* scenarios PASS. Then run the full `npm test` to confirm nothing else regressed (the fault-injection/plugin-config specs touch the UI schema we changed).

- [ ] **Step 5: Commit**

```bash
git add e2e/tests/validation-conditions.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "test(e2e): validation-conditions scenarios (data plane + condition builder)"
```

---

### Task 10: Documentation + graph refresh

**Files:**
- Create: `website/docs/reference/conditions.md`
- Modify: `website/sidebars.ts` (add `'reference/conditions'` after `'reference/context-vars'`, line ~194)
- Modify: `website/docs/reference/plugins/request-validation.md` (document `conditions`, example, rejection message)
- Modify: `website/docs/reference/plugins/fault-injection.md` and `.../response-rewrite.md` if they exist (`ls website/docs/reference/plugins/`) — cross-link the conditions reference from their `vars` sections
- Modify: `docs/apisix-parity.md` (note the dialect extensions)
- Modify: `CLAUDE.md` — no change needed unless the "Not Yet Implemented" section claims otherwise (it doesn't)

- [ ] **Step 1: Write the conditions reference page**

`website/docs/reference/conditions.md` — content requirements (write real prose, follow the tone/frontmatter of `reference/context-vars.md`):
- The triple-array shape: rules, `!` negation, `AND`/`OR`, the `NOT` group (marked as a featherbit extension over APISIX's lua-resty-expr dialect).
- Subjects: var names (link to context-vars.md) and JSONPath (`$...` request body, `request_body:$...`, `response_body:$...`, RFC 9535).
- Operator table: all 15 operators with flat-var and JSONPath columns (behavior per the spec: any-match, native scalar equality, contains on strings vs arrays).
- Three-state semantics section with this exact truth table:

| body | `$.k present` | `$.k is_null` | `$.k absent` |
|---|---|---|---|
| `{}` | false | false | true |
| `{"k": null}` | true | true | false |
| `{"k": 1}` | true | false | false |

- ANY-match semantics with the `["$.items[*].price", "!", "<=", 0]` ALL-via-negation idiom.
- Which plugins consume conditions: request-validation (`conditions`), fault-injection / response-rewrite (`vars`), workflow / traffic-label (`case`/`match` — they share `Expr`; verify by grep before claiming, `grep -l "Expr::parse" src/plugins/native/`).

- [ ] **Step 2: Update the plugin pages and parity doc**

- request-validation.md: `conditions` key docs + the spec's YAML example + note "at least one of header_schema / body_schema / conditions".
- fault-injection.md / response-rewrite.md: link their `vars` docs to the new reference page and mention the new operators/subjects work there too.
- `docs/apisix-parity.md`: in the request-validation row/section, note the featherbit-only extensions: `present`/`absent`/`is_null`/`contains`, JSONPath subjects, `NOT` groups.

- [ ] **Step 3: Build the docs site**

Run: `cd website && npm run build`
Expected: build succeeds, no broken links (Docusaurus fails on broken internal links).

- [ ] **Step 4: Refresh the knowledge graph**

Run: `graphify update .` (per project CLAUDE.md, after code changes; if it fails on encoding, prefix with `PYTHONIOENCODING=utf-8`).

- [ ] **Step 5: Commit**

```bash
git add website/docs/reference/conditions.md website/sidebars.ts website/docs/reference/plugins/ docs/apisix-parity.md graphify-out
git commit -m "docs: condition-expression reference and plugin doc updates"
```

---

### Final verification (before requesting review/merge)

- [ ] `cargo test` — full Rust suite green.
- [ ] `cd ui && npx tsc --noEmit && npm test` — UI green.
- [ ] `cargo build --release && cd e2e && npm test` — full e2e green.
- [ ] `cd website && npm run build` — docs green.
- [ ] Use superpowers:requesting-code-review, then superpowers:finishing-a-development-branch (PR targets `develop` per gitflow).
