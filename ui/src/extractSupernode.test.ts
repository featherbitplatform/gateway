import { describe, expect, it } from 'vitest';
import { extractSupernode } from './extractSupernode';
import type { Policy } from './types';

/** listener -> auth -> rl -> upstream -> client, with auth.denied and both
 *  error ports exiting to a shared handler. auth+rl get selected. */
function fixture(): Policy {
  return {
    name: 'p',
    nodes: [
      { id: 'listener', type: 'listener', config: {}, position: { x: 0, y: 100 } },
      { id: 'auth', type: 'key-auth', config: { header: 'apikey' }, position: { x: 250, y: 100 } },
      { id: 'rl', type: 'rate-limit', config: {}, position: { x: 500, y: 100 } },
      { id: 'up', type: 'upstream', config: {}, position: { x: 750, y: 100 } },
      { id: 'eh', type: 'error-handler', config: {}, position: { x: 500, y: 300 } },
      { id: 'client', type: 'client', config: {}, position: { x: 1000, y: 100 } },
    ],
    edges: [
      { from: 'listener.out', to: 'auth.in' },
      { from: 'auth.success', to: 'rl.in' },
      { from: 'auth.denied', to: 'eh.in' },
      { from: 'auth.error', to: 'eh.in' },
      { from: 'rl.limited', to: 'eh.in' },
      { from: 'rl.success', to: 'up.in' },
      { from: 'up.success', to: 'client.in' },
      { from: 'eh.success', to: 'client.in' },
    ],
  };
}

describe('extractSupernode', () => {
  it('builds a definition with input/entry, per-exit output boundaries, and error', () => {
    const { definition } = extractSupernode(fixture(), ['auth', 'rl'], 'guard');
    expect(definition.name).toBe('guard');
    const byId = Object.fromEntries(definition.nodes.map((n) => [n.id, n.type]));
    expect(byId['input']).toBe('input');
    expect(byId['error']).toBe('error');
    expect(byId['output']).toBe('output'); // rl.success exit -> success port
    expect(byId['denied']).toBe('output'); // auth.denied exit
    expect(byId['limited']).toBe('output'); // rl.limited exit
    expect(byId['auth']).toBe('key-auth');
    expect(byId['rl']).toBe('rate-limit');
    const edgeSet = definition.edges.map((e) => `${e.from}->${e.to}`).sort();
    expect(edgeSet).toEqual([
      'auth.denied->denied.in',
      'auth.error->error.in',
      'auth.success->rl.in',
      'input.out->auth.in',
      'rl.limited->limited.in',
      'rl.success->output.in',
    ]);
  });

  it('rewrites the policy around one wired instance node', () => {
    const { policy, instanceId } = extractSupernode(fixture(), ['auth', 'rl'], 'guard');
    const inst = policy.nodes.find((n) => n.id === instanceId)!;
    expect(inst.type).toBe('supernode');
    expect(inst.config).toEqual({ name: 'guard' });
    expect(policy.nodes.map((n) => n.id).sort()).toEqual(
      ['client', 'eh', instanceId, 'listener', 'up'].sort()
    );
    const edgeSet = policy.edges.map((e) => `${e.from}->${e.to}`).sort();
    expect(edgeSet).toEqual(
      [
        `listener.out->${instanceId}.in`,
        `${instanceId}.success->up.in`,
        `${instanceId}.denied->eh.in`,
        `${instanceId}.limited->eh.in`,
        `${instanceId}.error->eh.in`,
        'up.success->client.in',
        'eh.success->client.in',
      ].sort()
    );
  });

  it('preserves configs, config_ref, and positions into the definition', () => {
    const p = fixture();
    p.nodes[1].config_ref = 'shared-auth';
    const { definition } = extractSupernode(p, ['auth', 'rl'], 'guard');
    const auth = definition.nodes.find((n) => n.id === 'auth')!;
    expect(auth.config).toEqual({ header: 'apikey' });
    expect(auth.config_ref).toBe('shared-auth');
    expect(auth.position).toEqual({ x: 250, y: 100 });
  });

  it('dedupes clashing port names with numeric suffixes', () => {
    const p = fixture();
    // second node with its own `denied` exit
    p.nodes.push({ id: 'auth2', type: 'key-auth', config: {}, position: { x: 300, y: 200 } });
    p.edges = p.edges.filter((e) => e.from !== 'auth.success');
    p.edges.push({ from: 'auth.success', to: 'auth2.in' });
    p.edges.push({ from: 'auth2.success', to: 'rl.in' });
    p.edges.push({ from: 'auth2.denied', to: 'eh.in' });
    const { definition } = extractSupernode(p, ['auth', 'auth2', 'rl'], 'guard');
    const outputs = definition.nodes.filter((n) => n.type === 'output').map((n) => n.id).sort();
    expect(outputs).toEqual(['denied', 'denied-2', 'limited', 'output']);
  });

  it('rejects selections containing listener/client/supernode nodes', () => {
    expect(() => extractSupernode(fixture(), ['listener', 'auth'], 'x')).toThrow(/listener/);
  });

  it('rejects selections with no inbound edge or split entry', () => {
    // up+eh: inbound edges target both `up` (from rl) and `eh` (from auth/rl)
    expect(() => extractSupernode(fixture(), ['up', 'eh'], 'x')).toThrow(/single entry/i);
  });

  it('rejects conflicting outer error targets', () => {
    const p = fixture();
    p.nodes.push({ id: 'eh2', type: 'error-handler', config: {}, position: { x: 700, y: 300 } });
    // rl's error goes somewhere other than auth's error target
    p.edges.push({ from: 'rl.error', to: 'eh2.in' });
    p.edges.push({ from: 'eh2.success', to: 'client.in' });
    expect(() => extractSupernode(p, ['auth', 'rl'], 'x')).toThrow(/error exits/i);
  });

  it('rejects selections whose exits are all error edges', () => {
    const p: Policy = {
      name: 'p',
      nodes: [
        { id: 'listener', type: 'listener', config: {} },
        { id: 'a', type: 'request-validation', config: {} },
        { id: 'client', type: 'client', config: {} },
      ],
      // only an error edge leaves the selection
      edges: [
        { from: 'listener.out', to: 'a.in' },
        { from: 'a.error', to: 'client.in' },
      ],
    };
    expect(() => extractSupernode(p, ['a'], 'x')).toThrow(/non-error exit/i);
  });

  it('rejects supernode names containing /', () => {
    expect(() => extractSupernode(fixture(), ['auth', 'rl'], 'a/b')).toThrow(/'\/'/)
  });

  it('rejects selections containing nodes with reserved boundary ids', () => {
    const p = fixture();
    p.nodes[4].id = 'error'; // rename eh to error
    expect(() => extractSupernode(p, ['error', 'rl'], 'x')).toThrow(/reserved for supernode boundary/);
  });

  it('rejects selections containing the policy error handler', () => {
    const p = fixture();
    p.error_handler = 'rl';
    expect(() => extractSupernode(p, ['auth', 'rl'], 'x')).toThrow(/error handler/);
  });
});
