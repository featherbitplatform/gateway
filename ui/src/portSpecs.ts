/**
 * Port-spec lookup for the policy editor's dynamic handles.
 *
 * Wraps the `ports` field carried by every `GET /api/plugins` catalog entry
 * (see src/plugins/ports.rs, src/admin/policies.rs::plugin_catalog) into a
 * type-keyed lookup for {@link module:components/PluginNode} and
 * {@link module:components/GraphCanvas}, with a hard-coded success+error
 * fallback for any plugin type missing from the catalog — a stale UI build
 * talking to an older gateway, or a node type the catalog doesn't cover —
 * so the editor stays usable rather than rendering no handles at all.
 *
 * @module portSpecs
 */
import type { PluginType, PortSpec } from './types';

/** Default success+error pair used for any plugin type missing from the catalog. */
export const DEFAULT_PORT_SPEC: PortSpec = {
  input: 'Request context from the previous node.',
  outputs: [
    {
      name: 'success',
      kind: 'success',
      description: 'The node completed normally; the request continues.',
    },
    {
      name: 'error',
      kind: 'error',
      description: 'The node failed (configuration, parse, or infrastructure error).',
    },
  ],
};

/** `pluginType -> PortSpec` lookup, built once from a fetched `GET /api/plugins` catalog. */
export type PortSpecLookup = Record<string, PortSpec>;

/**
 * Builds a {@link PortSpecLookup} from the plugin catalog.
 *
 * @param plugins - Catalog entries from `GET /api/plugins` (already fetched
 *   for the add-node drawer/palette; this does not issue a second request).
 */
export function buildPortSpecs(plugins: PluginType[]): PortSpecLookup {
  const specs: PortSpecLookup = {};
  for (const p of plugins) {
    if (p.ports) specs[p.type] = p.ports;
  }
  return specs;
}

/**
 * Resolves a plugin type's port spec, falling back to {@link DEFAULT_PORT_SPEC}
 * when the type is absent from the lookup.
 */
export function getPortSpec(specs: PortSpecLookup, pluginType: string): PortSpec {
  return specs[pluginType] ?? DEFAULT_PORT_SPEC;
}
