/**
 * Shared entry/terminal classification and output-port resolution for
 * structural node types, used by both PluginNode (canvas handle rendering)
 * and GraphCanvas's save-time unwired-port check.
 *
 * These two call sites previously computed "what outputs does this node
 * have" independently: PluginNode hard-coded the entry/terminal special
 * cases inline, while GraphCanvas's `findUnwiredPorts` looked up the
 * catalog spec (or {@link DEFAULT_PORT_SPEC}) directly. That duplication let
 * the two drift — `findUnwiredPorts` had no terminal-type special case, so
 * the non-catalog `output`/`error` supernode boundary pseudo-nodes fell
 * through to `DEFAULT_PORT_SPEC` (success+error) and got a bogus `success`
 * output demanded, even though PluginNode correctly renders zero output
 * handles for them and `src/graph/validation.rs::validate_supernode` forbids
 * any outgoing edge from `output`/`error` in the first place. Routing both
 * call sites through {@link resolveOutputs} closes that gap structurally.
 *
 * @module nodeKinds
 */
import { DEFAULT_PORT_SPEC } from './portSpecs';
import type { PortDecl, PortSpec } from './types';

/**
 * Entry-like nodes have no `in` handle/edge: the catalog `listener` type and
 * the non-catalog `input` supernode boundary pseudo-node (src/graph/expand.rs).
 */
export function isEntryType(pluginType: string): boolean {
  return pluginType === 'listener' || pluginType === 'input';
}

/**
 * Terminal-like nodes have no outputs at all: the catalog `client` type and
 * the non-catalog `output`/`error` supernode boundary pseudo-nodes
 * (src/graph/expand.rs). `src/graph/validation.rs::validate_supernode`
 * forbids outgoing edges from `output`/`error` and allows an unwired `error`
 * boundary, so these must never be asked to wire a `success` port.
 */
export function isTerminalType(pluginType: string): boolean {
  return pluginType === 'client' || pluginType === 'output' || pluginType === 'error';
}

/**
 * Single fixed `success` port used for entry-like types with no catalog
 * entry (the `input` pseudo-node). Catalog entry-like types (`listener`)
 * use their own declared `ports.outputs` instead — see {@link resolveOutputs}.
 */
const ENTRY_FALLBACK_OUTPUTS: PortDecl[] = [
  {
    name: 'success',
    kind: 'success',
    description: 'Entry into the policy pipeline.',
  },
];

/**
 * Resolves a node's effective declared outputs, applying the entry/terminal
 * special cases before falling back to the catalog-derived `ports` (or
 * {@link DEFAULT_PORT_SPEC} when the type has no catalog entry at all).
 *
 * @param pluginType - The node's plugin type.
 * @param ports - The type's catalog `PortSpec`, if any (undefined for
 *   boundary pseudo-nodes and any type missing from the catalog).
 */
export function resolveOutputs(pluginType: string, ports: PortSpec | undefined): PortDecl[] {
  if (isTerminalType(pluginType)) return [];
  if (isEntryType(pluginType)) return ports?.outputs ?? ENTRY_FALLBACK_OUTPUTS;
  return ports?.outputs ?? DEFAULT_PORT_SPEC.outputs;
}
