---
title: Conditions
description: The triple-array condition-expression dialect shared by request-validation conditions, fault-injection/response-rewrite vars, and workflow/traffic-label/traffic-split case/match — subjects, all 15 operators, JSONPath bodies, and three-state present/absent/is_null semantics.
---

Several plugins gate their behavior on a boolean condition written in APISIX's
`lua-resty-expr` triple-array dialect — the same shape `vars` used in APISIX's
`fault-injection`, `response-rewrite`, and traffic-control plugins. featherbit
implements this dialect once, in [`src/vars/mod.rs`](../concepts/context-object.md)
(`Expr::parse` / `Expr::eval`), and every consuming plugin shares it — including
two featherbit extensions over the APISIX original: `NOT` groups, and JSONPath
subjects with three new operators (`present`, `absent`, `is_null`) plus
`contains`. This page is the reference for the dialect itself; see
[Context vars](./context-vars.md) for the catalog of flat variable names a
condition's subject can name.

## Shape

A condition is a JSON/YAML array of rules. Rules in a top-level list are
**ANDed**:

```yaml
conditions:
  - ["arg_name", "==", "jack"]
  - ["http_user_agent", "~~", "Mozilla.+"]
```

A rule is one of:

- **`[subject, op, value]`** — the common case.
- **`[subject, "!", op, value]`** — per-rule negation: the operator's result
  is inverted. `!` sits immediately before the operator, not before the
  subject.
- **`[subject, "present" | "absent" | "is_null"]`** — unary rules; these three
  operators take no value.

Nested logic replaces a rule with a group:

- **`["AND", rule, rule, ...]`** / **`["OR", rule, rule, ...]`** — the same
  AND/OR semantics as the top-level list, usable anywhere a rule is expected
  (so they nest).
- **`["NOT", rule-or-group]`** — inverts one child. **This is a featherbit
  extension over APISIX's dialect** — APISIX's `lua-resty-expr` has no `NOT`.
  `NOT` takes **exactly one** child (a rule or another group); parsing fails
  if it has zero or more than one.

`AND`/`OR`/`NOT` are matched case-insensitively.

```yaml
conditions:
  - ["http_authorization", "present"]
  - ["http_authorization", "contains", "Bearer"]
  - - "OR"
    - ["$.user.email", "present"]
    - ["NOT", ["$.user.id", "is_null"]]
```

## Subjects

A rule's subject (the first array element) is either:

- **A context variable name** — anything [`src/vars/mod.rs`](./context-vars.md)
  resolves: `uri`, `arg_<name>`, `http_<name>`, `remote_addr`, and the rest of
  the [context-vars catalog](./context-vars.md). Resolution happens without
  the `$` prefix used in `$var` templates — write `"arg_name"`, not
  `"$arg_name"`.
