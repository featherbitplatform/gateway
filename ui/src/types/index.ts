/**
 * TypeScript mirrors of the gateway's YAML configuration contract.
 *
 * These shapes match, field for field, the serde structs the Rust side
 * deserializes from `config/gateway.yaml` and serves over the Admin API,
 * so objects can round-trip between the editor and the gateway unchanged.
 *
 * @module types
 */

/**
 * A routing rule binding a request matcher to a named policy.
 *
 * @remarks
 * Served and persisted by the CRUD handlers in src/admin/routes.rs.
 */
export interface Route {
  /** Unique route name, used as the identifier in Admin API paths. */
  name: string;
  /** Conditions an incoming request must satisfy for this route to apply. */
  match: MatchRule;
  /** Name of the {@link Policy} executed for requests matching this route. */
  policy: string;
}

/**
 * Request-matching criteria for a route; omitted fields match anything.
 */
export interface MatchRule {
  /** Path pattern to match (supports trailing wildcards, e.g. `/api/*`). */
  path?: string;
  /** HTTP methods accepted by the route (e.g. `["GET", "POST"]`). */
  methods?: string[];
  /** Header name/value pairs that must all be present on the request. */
  headers?: Record<string, string>;
  /** Host header value to match. */
  host?: string;
}

/**
 * A node-graph policy: the request/response pipeline the gateway executes
 * for a matched route.
 *
 * @remarks
 * Compiled into a CompiledGraph and executed with success/error port
 * routing by src/graph/engine.rs.
 */
export interface Policy {
  /** Unique policy name, referenced by {@link Route.policy}. */
  name: string;
  /** Optional id of the node the engine jumps to on unrouted errors. */
  error_handler?: string;
  /** Plugin instances making up the graph. */
  nodes: PolicyNode[];
  /** Directed connections between node ports. */
  edges: PolicyEdge[];
}

/**
 * A single plugin instance within a policy graph.
 */
export interface PolicyNode {
  /** Graph-unique node id, referenced by edge endpoints. */
  id: string;
  /** Plugin type name (e.g. `upstream`, `rate-limit`, `script`). */
  type: string;
  /** Plugin-specific configuration; its shape is described per type by the pluginConfig schema registry. */
  config: Record<string, unknown>;
  /** Canvas coordinates for the editor; ignored by the gateway engine. */
  position?: { x: number; y: number };
}

/**
 * A directed edge between two node ports.
 *
 * Endpoints use the `node_id.port` format (e.g. `from: rewrite.success`,
 * `to: client.in`); valid ports are `out`, `success`, `error`, and `in`.
 *
 * @remarks
 * Parsed on the Rust side by parse_edge_endpoint in src/graph/engine.rs.
 */
export interface PolicyEdge {
  /** Source endpoint, `node_id.port` (typically an `out`/`success`/`error` port). */
  from: string;
  /** Destination endpoint, `node_id.port` (typically an `in` port). */
  to: string;
}

/**
 * A plugin type available on the gateway, as listed by `GET /api/plugins`.
 *
 * @remarks
 * The catalog is served by list_plugin_types in src/admin/policies.rs and
 * backed by the plugin factory in src/plugins/mod.rs.
 */
export interface PluginType {
  /** Plugin type name, matching {@link PolicyNode.type}. */
  type: string;
  /** Human-readable one-line description. */
  description: string;
}

/**
 * A scripted-plugin source file discovered on the gateway host,
 * as listed by `GET /api/scripts`.
 */
export interface ScriptFile {
  /** Display name of the script. */
  name: string;
  /** Path of the script file on the gateway host. */
  file: string;
  /** Scripting runtime the file targets (e.g. `lua`). */
  runtime: string;
}

/**
 * Gateway summary returned by `GET /api/status`.
 */
export interface GatewayStatus {
  /** Gateway build version. */
  version: string;
  /** Number of configured routes. */
  routes: number;
  /** Number of configured policies. */
  policies: number;
}

/**
 * Effective debug settings, from `GET /api/debug/config`.
 *
 * This endpoint answers even when debug mode is off, so the UI can explain
 * *why* the Debug panel is unavailable instead of failing silently.
 */
export interface DebugConfig {
  enabled: boolean;
  sandbox: boolean;
  trigger_header: string;
  trace_all: boolean;
  capture_bodies: boolean;
  max_traces: number;
  max_steps: number;
  max_body_bytes: number;
  traces_stored: number;
}

/** A captured request or response body (text present only when enabled). */
export interface BodyCapture {
  len: number;
  text?: string;
  truncated?: boolean;
  unchanged?: boolean;
  /** True when the body is not valid UTF-8 (image, blob, …); `text` is omitted. */
  binary?: boolean;
}

/** Redacted point-in-time view of the gateway Context. */
export interface ContextSnapshot {
  request: {
    method: string;
    path: string;
    host: string;
    scheme: string;
    headers: Record<string, string[]>;
    query_params: Record<string, string[]>;
    body: BodyCapture;
  };
  response: {
    status_code: number;
    headers: Record<string, string[]>;
    body: BodyCapture;
  };
  message: Record<string, unknown>;
  errors: { node_id: string; code: string; message: string }[];
}

/** One field-level difference between two consecutive steps. */
export interface Change {
  path: string;
  kind: 'added' | 'removed' | 'modified';
  before?: string;
  after?: string;
}

/** Which edge the engine followed after a node. */
export type EdgeKind =
  | 'success'
  | 'error'
  | 'catch_all'
  | 'terminal'
  | 'end_of_chain'
  | 'unhandled'
  | 'node_not_found';

/** One node execution within a trace. */
export interface NodeStep {
  index: number;
  node_id: string;
  node_type: string;
  outcome: { kind: 'success' } | { kind: 'error'; code: string; message: string };
  duration_us: number;
  edge: EdgeKind;
  next_node_id?: string;
  after: ContextSnapshot;
  /** Computed server-side when a trace is fetched. */
  changes: Change[];
}

/** Row shape from `GET /api/debug/traces`. */
export interface TraceSummary {
  id: string;
  seq: number;
  source: 'request' | 'sandbox';
  started_ms: number;
  route?: string;
  policy: string;
  method: string;
  path: string;
  status: number;
  duration_us: number;
  step_count: number;
  error_count: number;
  captured_bodies: boolean;
}

/** Full trace from `GET /api/debug/traces/{id}` (and inside a sandbox result). */
export interface TraceDetail extends TraceSummary {
  initial: ContextSnapshot;
  steps: NodeStep[];
  notes: string[];
}

/** Result of `POST /api/debug/sandbox`. */
export interface SandboxResult {
  mode: 'nodes' | 'policy';
  policy: string;
  warning: string;
  stored_trace_id: string;
  trace: TraceDetail;
}
