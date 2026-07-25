/**
 * Thin fetch-based client for the gateway Admin API.
 *
 * Every call is JSON over HTTP with Basic Auth, hitting the axum router
 * mounted in src/admin/mod.rs (routes, policies, plugins, scripts, status).
 *
 * @module api/client
 */
import type {
  Route,
  Policy,
  PluginType,
  ScriptFile,
  GatewayStatus,
  DebugConfig,
  TraceSummary,
  TraceDetail,
  SandboxResult,
} from '../types';

const BASE = '';

/**
 * Builds the default headers for every Admin API request.
 *
 * Reads Basic Auth credentials from `localStorage` under `gw_credentials`
 * (a `user:pass` string), falling back to `admin:admin`, and sends them
 * base64-encoded in the `Authorization` header.
 *
 * @remarks
 * Verified server-side by the Basic Auth middleware in src/admin/auth.rs.
 */
function authHeaders(): HeadersInit {
  const creds = localStorage.getItem('gw_credentials') || 'admin:admin';
  return {
    'Content-Type': 'application/json',
    Authorization: `Basic ${btoa(creds)}`,
  };
}

/**
 * Performs a fetch against the Admin API and parses the JSON response.
 *
 * @param path - Request path relative to the UI origin (e.g. `/api/routes`).
 * @param init - Optional fetch overrides (method, body); auth headers are merged in.
 * @returns The response body parsed as `T`.
 * @throws Error with `"<status>: <body>"` when the response is not 2xx.
 */
async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    ...init,
    headers: { ...authHeaders(), ...init?.headers },
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`${res.status}: ${body}`);
  }
  return res.json();
}

/**
 * Performs a fetch against the Admin API and returns the raw response text.
 *
 * Used for endpoints that return non-JSON payloads (e.g. the YAML config
 * export), which {@link request} would fail to parse.
 *
 * @param path - Request path relative to the UI origin.
 * @returns The response body as a string.
 * @throws Error with `"<status>: <body>"` when the response is not 2xx.
 */
async function requestText(path: string, init?: RequestInit): Promise<string> {
  const res = await fetch(`${BASE}${path}`, {
    ...init,
    headers: { ...authHeaders(), ...init?.headers },
  });
  const body = await res.text();
  if (!res.ok) throw new Error(`${res.status}: ${body}`);
  return body;
}

/**
 * The gateway Admin API surface used by the editor.
 *
 * All methods return promises and throw on non-2xx responses; mutating
 * calls trigger route/policy revalidation on the gateway side.
 *
 * @remarks
 * Handlers live in src/admin/routes.rs (routes), src/admin/policies.rs
 * (policies, plugins, scripts), and src/admin/status.rs (status, reload).
 */
export const api = {
  // Routes
  /** `GET /api/routes` — returns all configured routes. */
  listRoutes: () => request<Route[]>('/api/routes'),
  /** `GET /api/routes/{name}` — returns the named route. */
  getRoute: (name: string) => request<Route>(`/api/routes/${name}`),
  /** `POST /api/routes` — creates a new route; returns the API's JSON acknowledgement. */
  createRoute: (route: Route) =>
    request('/api/routes', { method: 'POST', body: JSON.stringify(route) }),
  /** `PUT /api/routes/{name}` — replaces the named route; returns the API's JSON acknowledgement. */
  updateRoute: (name: string, route: Route) =>
    request(`/api/routes/${name}`, { method: 'PUT', body: JSON.stringify(route) }),
  /** `DELETE /api/routes/{name}` — removes the named route; returns the API's JSON acknowledgement. */
  deleteRoute: (name: string) =>
    request(`/api/routes/${name}`, { method: 'DELETE' }),

  // Policies
  /** `GET /api/policies` — returns all configured policies. */
  listPolicies: () => request<Policy[]>('/api/policies'),
  /** `GET /api/policies/{name}` — returns the named policy. */
  getPolicy: (name: string) => request<Policy>(`/api/policies/${name}`),
  /** `PUT /api/policies/{name}` — upserts the named policy (also used to create new policies); returns the API's JSON acknowledgement. */
  updatePolicy: (name: string, policy: Policy) =>
    request(`/api/policies/${name}`, { method: 'PUT', body: JSON.stringify(policy) }),
  /** `DELETE /api/policies/{name}` — removes the named policy; returns the API's JSON acknowledgement. */
  deletePolicy: (name: string) =>
    request(`/api/policies/${name}`, { method: 'DELETE' }),

  // Plugins
  /** `GET /api/plugins` — returns the catalog of available plugin types, unwrapped from `{ plugins: [...] }`. */
  listPlugins: () =>
    request<{ plugins: PluginType[] }>('/api/plugins').then((r) => r.plugins),

  // Scripts
  /** `GET /api/scripts` — returns scripted-plugin files found on the gateway host, unwrapped from `{ scripts: [...] }`. */
  listScripts: () =>
    request<{ scripts: ScriptFile[] }>('/api/scripts').then((r) => r.scripts),

  // Status
  /** `GET /api/status` — returns gateway version plus route and policy counts. */
  status: () => request<GatewayStatus>('/api/status'),
  /** `POST /api/config/reload` — makes the gateway re-read gateway.yaml from disk; returns the API's JSON acknowledgement. */
  reload: () => request('/api/config/reload', { method: 'POST' }),
  /** `GET /api/config/export` — returns the live in-memory config (routes + policies) as a YAML string. */
  exportConfig: () => requestText('/api/config/export'),

  // Debug mode
  /** `GET /api/debug/config` — effective debug settings; answers even when debug is disabled. */
  debugConfig: () => request<DebugConfig>('/api/debug/config'),
  /** `GET /api/debug/traces` — recorded traces, newest first, unwrapped from `{ traces: [...] }`. */
  listTraces: () =>
    request<{ traces: TraceSummary[] }>('/api/debug/traces').then((r) => r.traces),
  /** `GET /api/debug/traces/{id}` — one trace with per-step computed changes. */
  getTrace: (id: string) => request<TraceDetail>(`/api/debug/traces/${id}`),
  /** `DELETE /api/debug/traces` — empties the trace buffer. */
  clearTraces: () => request('/api/debug/traces', { method: 'DELETE' }),
  /** `POST /api/debug/sandbox` — runs ad-hoc nodes or a named policy against a synthetic context. */
  runSandbox: (body: unknown) =>
    request<SandboxResult>('/api/debug/sandbox', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
};
