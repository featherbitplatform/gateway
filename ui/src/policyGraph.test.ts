import { describe, expect, test } from 'vitest';

import { policyToEdges, policyToNodes, splitEdge } from './policyGraph';
import type { PortSpecLookup } from './portSpecs';
import type { Policy } from './types';

const noop = () => {};

/** Minimal catalog lookup: one type with a declared outcome port. */
const SPECS: PortSpecLookup = {
  cors: {
    input: 'in',
    outputs: [
      { name: 'success', kind: 'success', description: '' },
      { name: 'preflight', kind: 'outcome', description: '' },
      { name: 'error', kind: 'error', description: '' },
    ],
  },
};

/** input → cors, with cors's outcome and error ports both feeding a supernode instance. */
const policy = (): Policy => ({
  name: 'p',
  nodes: [
    { id: 'input', type: 'input', config: {} },
    { id: 'cors', type: 'cors', config: {} },
    { id: 'sn', type: 'supernode', config: { name: 'auth-check' }, position: { x: 40, y: 60 } },
  ],
  edges: [
    { from: 'input.out', to: 'cors.in' },
    { from: 'cors.preflight', to: 'sn.in' },
    { from: 'cors.error', to: 'sn.in' },
  ],
});

describe('splitEdge', () => {
  test('splits node and port on the last dot', () => {
    expect(splitEdge('up.success')).toEqual(['up', 'success']);
  });
  test('node ids containing dots stay intact', () => {
    expect(splitEdge('a.b.error')).toEqual(['a.b', 'error']);
  });
  test('no dot defaults to the out port', () => {
    expect(splitEdge('listener')).toEqual(['listener', 'out']);
  });
});

describe('policyToNodes', () => {
  test('labels a supernode instance with its definition name', () => {
    const nodes = policyToNodes(policy(), noop, SPECS, true);
    expect(nodes.find((n) => n.id === 'sn')?.data.label).toBe('⬡ auth-check');
  });
  test('saved positions win over auto-layout', () => {
    const nodes = policyToNodes(policy(), noop, SPECS, true);
    expect(nodes.find((n) => n.id === 'sn')?.position).toEqual({ x: 40, y: 60 });
  });
  test('auto-layout places the input entry node at the first column', () => {
    const nodes = policyToNodes(policy(), noop, SPECS, true);
    expect(nodes.find((n) => n.id === 'input')?.position).toEqual({ x: 0, y: 150 });
  });
});

describe('policyToEdges', () => {
  test("normalizes an 'out' source port to the success handle", () => {
    const edges = policyToEdges(policy(), SPECS);
    expect(edges[0].sourceHandle).toBe('success');
    expect(edges[0].style?.stroke).toBe('var(--success)');
  });
  test('colors a declared outcome-port edge with the accent stroke, not animated', () => {
    const edges = policyToEdges(policy(), SPECS);
    expect(edges[1].style?.stroke).toBe('var(--accent)');
    expect(edges[1].animated).toBe(false);
  });
  test('error-port edges animate with the error stroke', () => {
    const edges = policyToEdges(policy(), SPECS);
    expect(edges[2].animated).toBe(true);
    expect(edges[2].style?.stroke).toBe('var(--error)');
  });
});
