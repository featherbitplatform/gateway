---
title: Supernodes
description: Reusable named subgraphs — defined once, referenced from any policy by a single node, and inlined at compile time.
---

A **supernode** is a reusable, named subgraph — its own nodes and edges — stored top-level in `gateway.yaml` under `supernodes:` and usable from any [policy](policies-and-graphs.md) as a single node. Where a policy graph is per-route, a supernode is shared: an auth-then-upstream-then-error-handling pattern that would otherwise get copy-pasted across policies (and drift apart over time) can be written once and referenced everywhere. Editing the definition updates every policy that uses it — true reuse, not copy-paste.

## Boundary nodes

A supernode definition declares exactly three structural nodes, the same way a policy declares `listener`/`client`:

| Node | `id` | Ports |
|---|---|---|
| `input` | must be `input` | one `out` edge only |
| `output` | must be `output` | `in` only (fan-in allowed) |
| `error` | must be `error` | `in` only (fan-in allowed) |

`input` is where the context enters when the supernode instance runs; `output` and `error` are its two exits, corresponding to the instance's own `success` and `error` ports once it's dropped into a policy.

## Defining a supernode

```yaml
supernodes:
  - name: secured-call
    description: "Key auth + upstream with unified error exit"
    nodes:
      - { id: input,  type: input }     # boundary nodes declared like listener/client,
      - { id: output, type: output }    # so the UI can persist canvas positions
      - { id: error,  type: error }
      - { id: auth,   type: key-auth }
      - { id: up,     type: upstream, config: { targets: [ { host: "svc", port: 80 } ] } }
    edges:
      - { from: input.out,    to: auth.in }
      - { from: auth.success, to: up.in }
      - { from: auth.error,   to: error.in }
      - { from: up.success,   to: output.in }
```

A definition doesn't have to do anything: wiring `input.out` straight to `output.in` is a valid, if trivial, supernode — it's what the web UI's library editor seeds a brand-new definition with, ready to be built out on the canvas.

## Using a supernode from a policy

Inside a policy, an instance is a plain node with `type: supernode`, referencing the definition by name:

```yaml
- { id: sec, type: supernode, config: { name: secured-call } }
# edges wire sec.in / sec.success / sec.error like any other node
```

From the policy's point of view `sec` behaves like any other node — it has one input port and `success`/`error` output ports — regardless of how many nodes the definition contains internally.

### An instance exposes exactly two exits

`success` (alias `out`, the `output` boundary) and `error` (the `error` boundary) — nothing else. An instance never exposes an [outcome port](policies-and-graphs.md#outcome-ports-and-the-mandatory-wiring-rule): those belong to *inner* nodes, and the definition must route them to a boundary itself. An edge leaving the instance on any other port name is rejected, as is a second edge from either exit:

```
policy 'p': unknown port 'denied' on supernode instance 'sec' — instances expose success (alias out) and error
```

Correspondingly, **every `success`/outcome port of every inner node must be wired inside the definition** — to another inner node, or to the `output`/`error` boundary. This is checked when you save the definition, so the error names the definition rather than surfacing later as a puzzling compile failure on whichever policy happens to instantiate it. All such violations are reported together. Inner `error` ports stay exempt — the black-box rule below covers them.

## Black-box error routing

A supernode instance exposes a single `error` port. `auth.error -> error.in` above is an explicit edge the definition itself wires — nothing implicit about it. The implicit case is `up`: its `error` port is left unwired inside the definition entirely. At expansion time, any inner node with no error edge of its own gets an implicit edge straight to wherever the policy connected the instance's `error` port — but only if the policy wired that port. If it didn't, those unhandled inner errors aren't silently swallowed; they fall through to the policy's `error_handler`, or a 500 if there isn't one. Either way, the policy wiring the instance only ever sees one error exit, no matter how many inner nodes could fail.

## Compile-time expansion

Supernode instances are macro-expanded, not executed as a nested engine: before a policy is compiled, every instance is inlined into the flat node graph the engine already knows how to run — engine, metrics, and trace code are unmodified. Each inner node `n` from an instance `sec` becomes `sec/n` in the compiled graph, and inner edges are rewired with that prefix on both ends. Outer edges into the instance's `in`, `success`, and `error` ports are spliced onto the corresponding inlined boundary edges.

This is visible, by design, wherever node ids show up:

- **Metrics** carry labels like `node_id="sec/auth"`.
- **Debug traces** record each inner step individually, grouped under the instance id.
- **Error records** (`GatewayError.node_id`) show exactly which inner node failed, so an `error-handler` template can report `sec/auth` rather than a generic instance failure.

Expansion happens fresh on every compile; the stored config in `gateway.yaml`, the Admin API, and etcd all keep the compact `type: supernode` reference form. The expanded graph is never persisted.

## V1 limits

- **No parameters.** A supernode definition is fixed; the only per-environment variation is `${VAR}` interpolation, which still works inside a definition the same as anywhere else in `gateway.yaml`.
- **No nesting.** A supernode cannot reference another supernode. Multiple instances of the same definition are fine — each gets its own namespace — but the definitions themselves are flat.
- Node ids inside a definition may not contain `/` and may not reuse the reserved boundary ids (`input`/`output`/`error`) for anything other than the boundary nodes themselves.

## Export and seeding

`SupernodeConfig` is part of `GatewayConfig`, so supernodes travel with the rest of the config wherever it does: `GET /api/config/export` (see the [Admin API guide](../guides/admin-api.md)) includes a `supernodes:` section alongside `routes:` and `policies:`, and a `gateway.yaml` that has one seeds it into a fresh instance or etcd cluster on first load — no separate export/import path to remember.
