/**
 * Fetch-based MCP client for this gateway's own `/mcp` endpoint (Streamable
 * HTTP). Same wire shape as the e2e helper in e2e/tests/mcp.spec.ts: one
 * JSON-RPC request per POST, a session id from `initialize` echoed back on
 * every later call, replies as plain JSON or a single SSE event.
 *
 * @module chat/mcpClient
 */
import type { ToolDef } from './openai';

export interface McpTool {
  name: string;
  description?: string;
  inputSchema?: unknown;
}

export interface McpToolResult {
  content: Array<{ type: string; text?: string }>;
  isError?: boolean;
}

/** Transport/protocol failure (not a tool-level `isError`, which is returned). */
export class McpError extends Error {
  status?: number;
  code?: number;
  constructor(message: string, opts: { status?: number; code?: number } = {}) {
    super(message);
    this.name = 'McpError';
    this.status = opts.status;
    this.code = opts.code;
  }
}

export interface McpClient {
  /** Initializes lazily, then `tools/list`. */
  listTools(): Promise<McpTool[]>;
  callTool(name: string, args: unknown, signal?: AbortSignal): Promise<McpToolResult>;
  /** Forgets the session; the next call re-initializes. */
  reset(): void;
}

const PROTOCOL_VERSION = '2025-03-26';

/** Parses a JSON or single-event SSE body; empty → null. */
export function parseRpcBody(text: string): unknown {
  const t = text.trim();
  if (t === '') return null;
  if (t.startsWith('{') || t.startsWith('[')) return JSON.parse(t);
  const data = t
    .split(/\r?\n/)
    .filter((l) => l.startsWith('data:'))
    .map((l) => l.slice(5).trim())
    .join('');
  return data === '' ? null : JSON.parse(data);
}

export function toToolDefs(tools: McpTool[]): ToolDef[] {
  return tools.map((t) => {
    const fn: ToolDef['function'] = { name: t.name, parameters: t.inputSchema ?? { type: 'object', properties: {} } };
    if (t.description) fn.description = t.description;
    return { type: 'function', function: fn };
  });
}

export function resultText(r: McpToolResult): string {
  return r.content
    .filter((c) => c.type === 'text' && typeof c.text === 'string')
    .map((c) => c.text as string)
    .join('\n');
}

export function createMcpClient(opts: { url: string; token: string; fetchImpl?: typeof fetch }): McpClient {
  const fetchImpl = opts.fetchImpl ?? fetch;
  let session: string | null = null;
  let nextId = 1;
  let initializing: Promise<void> | null = null;

  const headers = (): Record<string, string> => {
    const h: Record<string, string> = {
      'Content-Type': 'application/json',
      Accept: 'application/json, text/event-stream',
      Authorization: `Bearer ${opts.token}`,
    };
    if (session) h['Mcp-Session-Id'] = session;
    return h;
  };

  const post = async (payload: unknown, signal?: AbortSignal): Promise<Response> =>
    fetchImpl(opts.url, { method: 'POST', headers: headers(), body: JSON.stringify(payload), signal });

  const initialize = async (): Promise<void> => {
    session = null;
    const id = nextId++;
    const res = await post({
      jsonrpc: '2.0',
      id,
      method: 'initialize',
      params: { protocolVersion: PROTOCOL_VERSION, capabilities: {}, clientInfo: { name: 'featherbit-ui', version: '1' } },
    });
    if (!res.ok) throw new McpError(`initialize failed: ${res.status} ${await res.text()}`, { status: res.status });
    const body = parseRpcBody(await res.text()) as { error?: { code?: number; message?: string } } | null;
    if (body?.error) throw new McpError(body.error.message ?? 'initialize error', { code: body.error.code });
    session = res.headers.get('mcp-session-id');
    await post({ jsonrpc: '2.0', method: 'notifications/initialized' });
  };

  const ensureSession = async (): Promise<void> => {
    if (session) return;
    initializing ??= initialize().finally(() => (initializing = null));
    await initializing;
  };

  const rpc = async <T>(method: string, params: unknown, signal?: AbortSignal, retried = false): Promise<T> => {
    await ensureSession();
    const id = nextId++;
    const res = await post({ jsonrpc: '2.0', id, method, params }, signal);
    if (res.status === 404 && !retried) {
      session = null;
      return rpc<T>(method, params, signal, true);
    }
    const text = await res.text();
    if (!res.ok) throw new McpError(`${method} failed: ${res.status} ${text}`, { status: res.status });
    const body = parseRpcBody(text) as { result?: T; error?: { code?: number; message?: string } } | null;
    if (!body) throw new McpError(`${method}: empty reply`);
    if (body.error) throw new McpError(body.error.message ?? `${method} error`, { code: body.error.code });
    return body.result as T;
  };

  return {
    async listTools() {
      const r = await rpc<{ tools?: McpTool[] }>('tools/list', {});
      return r.tools ?? [];
    },
    async callTool(name, args, signal) {
      return rpc<McpToolResult>('tools/call', { name, arguments: args ?? {} }, signal);
    },
    reset() {
      session = null;
    },
  };
}
