import { describe, expect, test } from 'vitest';

import { policyToEdges, policyToNodes, splitEdge, supernodePortSpec } from './policyGraph';
import type { PortSpecLookup } from './portSpecs';
import type { Policy, Supernode } from './types';

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

/** Supernode definition with a default output boundary and a named 'denied' boundary. */
const gateDef: Supernode = {
  name: 'auth-gate',
  nodes: [
    { id: 'input', type: 'input', config: {} },
    { id: 'output', type: 'output', config: {} },
    { id: 'denied', type: 'output', config: {} },
    { id: 'error', type: 'error', config: {} },
    { id: 'auth', type: 'key-auth', config: {} },
  ],
  edges: [
    { from: 'input.out', to: 'auth.in' },
    { from: 'auth.success', to: 'output.in' },
    { from: 'auth.denied', to: 'denied.in' },
  ],
};

describe('supernodePortSpec', () => {
  test('derives success + named outcome + error, in order', () => {
    const spec = supernodePortSpec(gateDef)!;
    expect(spec.outputs.map((p) => [p.name, p.kind])).toEqual([
      ['success', 'success'],
      ['denied', 'outcome'],
      ['error', 'error'],
    ]);
  });

  test('omits success when no output-id boundary exists', () => {
    const namedOnly: Supernode = {
      ...gateDef,
      nodes: gateDef.nodes.filter((n) => n.id !== 'output'),
    };
    const spec = supernodePortSpec(namedOnly)!;
    expect(spec.outputs.map((p) => p.name)).toEqual(['denied', 'error']);
  });

  test('returns undefined for a missing definition', () => {
    expect(supernodePortSpec(undefined)).toBeUndefined();
  });
});

describe('policyToNodes/policyToEdges with supernode instances', () => {
  const snPolicy: Policy = {
    name: 'p',
    nodes: [
      { id: 'listener', type: 'listener', config: {} },
      { id: 'gate', type: 'supernode', config: { name: 'auth-gate' } },
      { id: 'reject', type: 'error-handler', config: {} },
      { id: 'client', type: 'client', config: {} },
    ],
    edges: [
      { from: 'listener.out', to: 'gate.in' },
      { from: 'gate.success', to: 'client.in' },
      { from: 'gate.denied', to: 'reject.in' },
    ],
  };

  test('threads the derived port spec into instance node data', () => {
    const nodes = policyToNodes(snPolicy, noop, {}, true, [gateDef]);
    const gate = nodes.find((n) => n.id === 'gate')!;
    const ports = (gate.data as { ports?: { outputs: { name: string }[] } }).ports;
    expect(ports?.outputs.map((p) => p.name)).toEqual(['success', 'denied', 'error']);
  });

  test('styles a named instance port as an outcome edge', () => {
    const edges = policyToEdges(snPolicy, {}, [gateDef]);
    const denied = edges.find((e) => e.sourceHandle === 'denied')!;
    expect(denied.style?.stroke).toBe('var(--accent)');
    expect(denied.animated).toBe(false);
  });

  test('falls back to the catalog/default pair when the definition is missing', () => {
    const nodes = policyToNodes(snPolicy, noop, {}, true, []);
    const gate = nodes.find((n) => n.id === 'gate')!;
    expect((gate.data as { ports?: unknown }).ports).toBeUndefined();
  });
});
