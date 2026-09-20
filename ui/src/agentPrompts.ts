/**
 * Pure helpers behind the Agent panel and the "Copy as agent prompt"
 * actions. No fetching here — see api/client.ts.
 */

/** The MCP endpoint URL for this browser's admin origin. */
export function mcpEndpoint(origin: string, path: string): string {
  return origin.replace(/\/+$/, '') + (path.startsWith('/') ? path : `/${path}`);
}

/** Ready-to-paste client configs; `<TOKEN>` is for the user to fill in. */
export function clientSnippets(url: string): { claudeCode: string; mcpJson: string; curl: string } {
  const claudeCode = `claude mcp add --transport http featherbit ${url} \\\n  --header "Authorization: Bearer <TOKEN>"`;
  const mcpJson = JSON.stringify(
    { mcpServers: { featherbit: { type: 'http', url, headers: { Authorization: 'Bearer <TOKEN>' } } } },
    null,
    2,
  );
  const init = JSON.stringify({
    jsonrpc: '2.0',
    id: 1,
    method: 'initialize',
    params: { protocolVersion: '2025-03-26', capabilities: {}, clientInfo: { name: 'curl', version: '0' } },
  });
  const curl = `curl -s -X POST ${url} \\\n  -H "Authorization: Bearer <TOKEN>" \\\n  -H "Accept: application/json, text/event-stream" \\\n  -H "Content-Type: application/json" \\\n  -d '${init}'`;
  return { claudeCode, mcpJson, curl };
}

/** Tools a `read` token can call (kept in sync with the server by E2E-MCP-02). */
export const READ_TOOLS: readonly string[] = [
  'list_node_types', 'get_node_type', 'list_vars', 'get_status', 'export_config',
  'list_routes', 'get_route', 'list_policies', 'get_policy', 'list_supernodes', 'get_supernode',
  'list_plugin_configs', 'get_plugin_config', 'list_stores', 'list_consumers', 'get_consumer',
  'validate_policy', 'validate_supernode', 'list_traces', 'get_trace', 'get_trace_step', 'run_sandbox',
];

/** Tools that additionally need a `write` token. */
export const WRITE_TOOLS: readonly string[] = [
  'put_route', 'delete_route', 'put_policy', 'delete_policy', 'put_supernode', 'delete_supernode',
  'put_plugin_config', 'delete_plugin_config', 'put_store', 'delete_store', 'purge_cache', 'reload_config',
];

const MCP_HINT =
  'If the `featherbit` MCP server is connected, prefer its tools (get_trace_step, get_node_type, validate_policy, run_sandbox) over the data inlined above.';

/** Appends the one-line MCP hint to a rendered prompt. */
export function withMcpHint(text: string): string {
  return `${text.replace(/\s+$/, '')}\n\n${MCP_HINT}\n`;
}

/** Query string for `GET /api/mcp/prompts/{name}` from possibly-undefined args. */
export function promptQuery(args: Record<string, string | undefined>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(args)) if (v !== undefined && v !== '') q.set(k, v);
  return q.toString();
}
