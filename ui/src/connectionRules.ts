/**
 * Pure connection rules for the graph canvas.
 *
 * The engine gives every edge endpoint single cardinality on the output side:
 * a declared port carries exactly one outcome, and policy compilation rejects
 * fan-out (engine.rs: "duplicate edge from '<node>.<port>'"). The canvas
 * enforces the same invariant at draw time so a graph never leaves the editor
 * in a shape the compiler would bounce.
 */

/** The endpoint pair React Flow hands to onConnect. */
export type ProposedConnection = {
  source: string;
  sourceHandle?: string | null;
  target: string;
  targetHandle?: string | null;
};

/** Target types whose input legitimately converges (many paths return a
 *  response; errors funnel from several nodes; supernode boundary exits). */
const MULTI_INPUT_TARGETS = new Set(['client', 'error-handler', 'output', 'error']);

/**
 * Applies the cardinality rules to a proposed connection and returns the edge
 * list the new edge should be added onto, or `null` to reject the connection.
 *
 * - Input side: a second edge into an occupied `in` handle is rejected,
 *   unless the target type converges by design (client, error-handler,
 *   supernode boundary exits).
 * - Output side: a source port holds at most one outgoing edge — drawing
 *   from an already-wired port rewires it, dropping the previous edge.
 *
 * Handles are normalized the way the rest of the canvas stores them: a
 * missing source handle is the `success` port (loaded edges fold `out` into
 * `success` in policyToEdges), a missing target handle is `in`.
 */
export function edgesAfterConnect<
  E extends Pick<ProposedConnection, 'source' | 'sourceHandle' | 'target' | 'targetHandle'>,
>(eds: E[], connection: ProposedConnection, targetType: string | undefined): E[] | null {
  const targetHandle = connection.targetHandle || 'in';
  if (!MULTI_INPUT_TARGETS.has(targetType ?? '')) {
    const targetWired = eds.some(
      (e) => e.target === connection.target && (e.targetHandle || 'in') === targetHandle
    );
    if (targetWired) return null;
  }

  const sourceHandle = connection.sourceHandle || 'success';
  return eds.filter(
    (e) => !(e.source === connection.source && (e.sourceHandle || 'success') === sourceHandle)
  );
}
