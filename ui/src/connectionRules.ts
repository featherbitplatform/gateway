/**
 * Pure connection rules for the graph canvas.
 *
 * The canvas enforces at draw time exactly the invariants policy compilation
 * enforces on save (engine.rs), so a graph never leaves the editor in a shape
 * the compiler would bounce:
 *
 * - no fan-out — a declared output port carries exactly one outcome
 *   ("duplicate edge from '<node>.<port>'");
 * - no cycles — the runtime walk has no step limit, so a loop would never
 *   terminate ("policy graph contains a cycle through node '<node>'").
 *
 * Fan-in is unrestricted on both sides: any number of edges may converge on
 * a node's input.
 */

/** The endpoint pair React Flow hands to onConnect. */
export type ProposedConnection = {
  source: string;
  sourceHandle?: string | null;
  target: string;
  targetHandle?: string | null;
};

/**
 * Applies the cardinality and acyclicity rules to a proposed connection and
 * returns the edge list the new edge should be added onto, or `null` to
 * reject the connection.
 *
 * - Output side: a source port holds at most one outgoing edge — drawing
 *   from an already-wired port rewires it, dropping the previous edge.
 * - Cycles: a connection whose target already reaches the source (over the
 *   edges left after the rewire) would close a loop and is rejected, as is a
 *   direct self-loop.
 *
 * A missing source handle is the `success` port (loaded edges fold `out` into
 * `success` in policyToEdges); target handles play no role since inputs
 * accept any number of edges.
 */
export function edgesAfterConnect<
  E extends Pick<ProposedConnection, 'source' | 'sourceHandle' | 'target' | 'targetHandle'>,
>(eds: E[], connection: ProposedConnection): E[] | null {
  const sourceHandle = connection.sourceHandle || 'success';
  const remaining = eds.filter(
    (e) => !(e.source === connection.source && (e.sourceHandle || 'success') === sourceHandle)
  );

  if (connection.target === connection.source || reaches(remaining, connection.target, connection.source)) {
    return null;
  }
  return remaining;
}

/** Depth-first reachability from `from` to `to` over the given edges. */
function reaches(
  eds: ReadonlyArray<Pick<ProposedConnection, 'source' | 'target'>>,
  from: string,
  to: string
): boolean {
  const stack = [from];
  const seen = new Set<string>();
  while (stack.length > 0) {
    const node = stack.pop()!;
    if (node === to) return true;
    if (seen.has(node)) continue;
    seen.add(node);
    for (const e of eds) {
      if (e.source === node) stack.push(e.target);
    }
  }
  return false;
}
