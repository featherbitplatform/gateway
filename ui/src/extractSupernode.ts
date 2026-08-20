import type { Policy, PolicyEdge, PolicyNode, Supernode } from './types';
import { splitEdge } from './policyGraph';

const FORBIDDEN_TYPES = ['listener', 'client', 'supernode', 'input', 'output', 'error'];
const RESERVED = ['input', 'output', 'error', 'in', 'out', 'success'];

export interface ExtractionResult {
  definition: Supernode;
  policy: Policy;
  instanceId: string;
}

export function extractSupernode(
  policy: Policy,
  selectedIds: string[],
  name: string
): ExtractionResult {
  if (name.includes('/')) {
    throw new Error("Supernode names must not contain '/'");
  }

  const selected = new Set(selectedIds);

  if (policy.error_handler && selected.has(policy.error_handler)) {
    throw new Error(
      `Node '${policy.error_handler}' is the policy's error handler — reassign it before extracting`
    );
  }

  const selectedNodes = policy.nodes.filter((n) => selected.has(n.id));

  for (const n of selectedNodes) {
    if (FORBIDDEN_TYPES.includes(n.type)) {
      throw new Error(
        `Cannot extract '${n.id}': ${n.type} nodes cannot live inside a supernode`
      );
    }
    if (['input', 'output', 'error'].includes(n.id)) {
      throw new Error(
        `Rename node '${n.id}' before extracting — that id is reserved for supernode boundary nodes`
      );
    }
  }

  // Classify edges relative to the selection.
  const inner: PolicyEdge[] = [];
  const inbound: PolicyEdge[] = [];
  const outNonError: PolicyEdge[] = [];
  const outError: PolicyEdge[] = [];
  const outside: PolicyEdge[] = [];
  for (const e of policy.edges) {
    const [from, fromPort] = splitEdge(e.from);
    const [to] = splitEdge(e.to);
    const fromIn = selected.has(from);
    const toIn = selected.has(to);
    if (fromIn && toIn) inner.push(e);
    else if (!fromIn && toIn) inbound.push(e);
    else if (fromIn && !toIn) (fromPort === 'error' ? outError : outNonError).push(e);
    else outside.push(e);
  }

  // Single entry node.
  const entryTargets = new Set(inbound.map((e) => splitEdge(e.to)[0]));
  if (entryTargets.size !== 1) {
    throw new Error(
      entryTargets.size === 0
        ? 'The selection needs an incoming edge to become the single entry point'
        : `The selection must have a single entry node — edges enter at: ${[...entryTargets].join(', ')}`
    );
  }
  const entryId = [...entryTargets][0];

  if (outNonError.length === 0) {
    throw new Error('The selection needs at least one non-error exit edge');
  }

  // One error exit target at most.
  const errorTargets = new Set(outError.map((e) => e.to));
  if (errorTargets.size > 1) {
    throw new Error(
      `All error exits must share one target (an instance has a single error port) — found: ${[...errorTargets].join(', ')}`
    );
  }

  // Output boundaries: one per non-error exit edge, named after its source
  // port (`success`/`out` -> the `output` boundary = instance port `success`).
  const takenIds = new Set(selectedNodes.map((n) => n.id));
  const uniquify = (base: string): string => {
    let candidate = base;
    let i = 2;
    while (takenIds.has(candidate) || (candidate !== 'output' && RESERVED.includes(candidate))) {
      candidate = `${base}-${i++}`;
    }
    takenIds.add(candidate);
    return candidate;
  };
  const exits = outNonError.map((e) => {
    const [, fromPort] = splitEdge(e.from);
    const normalized = fromPort === 'out' ? 'success' : fromPort;
    const boundaryId = uniquify(normalized === 'success' ? 'output' : normalized);
    return { edge: e, boundaryId, port: boundaryId === 'output' ? 'success' : boundaryId };
  });

  // Geometry for the boundary pseudo-nodes.
  const pos = (n: PolicyNode) => n.position ?? { x: 0, y: 0 };
  const xs = selectedNodes.map((n) => pos(n).x);
  const ys = selectedNodes.map((n) => pos(n).y);
  const minX = Math.min(...xs), maxX = Math.max(...xs);
  const minY = Math.min(...ys), maxY = Math.max(...ys);
  const entryY = pos(selectedNodes.find((n) => n.id === entryId)!).y;

  const definition: Supernode = {
    name,
    nodes: [
      { id: 'input', type: 'input', config: {}, position: { x: minX - 250, y: entryY } },
      ...exits.map((x, i) => ({
        id: x.boundaryId, type: 'output', config: {},
        position: { x: maxX + 250, y: minY + i * 100 },
      })),
      { id: 'error', type: 'error', config: {}, position: { x: maxX + 250, y: maxY + 180 } },
      ...selectedNodes.map((n) => ({ ...n })),
    ],
    edges: [
      { from: 'input.out', to: `${entryId}.in` },
      ...inner,
      ...exits.map((x) => ({ from: x.edge.from, to: `${x.boundaryId}.in` })),
      ...outError.map((e) => ({ from: e.from, to: 'error.in' })),
    ],
  };

  // Rewritten policy: selection replaced by one instance node.
  const remaining = policy.nodes.filter((n) => !selected.has(n.id));
  let instanceId = name;
  while (remaining.some((n) => n.id === instanceId)) {
    instanceId = `${name}-${Math.floor(Math.random() * 36 ** 4).toString(36)}`;
  }
  const centroid = {
    x: Math.round(xs.reduce((a, b) => a + b, 0) / xs.length),
    y: Math.round(ys.reduce((a, b) => a + b, 0) / ys.length),
  };
  const instance: PolicyNode = {
    id: instanceId,
    type: 'supernode',
    config: { name },
    position: centroid,
  };

  const newEdges: PolicyEdge[] = [
    ...outside,
    ...inbound.map((e) => ({ from: e.from, to: `${instanceId}.in` })),
    ...exits.map((x) => ({ from: `${instanceId}.${x.port}`, to: x.edge.to })),
    ...(outError.length > 0
      ? [{ from: `${instanceId}.error`, to: outError[0].to }]
      : []),
  ];

  return {
    definition,
    policy: { ...policy, nodes: [...remaining, instance], edges: newEdges },
    instanceId,
  };
}
