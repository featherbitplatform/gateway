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
  test('rejects a second edge into an occupied single-input target', () => {
    const result = edgesAfterConnect(
      baseline(),
      { source: 'cors', sourceHandle: 'preflight', target: 'strip-prefix', targetHandle: 'in' },
      'proxy-rewrite'
    );
    expect(result).toBeNull();
  });

  test.each(['client', 'error-handler', 'output', 'error'])(
    'allows converging edges into an exempt %s target',
    (targetType) => {
      const eds = [...baseline(), edge('e-3', 'upstream', 'success', 'client')];
      const result = edgesAfterConnect(
        eds,
        { source: 'cors', sourceHandle: 'preflight', target: 'client', targetHandle: 'in' },
        targetType
      );
      expect(result).toEqual(eds);
    }
  );

  test('an already-wired source port is rewired, not fanned out', () => {
    // cors.success already feeds strip-prefix; drawing cors.success → client
    // must drop the old edge so the port keeps exactly one outcome.
    const result = edgesAfterConnect(
      baseline(),
      { source: 'cors', sourceHandle: 'success', target: 'client', targetHandle: 'in' },
      'client'
    );
    expect(result).not.toBeNull();
    expect(result!.map((e) => e.id)).toEqual(['e-0', 'e-2']);
  });

  test('rewiring one port leaves the node\'s other ports untouched', () => {
    const result = edgesAfterConnect(
      baseline(),
      { source: 'cors', sourceHandle: 'error', target: 'client', targetHandle: 'in' },
      'client'
    );
    expect(result!.map((e) => e.id)).toEqual(['e-0', 'e-1']);
  });

  test('a missing sourceHandle means the success port', () => {
    // React Flow reports null handles for single-handle nodes; loaded edges
    // normalize `out` to `success` (policyToEdges), so both sides compare equal.
    const result = edgesAfterConnect(
      baseline(),
      { source: 'cors', sourceHandle: null, target: 'client', targetHandle: null },
      'client'
    );
    expect(result!.map((e) => e.id)).toEqual(['e-0', 'e-2']);
  });

  test('a free source port connecting to a free target changes nothing', () => {
    const eds = baseline();
    const result = edgesAfterConnect(
      eds,
      { source: 'cors', sourceHandle: 'preflight', target: 'client', targetHandle: 'in' },
      'client'
    );
    expect(result).toEqual(eds);
  });
});
