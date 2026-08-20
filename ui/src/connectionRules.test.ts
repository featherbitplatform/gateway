import { describe, expect, test } from 'vitest';

import { edgesAfterConnect } from './connectionRules';

type TestEdge = {
  id: string;
  source: string;
  sourceHandle?: string | null;
  target: string;
  targetHandle?: string | null;
};

const edge = (id: string, from: string, fromPort: string, to: string): TestEdge => ({
  id,
  source: from,
  sourceHandle: fromPort,
  target: to,
  targetHandle: 'in',
});

// The echo-policy shape: listener → cors → strip-prefix → upstream → client,
// with cors.error feeding the error-handler.
const baseline = (): TestEdge[] => [
  edge('e-0', 'listener', 'success', 'cors'),
  edge('e-1', 'cors', 'success', 'strip-prefix'),
  edge('e-2', 'cors', 'error', 'error-handler'),
];

describe('edgesAfterConnect', () => {
  test('allows converging edges into an occupied target (fan-in)', () => {
    // strip-prefix.in is already fed by cors.success; the engine accepts any
    // number of incoming edges, so the canvas must too.
    const eds = baseline();
    const result = edgesAfterConnect(eds, {
      source: 'listener',
      sourceHandle: 'preflight',
      target: 'strip-prefix',
      targetHandle: 'in',
    });
    expect(result).toEqual(eds);
  });

  test('an already-wired source port is rewired, not fanned out', () => {
    // cors.success already feeds strip-prefix; drawing cors.success → client
    // must drop the old edge so the port keeps exactly one outcome.
    const result = edgesAfterConnect(baseline(), {
      source: 'cors',
      sourceHandle: 'success',
      target: 'client',
      targetHandle: 'in',
    });
    expect(result).not.toBeNull();
    expect(result!.map((e) => e.id)).toEqual(['e-0', 'e-2']);
  });

  test("rewiring one port leaves the node's other ports untouched", () => {
    const result = edgesAfterConnect(baseline(), {
      source: 'cors',
      sourceHandle: 'error',
      target: 'client',
      targetHandle: 'in',
    });
    expect(result!.map((e) => e.id)).toEqual(['e-0', 'e-1']);
  });

  test('a missing sourceHandle means the success port', () => {
    // React Flow reports null handles for single-handle nodes; loaded edges
    // normalize `out` to `success` (policyToEdges), so both sides compare equal.
    const result = edgesAfterConnect(baseline(), {
      source: 'cors',
      sourceHandle: null,
      target: 'client',
      targetHandle: null,
    });
    expect(result!.map((e) => e.id)).toEqual(['e-0', 'e-2']);
  });

  test('a free source port connecting to a free target changes nothing', () => {
    const eds = baseline();
    const result = edgesAfterConnect(eds, {
      source: 'cors',
      sourceHandle: 'preflight',
      target: 'client',
      targetHandle: 'in',
    });
    expect(result).toEqual(eds);
  });

  test('rejects an edge that would close a cycle', () => {
    // cors already reaches strip-prefix; strip-prefix → cors would loop.
    const result = edgesAfterConnect(baseline(), {
      source: 'strip-prefix',
      sourceHandle: 'success',
      target: 'cors',
      targetHandle: 'in',
    });
    expect(result).toBeNull();
  });

  test('rejects a cycle closed across a multi-hop path', () => {
    // listener → cors → strip-prefix → upstream; upstream → listener loops.
    const eds = [...baseline(), edge('e-3', 'strip-prefix', 'success', 'upstream')];
    const result = edgesAfterConnect(eds, {
      source: 'upstream',
      sourceHandle: 'success',
      target: 'listener',
      targetHandle: 'in',
    });
    expect(result).toBeNull();
  });

  test('rejects a self-loop', () => {
    const result = edgesAfterConnect(baseline(), {
      source: 'cors',
      sourceHandle: 'preflight',
      target: 'cors',
      targetHandle: 'in',
    });
    expect(result).toBeNull();
  });

  test('a rewire that removes the only path back is still a rewire, not a cycle', () => {
    // cors.success currently feeds strip-prefix. Redrawing that same port
    // back out of strip-prefix's downstream is a cycle; but redrawing
    // cors.success onto a node with no path back to cors is fine even though
    // cors sits upstream of half the graph.
    const eds = [...baseline(), edge('e-3', 'strip-prefix', 'success', 'upstream')];
    const result = edgesAfterConnect(eds, {
      source: 'cors',
      sourceHandle: 'success',
      target: 'upstream',
      targetHandle: 'in',
    });
    expect(result).not.toBeNull();
    expect(result!.map((e) => e.id)).toEqual(['e-0', 'e-2', 'e-3']);
  });
});
