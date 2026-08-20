---
title: Supernodes
description: Reusable named subgraphs — defined once, referenced from any policy by a single node, and inlined at compile time.
---

import UiShot from '@site/src/components/UiShot';

A **supernode** is a reusable, named subgraph — its own nodes and edges — stored top-level in `gateway.yaml` under `supernodes:` and usable from any [policy](policies-and-graphs.md) as a single node. Where a policy graph is per-route, a supernode is shared: an auth-then-upstream-then-error-handling pattern that would otherwise get copy-pasted across policies (and drift apart over time) can be written once and referenced everywhere. Editing the definition updates every policy that uses it — true reuse, not copy-paste.

## Boundary nodes

A supernode definition declares three kinds of structural node, the same way a policy declares `listener`/`client`: exactly one `input`, exactly one `error`, and **one or more** `type: output` nodes.

| Node | `id` | Ports |
|---|---|---|
| `input` | must be `input` | one `out` edge only |
| `output` (one or more) | each id becomes an instance port name — see [Named output ports](#named-output-ports) | `in` only (fan-in allowed) |
| `error` | must be `error` | `in` only (fan-in allowed) |

`input` is where the context enters when the supernode instance runs; each `output` boundary and the `error` boundary are its exits, corresponding to the instance's own output ports once it's dropped into a policy.

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

## Named output ports

A definition may declare any number of `type: output` boundary nodes, not just the one named `output`. Each output node's **id is the port name** the instance exposes — with one backward-compatible exception: the output node with id `output` maps to the instance's `success` port (alias `out`), exactly as before. `input` and `error` stay singular (exactly one node of each, id equal to the type); output ids must be unique and must not collide with the reserved names `input`, `error`, `in`, `out`, `success`. A definition needs at least one output boundary, but it doesn't need one with id `output` — a definition with only named outputs and no `output`-id node is legal, and its instances simply have no `success` port.

This lets a supernode expose a rejection path as its own exit instead of forcing every non-error outcome through a single `success` port. An auth check wrapped in a supernode, for example, can give its instance both a `success` and a `denied` exit:

```yaml
supernodes:
  - name: auth-gate
    nodes:
      - { id: input,   type: input }
      - { id: output,  type: output }   # -> instance port `success` (alias `out`)
      - { id: denied,  type: output }   # -> instance port `denied`
      - { id: error,   type: error }
      - { id: auth,    type: key-auth }
    edges:
      - { from: input.out,    to: auth.in }
      - { from: auth.success, to: output.in }
      - { from: auth.denied,  to: denied.in }

policies:
  - name: p
    nodes:
      - { id: gate, type: supernode, config: { name: auth-gate } }
      # ... listener, upstream, reject, client ...
    edges:
      - { from: gate.success, to: upstream.in }
      - { from: gate.denied,  to: reject.in }
```

Every output-derived instance port is **mandatory-wired**, the same rule plugin outcome ports follow: a policy that instantiates `auth-gate` but leaves `gate.denied` unwired fails to compile with

```
policy 'p': output port 'denied' of supernode instance 'gate' must be wired — add an edge from 'gate.denied'
```

`error` is the one exception and stays optional, as it always has — see [black-box error routing](#black-box-error-routing) below.

**Breaking change:** for a definition whose `output` boundary is unreachable — e.g. a pass-through `input.out -> error.in` with an orphaned `output` node — policies instantiating it used to compile with the instance's `success` port left unwired; they now must wire the instance's `success` port like any other mandatory port.

## Using a supernode from a policy

Inside a policy, an instance is a plain node with `type: supernode`, referencing the definition by name:

```yaml
- { id: sec, type: supernode, config: { name: secured-call } }
# edges wire sec.in / sec.success / sec.error like any other node
```

From the policy's point of view `sec` behaves like any other node — it has one input port and one output port per output boundary in the definition (`success`/`error` for a definition with the default single `output` boundary, more for a definition using [named output ports](#named-output-ports)) — regardless of how many nodes the definition contains internally.

### Previewing an instance in the editor

In the [web UI](../guides/web-ui.md), a supernode instance on the policy canvas carries an expand chevron in its header. Clicking it grows the node in place into a zoomed-out, **read-only** preview of the definition's inner graph — boundary nodes included — floating above its neighbors with the instance's edges still attached; clicking again folds it back. The preview is a glance, not an editor: nothing inside is clickable or draggable, and editing still happens in the definition's own canvas (the Supernodes section of the sidebar). Expansion is per-session editor state — it is never written into the policy, mirroring how [expansion is never persisted](#compile-time-expansion) on the gateway side. If the instance references a definition that no longer exists (deleted from the library while an unsaved instance still points at it), the preview shows an inline `supernode '<name>' not found` message instead of a graph.

<UiShot
  name="supernode-preview"
  alt="A supernode instance expanded in place on the policy canvas, showing a zoomed-out read-only preview of its inner graph below the node's ports."
  caption="An expanded instance. The inner graph — boundary nodes included — renders zoomed out below the instance's own ports; its edges into the policy stay attached, and folding restores the compact node."
/>

### An instance's exits are exactly its definition's boundaries

An instance exposes one outer port per `type: output` boundary in its definition (`success`, alias `out`, for the `output`-id boundary; the id itself for any other, per [named output ports](#named-output-ports) above), plus `error` — nothing else. An instance never exposes an [outcome port](policies-and-graphs.md#outcome-ports-and-the-mandatory-wiring-rule) directly: those belong to *inner* nodes, and the definition must route them to a boundary itself. An edge leaving the instance on a port its definition doesn't derive is rejected, as is a second edge from any one exit:

```
policy 'p': unknown port 'denied' on supernode instance 'sec' — supernode 'secured-call' exposes: success, error
```

(that message assumes `sec` references a definition with only the default `output`/`error` boundaries; a definition that also declares a `denied` output boundary makes `sec.denied` valid — and, per the mandatory-wiring rule above, required.)

Correspondingly, **every `success`/outcome port of every inner node must be wired inside the definition** — to another inner node, or to one of its output boundaries or the `error` boundary. This is checked when you save the definition, so the error names the definition rather than surfacing later as a puzzling compile failure on whichever policy happens to instantiate it. All such violations are reported together. Inner `error` ports stay exempt — the black-box rule below covers them.

## Black-box error routing

A supernode instance exposes a single `error` port. `auth.error -> error.in` above is an explicit edge the definition itself wires — nothing implicit about it. The implicit case is `up`: its `error` port is left unwired inside the definition entirely. At expansion time, any inner node with no error edge of its own gets an implicit edge straight to wherever the policy connected the instance's `error` port — but only if the policy wired that port. If it didn't, those unhandled inner errors aren't silently swallowed; they fall through to the policy's `error_handler`, or a 500 if there isn't one. Either way, the policy wiring the instance only ever sees one error exit, no matter how many inner nodes could fail.

## Compile-time expansion

Supernode instances are macro-expanded, not executed as a nested engine: before a policy is compiled, every instance is inlined into the flat node graph the engine already knows how to run — engine, metrics, and trace code are unmodified. Each inner node `n` from an instance `sec` becomes `sec/n` in the compiled graph, and inner edges are rewired with that prefix on both ends. Outer edges into the instance's `in` port and out of each of its output-derived ports and `error` are spliced onto the corresponding inlined boundary edges.

This is visible, by design, wherever node ids show up:

- **Metrics** carry labels like `node_id="sec/auth"`.
- **Debug traces** record each inner step individually, grouped under the instance id.
- **Error records** (`GatewayError.node_id`) show exactly which inner node failed, so an `error-handler` template can report `sec/auth` rather than a generic instance failure.

Expansion happens fresh on every compile; the stored config in `gateway.yaml`, the Admin API, and etcd all keep the compact `type: supernode` reference form. The expanded graph is never persisted.

## V1 limits

- **No parameters.** A supernode definition is fixed; the only per-environment variation is `${VAR}` interpolation, which still works inside a definition the same as anywhere else in `gateway.yaml`.
- **No nesting.** A supernode cannot reference another supernode. Multiple instances of the same definition are fine — each gets its own namespace — but the definitions themselves are flat.
- **Single input, single error exit.** Exactly one `input` boundary and exactly one `error` boundary; only the outputs side supports multiple named boundaries.
- Node ids inside a definition may not contain `/` and may not reuse the reserved ids `input`, `error`, `in`, `out`, `success` for anything other than the `input`/`error` boundary nodes themselves (an output boundary may take any other id — see [Named output ports](#named-output-ports)).

## Export and seeding

`SupernodeConfig` is part of `GatewayConfig`, so supernodes travel with the rest of the config wherever it does: `GET /api/config/export` (see the [Admin API guide](../guides/admin-api.md)) includes a `supernodes:` section alongside `routes:` and `policies:`, and a `gateway.yaml` that has one seeds it into a fresh instance or etcd cluster on first load — no separate export/import path to remember.