- **A JSONPath query over a request or response body**, recognized by a `$`
  prefix or a `request_body:`/`response_body:` prefix:
  - **`$.user.name`** — RFC 9535 JSONPath queried against the **request**
    body (the default target).
  - **`request_body:$.user.name`** — explicit request-body target, same
    default as the bare `$...` form.
  - **`response_body:$.status`** — queried against the **response** body
    (only meaningful in response-phase plugins, e.g. `response-rewrite`).

  Paths are parsed with [`serde_json_path`](https://docs.rs/serde_json_path),
  a full RFC 9535 implementation, so filter expressions, slices, and
  wildcards (`$.items[*].price`, `$.items[?@.price > 10]`) all work. Paths
  are **compiled once at policy-compile time** — a malformed JSONPath fails
  policy compilation, never a live request.

  A body is parsed as JSON **lazily and once per evaluation**: the first rule
  that needs the request (or response) body parses it and every other rule
  in the same `eval()` call reuses the result. An **empty body or a body that
  isn't valid JSON** parses to nothing, and every JSONPath rule against it
  matches **zero nodes** — it does not error, and it does not throw. What
  zero nodes means for a given operator is exactly the three-state semantics
  below.

> **Migration note:** subjects beginning with `$`, `request_body:`, or
> `response_body:`, and rules whose subject string is literally `not` (matched
> case-insensitively, same as the `AND`/`OR`/`NOT` group heads), are now
> claimed by this extended dialect. A config that previously used one of these
> strings as a flat context-var name will now fail policy compilation instead
> of resolving that var.

## Operators

All 15 operators, and how each behaves against a flat-var subject (a single
string, or absent) versus a JSONPath subject (a set of zero or more matched
JSON nodes, evaluated with **ANY-match** — see [below](#any-match-and-the-all-via-negation-idiom)):

| Operator | Flat var | JSONPath |
|---|---|---|
| `==` | String-equals the value (numbers/bools stringified for comparison) | **Native scalar equality** — string↔string, number↔number, bool↔bool; a JSON `null`, array, or object never equals a scalar |
| `~=` (alias `!=`) | Negation of `==` | Negation of `==` |
| `>` | Both sides parsed as `f64`; false if the var doesn't parse as a number | `true` if any matched node is a JSON number `>` the value |
| `>=` | Same, `>=` | Same, `>=` |
| `<` | Same, `<` | Same, `<` |
| `<=` | Same, `<=` | Same, `<=` |
| `~~` | Regex match (compiled at config load) | Regex match against string nodes only (non-string nodes never match) |
| `~*` | Case-insensitive regex (`(?i)` prefix) | Same, case-insensitive |
| `in` | True if the var string-equals any item in the value array | True if any matched node scalar-equals any item in the value array |
| `has` | Treats the var as a **comma-separated list**; true if any comma-separated part equals the value | Treats the matched **node** as a JSON array; true if any array element scalar-equals the value |
| `ipmatch` | Var parsed as an IP address, checked against one or more CIDR/IP values; false if unparsable or absent | Same, but the address comes from a string-typed matched node |
| `present` | True unless the var is absent (unknown name, or a known family member missing on this request) — an empty-but-set header still counts as present | True if the JSONPath match set is non-empty |
| `absent` | True if the var is absent | True if the JSONPath match set is empty |
| `is_null` | **JSONPath-only** — using `is_null` on a flat-var subject is rejected at config load ("headers and vars cannot be null (use 'absent')") | True if any matched node is JSON `null` |
| `contains` | Substring containment (`value.contains(needle)`) | String node: substring containment. Array node: element equality (native scalar equality, not stringified) against the value. Any other node type: false |

Notes:

- `==`, `~=`/`!=`, `has`, and `contains` require a **scalar** value (string,
  number, or bool) in config — arrays/objects there fail at config load.
- `in` and `ipmatch` require an **array** value; `ipmatch` entries must each
  parse as an IP address or CIDR.
- A flat var that resolves to `None` (absent) behaves as if it were the empty
  string for every binary operator except `ipmatch` (always false when
  absent) and `present`/`absent` themselves.

## Three-state semantics: `present`, `absent`, `is_null`

A flat context var has only two states — present or absent — because nothing
in a header or query string can be JSON `null`. A JSONPath subject over a
real JSON document has three, and the three unary operators exist to tell
them apart. For request body `{"k": null}` versus `{}` versus `{"k": 1}`:

| body | `$.k present` | `$.k is_null` | `$.k absent` |
|---|---|---|---|
| `{}` | false | false | true |
| `{"k": null}` | true | true | false |
| `{"k": 1}` | true | false | false |

Read it as: `present` asks "did the path match a node at all" (a `null`
value still counts as a match), `is_null` asks "is the matched node's value
JSON `null`", and `absent` is the negation of `present`. This is why
`is_null` is rejected for flat-var subjects — a header or query param is
either there (a string, possibly empty) or it isn't; there's no third state
to name, so `absent` already says everything `is_null` would.

## ANY-match and the ALL-via-negation idiom

A JSONPath query can match more than one node — `$.items[*].price` matches
one node per array element. Every binary and comparison operator (`==`,
`>`, `~~`, `in`, `has`, `ipmatch`, `contains`, ...) evaluates with
**ANY-match semantics**: the rule is true if **at least one** matched node
satisfies the operator. `["$.items[*].price", "==", 0]` is true if *any*
item's price is `0`, not all of them.

To express **ALL** matched nodes must satisfy a predicate, negate the rule
that expresses the opposite of what you want, using the per-rule `!`. Because
negation is applied to the ANY-reduced result — `NOT (any node matches)` —
this is De Morgan's law in one rule: negating "any node fails predicate P"
gives "every node satisfies P":

```yaml
# ALL items have a positive price:
# NOT (any item's price <= 0)  ==  every item's price > 0
conditions:
  - ["$.items[*].price", "!", "<=", 0]
```

The unary set operators (`present`, `absent`, `is_null`) are already
evaluated over the whole match set rather than per-node (see the table
above), so this idiom is specific to the value-comparing operators.

## Which plugins evaluate conditions

All of the following parse and evaluate the same `Expr` type
(`crate::vars::Expr`) — a condition written for one works, unchanged, in any
other:

| Plugin | Config key | Shape |
|---|---|---|
| [`request-validation`](./plugins/request-validation.md) | `conditions` | One expression (the top-level list is itself ANDed); evaluated after schema validation, rejects through `denied` |
| [`fault-injection`](./plugins/fault-injection.md) | `abort.vars` / `delay.vars` | An **array of expressions, OR-ed across items, AND-ed within each item** — a flat single-expression list is also accepted as a convenience |
| [`response-rewrite`](./plugins/response-rewrite.md) | `vars` | One expression; gates the whole node — false means an untouched passthrough |
| [`workflow`](./plugins/workflow.md) | `rules[].case` | One expression per rule; first matching rule's action applies |
| [`traffic-label`](./plugins/traffic-label.md) | `rules[].match` | One expression per rule; first matching rule's action applies |
| [`traffic-split`](./plugins/traffic-split.md) | `rules[].match` | One expression per rule; first matching rule selects the weighted upstream set |

The web UI's `ConditionBuilder` (a visual AND/OR/NOT tree of subject/op/value
rows, with a raw-JSON fallback for shapes it can't represent) is the schema
form for `request-validation`'s `conditions`, `fault-injection`'s
`abort.vars`/`delay.vars`, and `response-rewrite`'s `vars`. The
`workflow`/`traffic-label`/`traffic-split` `rules` keys are still edited as
raw JSON textareas — those plugins' `rules[].case`/`rules[].match` conditions
are only one part of a larger structure (actions, weights, ...) that has no
schema-form editor yet.
