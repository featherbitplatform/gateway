//! Policy compilation and graph execution.
//!
//! Turns a [`PolicyConfig`] (nodes + `from`/`to` edges from `gateway.yaml`)
//! into a [`CompiledGraph`] with instantiated plugins, then walks that graph
//! per request: success edges on `Ok`, error edges (or the policy-level
//! catch-all handler) on failure, stopping at terminal `client` nodes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::config::PolicyConfig;
use crate::context::Context;
use crate::debug::{EdgeKind, StepOutcome, TraceRecorder};
use crate::plugins::resources::PluginResources;
use crate::plugins::{self, Plugin};

/// A compiled, ready-to-execute graph with instantiated plugins.
///
/// Built by [`compile_policy`] and shared read-only by the data-plane; one
/// instance serves all requests matching the route that references its policy.
pub struct CompiledGraph {
    nodes: HashMap<String, Box<dyn Plugin>>,
    /// node_id -> (port -> target node_id). Ports normalized (`out` -> `success`).
    edges: HashMap<String, HashMap<String, String>>,
    /// The node that starts the pipeline (first node after listener)
    entry_node_id: String,
    /// Node IDs that are terminal (client nodes) — execution stops when we reach one
    terminal_node_ids: HashSet<String>,
    /// Policy-level catch-all error handler node ID
    catch_all_handler: Option<String>,
    /// Policy name, used as a metrics label.
    policy_name: String,
    /// Process-wide services (metrics registry, shared clients); per-node
    /// metrics recording is disabled when `resources.metrics` is `None`.
    resources: Arc<PluginResources>,
}

// Manual impl: `Box<dyn Plugin>` doesn't implement `Debug`, so `#[derive]`
// isn't available. This exists solely so `Result<CompiledGraph, String>` can
// be `.unwrap_err()`'d in tests; the node table is summarized by id only.
impl std::fmt::Debug for CompiledGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledGraph")
            .field("policy_name", &self.policy_name)
            .field("node_ids", &self.nodes.keys().collect::<Vec<_>>())
            .field("edges", &self.edges)
            .field("entry_node_id", &self.entry_node_id)
            .field("terminal_node_ids", &self.terminal_node_ids)
            .field("catch_all_handler", &self.catch_all_handler)
            .finish()
    }
}

impl CompiledGraph {
    /// Executes the graph with the given initial context and returns the
    /// final context (with `response` populated).
    ///
    /// Walks nodes starting at the entry node (the first node after the
    /// listener), following the success edge after each `Ok` result. On a
    /// plugin error the error is tagged with the failing node's ID, pushed
    /// onto `ctx.errors`, and execution jumps to the node's `error` edge if
    /// one exists, otherwise to the policy-level catch-all handler; with
    /// neither, execution stops and a generic JSON 500 response is written.
    /// Reaching a terminal `client` node (or a node with no success edge)
    /// ends the walk. This method never fails: every outcome is expressed
    /// through the returned context's response.
    pub async fn execute(&self, ctx: Context) -> Context {
        self.run(ctx, None).await.0
    }

    /// Same walk as [`execute`](Self::execute), additionally recording a
    /// [`Trace`] of every node: the context after each one, the outcome, and
    /// which edge the engine followed.
    ///
    /// Used by debug mode and the sandbox. Deliberately the *same* loop rather
    /// than a parallel implementation — a trace that could drift from the real
    /// execution path would be worse than no trace at all.
    pub async fn execute_traced(
        &self,
        ctx: Context,
        recorder: TraceRecorder,
    ) -> (Context, TraceRecorder) {
        let (ctx, rec) = self.run(ctx, Some(recorder)).await;
        (
            ctx,
            rec.expect("recorder is returned when one was supplied"),
        )
    }

    /// The single graph walk, optionally recording each step.
    ///
    /// When `recorder` is `None` the only added cost is one `Option`
    /// discriminant check per node — the same shape as the existing metrics
    /// branch, and perfectly branch-predicted.
    async fn run(
        &self,
        mut ctx: Context,
        mut recorder: Option<TraceRecorder>,
    ) -> (Context, Option<TraceRecorder>) {
        let mut current_node_id = self.entry_node_id.clone();

        loop {
            let node = match self.nodes.get(&current_node_id) {
                Some(n) => n,
                None => {
                    ctx.response.status_code = 500;
                    ctx.response.body = bytes::Bytes::from(format!(
                        "Graph error: node '{}' not found",
                        current_node_id
                    ));
                    if let Some(r) = recorder.as_mut() {
                        r.record_step(
                            &current_node_id,
                            "<missing>",
                            StepOutcome::Error {
                                code: "NODE_NOT_FOUND".to_string(),
                                message: format!("node '{}' not found", current_node_id),
                            },
                            std::time::Duration::ZERO,
                            EdgeKind::NodeNotFound,
                            None,
                            None,
                            &ctx,
                        );
                    }
                    break;
                }
            };

            let node_type = node.plugin_type().to_string();
            let started = std::time::Instant::now();
            let result = node.execute(ctx).await;
            let elapsed = started.elapsed();
            if let Some(ref m) = self.resources.metrics {
                m.node_execution_count
                    .with_label_values(&[&self.policy_name, &current_node_id, &node_type])
                    .inc();
                m.node_execution_duration
                    .with_label_values(&[&self.policy_name, &current_node_id])
                    .observe(elapsed.as_secs_f64());
            }

            match result {
                Ok(output) => {
                    ctx = output.context;
                    let port = output.port.unwrap_or("success");
                    // `error` is reachable only through `Err`, which records a
                    // GatewayError and follows the error/catch-all fallback
                    // chain. An `Ok` naming it would silently take an error
                    // edge with no error record attached.
                    debug_assert!(
                        port != "error",
                        "plugins must not emit on the error port via Ok"
                    );

                    // If this is a terminal node (client), we're done
                    let terminal = self.terminal_node_ids.contains(&current_node_id);
                    let next = if terminal {
                        None
                    } else {
                        self.edges.get(&current_node_id).and_then(|m| m.get(port))
                    };
                    if let Some(r) = recorder.as_mut() {
                        let edge = if terminal {
                            EdgeKind::Terminal
                        } else if next.is_none() {
                            EdgeKind::EndOfChain // defensive: validation makes this unreachable
                        } else if port == "success" {
                            EdgeKind::Success
                        } else {
                            EdgeKind::Outcome
                        };
                        r.record_step(
                            &current_node_id,
                            &node_type,
                            StepOutcome::Success,
                            elapsed,
                            edge,
                            (port != "success").then_some(port),
                            next.map(String::as_str),
                            &ctx,
                        );
                    }
                    match next {
                        Some(next_id) => current_node_id = next_id.clone(),
                        // Terminal, or no edge for this port — end of chain.
                        None => break,
                    }
                }
                Err(mut err) => {
                    if let Some(ref m) = self.resources.metrics {
                        m.node_errors
                            .with_label_values(&[
                                &self.policy_name,
                                &current_node_id,
                                &err.error.code,
                            ])
                            .inc();
                    }

                    // Tag the error with the node_id that produced it
                    err.error.node_id = current_node_id.clone();
                    let outcome = StepOutcome::Error {
                        code: err.error.code.clone(),
                        message: err.error.message.clone(),
                    };
                    err.context.errors.push(err.error);
                    ctx = err.context;

                    // Try per-node error edge first, then the policy catch-all.
                    let next_error = self
                        .edges
                        .get(&current_node_id)
                        .and_then(|m| m.get("error"))
                        .map(|id| (id.clone(), EdgeKind::Error))
                        .or_else(|| {
                            self.catch_all_handler
                                .as_ref()
                                .map(|id| (id.clone(), EdgeKind::CatchAll))
                        });

                    if next_error.is_none() {
                        // No error handling — return 500
                        ctx.response.status_code = 500;
                        ctx.response.body = bytes::Bytes::from(
                            r#"{"error": "internal_error", "message": "Unhandled error in routing policy"}"#,
                        );
                        ctx.response.headers.insert(
                            "content-type".to_string(),
                            vec!["application/json".to_string()],
                        );
                    }

                    if let Some(r) = recorder.as_mut() {
                        let (edge, next_id) = match &next_error {
                            Some((id, kind)) => (*kind, Some(id.as_str())),
                            None => (EdgeKind::Unhandled, None),
                        };
                        r.record_step(
                            &current_node_id,
                            &node_type,
                            outcome,
                            elapsed,
                            edge,
                            None,
                            next_id,
                            &ctx,
                        );
                    }

                    match next_error {
                        Some((id, _)) => current_node_id = id,
                        None => break,
                    }
                }
            }
        }

        (ctx, recorder)
    }
}

/// Compiles a [`PolicyConfig`] into a ready-to-execute [`CompiledGraph`].
///
/// Instantiates each node's plugin via `plugins::create_plugin`, records
/// `client` nodes as terminals, and indexes edges per node by source port
/// (`out` normalizes to `success`). The entry node is the target of the
/// listener's success/out edge. Fails if the policy has no `listener` node,
/// a plugin cannot be constructed, an edge names a port its node type does
/// not declare, two edges leave the same `node.port` (fan-out is not
/// supported), or a node's `success`/outcome port ([`PortKind::Success`] or
/// [`PortKind::Outcome`]) has no outgoing edge — `error` ports are exempt
/// (fallback chain: per-node error edge -> policy catch-all -> default 500).
///
/// [`PortKind::Success`]: crate::plugins::ports::PortKind::Success
/// [`PortKind::Outcome`]: crate::plugins::ports::PortKind::Outcome
///
/// Edge endpoints use the `node_id.port` form:
///
/// ```yaml
/// edges:
///   - from: listener.out
///     to: rewrite.in
///   - from: rewrite.success
///     to: client.in
///   - from: rewrite.error
///     to: error-handler.in
/// ```
pub fn compile_policy(
    policy: &PolicyConfig,
    resources: Arc<PluginResources>,
) -> Result<CompiledGraph, String> {
    let mut nodes: HashMap<String, Box<dyn Plugin>> = HashMap::new();
    let mut listener_node_id = None;
    let mut terminal_node_ids = HashSet::new();

    // Instantiate all nodes
    for node_config in &policy.nodes {
        tracing::debug!(
            "Compiling node '{}' type='{}' config={:?}",
            node_config.id,
            node_config.node_type,
            node_config.config
        );
        // Interpolate `${ENV_VAR}` in the node config before instantiating the
        // plugin. This is the source-agnostic choke point: config authored in the
        // Web UI / Admin API (or delivered over etcd) arrives as parsed JSON and
        // never sees the file-text interpolation in `load_yaml_with_env`, so we
        // resolve env vars here. File-loaded values are already resolved (no-op).
        let mut config = node_config.config.clone();
        for value in config.values_mut() {
            crate::config::interpolate_env_json(value);
        }
        let plugin = plugins::create_plugin(&node_config.node_type, &config, &resources)?;
        if node_config.node_type == "listener" {
            listener_node_id = Some(node_config.id.clone());
        }
        if node_config.node_type == "client" {
            terminal_node_ids.insert(node_config.id.clone());
        }
        nodes.insert(node_config.id.clone(), plugin);
    }

    let listener_node_id = listener_node_id.ok_or("Policy must have a listener node")?;

    let node_types: HashMap<String, String> = policy
        .nodes
        .iter()
        .map(|n| (n.id.clone(), n.node_type.clone()))
        .collect();

    // Parse edges, indexed by source port. `out` normalizes to `success`.
    let mut edges: HashMap<String, HashMap<String, String>> = HashMap::new();
    for edge in &policy.edges {
        let (from_node, from_port) = parse_edge_endpoint(&edge.from)?;
        let (to_node, _to_port) = parse_edge_endpoint(&edge.to)?;
        let from_port = if from_port == "out" {
            "success".to_string()
        } else {
            from_port
        };

        let node_type = node_types.get(&from_node).ok_or_else(|| {
            format!(
                "policy '{}': edge references unknown node '{}'",
                policy.name, from_node
            )
        })?;
        let spec = plugins::port_spec(node_type)
            .ok_or_else(|| format!("Unknown plugin type: {}", node_type))?;
        if !spec.outputs.iter().any(|p| p.name == from_port) {
            return Err(format!(
                "policy '{}': node '{}' (type '{}') has no output port '{}'",
                policy.name, from_node, node_type, from_port
            ));
        }
        if edges
            .entry(from_node.clone())
            .or_default()
            .insert(from_port.clone(), to_node)
            .is_some()
        {
            return Err(format!(
                "policy '{}': duplicate edge from '{}.{}' — fan-out is not supported",
                policy.name, from_node, from_port
            ));
        }
    }

    // Mandatory wiring: every success/outcome port of every node must have an edge.
    for node_config in &policy.nodes {
        let spec = plugins::port_spec(&node_config.node_type)
            .ok_or_else(|| format!("Unknown plugin type: {}", node_config.node_type))?;
        for p in spec.outputs {
            if matches!(p.kind, plugins::ports::PortKind::Error) {
                continue;
            }
            let wired = edges
                .get(&node_config.id)
                .is_some_and(|m| m.contains_key(p.name));
            if !wired {
                return Err(format!(
                    "policy '{}': output port '{}' of node '{}' (type '{}') must be wired — add an edge from '{}.{}'",
                    policy.name, p.name, node_config.id, node_config.node_type, node_config.id, p.name
                ));
            }
        }
    }

    // The entry node is the first node connected from the listener's success/out edge
    let entry_node_id = edges
        .get(&listener_node_id)
        .and_then(|m| m.get("success"))
        .cloned()
        .unwrap_or_else(|| listener_node_id.clone());

    Ok(CompiledGraph {
        nodes,
        edges,
        entry_node_id,
        terminal_node_ids,
        catch_all_handler: policy.error_handler.clone(),
        policy_name: policy.name.clone(),
        resources,
    })
}

/// Parses `"node_id.port"` into `(node_id, port)`, defaulting the port to
/// `"out"` when no dot is present. Splits on the last dot, so node IDs may
/// themselves contain dots.
fn parse_edge_endpoint(endpoint: &str) -> Result<(String, String), String> {
    if let Some(dot_pos) = endpoint.rfind('.') {
        let node_id = endpoint[..dot_pos].to_string();
        let port = endpoint[dot_pos + 1..].to_string();
        Ok((node_id, port))
    } else {
        Ok((endpoint.to_string(), "out".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EdgeConfig, NodeConfig, PolicyConfig};
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    #[test]
    fn test_parse_edge_endpoint() {
        let (node, port) = parse_edge_endpoint("listener.out").unwrap();
        assert_eq!(node, "listener");
        assert_eq!(port, "out");

        let (node, port) = parse_edge_endpoint("upstream.error").unwrap();
        assert_eq!(node, "upstream");
        assert_eq!(port, "error");

        let (node, port) = parse_edge_endpoint("rewrite.success").unwrap();
        assert_eq!(node, "rewrite");
        assert_eq!(port, "success");
    }

    fn test_context(path: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: path.to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "127.0.0.1:12345".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_graph_proxy_rewrite_pipeline() {
        let mut rewrite_config = HashMap::new();
        rewrite_config.insert(
            "strip_path_prefix".to_string(),
            serde_json::Value::String("/api/v1".to_string()),
        );
        rewrite_config.insert(
            "phase".to_string(),
            serde_json::Value::String("request".to_string()),
        );

        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![
                NodeConfig {
                    id: "listener".to_string(),
                    node_type: "listener".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "rewrite".to_string(),
                    node_type: "proxy-rewrite".to_string(),
                    config: rewrite_config,
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "client".to_string(),
                    node_type: "client".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
            ],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "rewrite.in".to_string(),
                },
                EdgeConfig {
                    from: "rewrite.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };

        let graph = compile_policy(&policy, PluginResources::empty()).unwrap();
        let ctx = test_context("/api/v1/users");
        let result = graph.execute(ctx).await;

        assert_eq!(result.request.path, "/users");
    }

    #[tokio::test]
    async fn test_graph_records_node_metrics() {
        let policy = PolicyConfig {
            name: "metrics-test".to_string(),
            error_handler: None,
            nodes: vec![
                NodeConfig {
                    id: "listener".to_string(),
                    node_type: "listener".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "client".to_string(),
                    node_type: "client".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
            ],
            edges: vec![EdgeConfig {
                from: "listener.out".to_string(),
                to: "client.in".to_string(),
            }],
        };

        let metrics = Arc::new(crate::metrics::GatewayMetrics::new());
        let graph = compile_policy(&policy, PluginResources::new(Some(metrics.clone()))).unwrap();
        graph.execute(test_context("/test")).await;

        assert_eq!(
            metrics
                .node_execution_count
                .with_label_values(&["metrics-test", "client", "client"])
                .get(),
            1
        );
    }

    /// The policy-level catch-all: a node whose own `error` port is unwired
    /// falls through to the named handler, which then renders the error.
    ///
    /// The failing node is a real one (an `upstream` pointed at a closed port),
    /// not a bare `listener -> error-handler` chain: since the port migration,
    /// `error-handler` passes an **errorless** context straight through, so a
    /// handler reached without a preceding failure has nothing to render.
    #[tokio::test]
    async fn test_graph_error_handler_catch_all() {
        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: Some("error-handler".to_string()),
            nodes: vec![
                NodeConfig {
                    id: "listener".to_string(),
                    node_type: "listener".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "backend".to_string(),
                    node_type: "upstream".to_string(),
                    config: {
                        let mut c = HashMap::new();
                        // Nothing listens on port 1: connect fails instantly.
                        c.insert(
                            "targets".to_string(),
                            serde_json::json!([{ "host": "127.0.0.1", "port": 1 }]),
                        );
                        c.insert("timeout_ms".to_string(), serde_json::json!(500));
                        c
                    },
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "error-handler".to_string(),
                    node_type: "error-handler".to_string(),
                    config: {
                        let mut c = HashMap::new();
                        c.insert("status_code".to_string(), serde_json::json!(503));
                        c.insert(
                            "body_template".to_string(),
                            serde_json::Value::String(r#"{"error": "{{error.code}}"}"#.to_string()),
                        );
                        c
                    },
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "client".to_string(),
                    node_type: "client".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
            ],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "backend.in".to_string(),
                },
                // backend.error is deliberately unwired -> catch-all.
                EdgeConfig {
                    from: "backend.success".to_string(),
                    to: "client.in".to_string(),
                },
                EdgeConfig {
                    from: "error-handler.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };

        let graph = compile_policy(&policy, PluginResources::empty()).unwrap();
        let ctx = test_context("/test");
        let result = graph.execute(ctx).await;

        assert_eq!(result.response.status_code, 503);
        let body = String::from_utf8(result.response.body.to_vec()).unwrap();
        assert!(!body.contains("{{"), "template must be rendered: {body}");
    }

    /// The complement of the test above: an `error-handler` reached with an
    /// **empty** `context.errors` — what every outcome exit looks like — must
    /// leave the already-prepared response alone instead of clobbering it with
    /// its own status and an unrendered template.
    #[tokio::test]
    async fn test_error_handler_passes_errorless_context_through() {
        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![
                NodeConfig {
                    id: "listener".to_string(),
                    node_type: "listener".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "error-handler".to_string(),
                    node_type: "error-handler".to_string(),
                    config: {
                        let mut c = HashMap::new();
                        c.insert("status_code".to_string(), serde_json::json!(503));
                        c.insert(
                            "body_template".to_string(),
                            serde_json::Value::String(r#"{"error": "{{error.code}}"}"#.to_string()),
                        );
                        c
                    },
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "client".to_string(),
                    node_type: "client".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
            ],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "error-handler.in".to_string(),
                },
                EdgeConfig {
                    from: "error-handler.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };

        let graph = compile_policy(&policy, PluginResources::empty()).unwrap();
        let mut ctx = test_context("/test");
        // Stand in for an outcome exit: a fully prepared 401, no error record.
        ctx.response.status_code = 401;
        ctx.response.body = Bytes::from(r#"{"error":"unauthorized"}"#);
        let result = graph.execute(ctx).await;

        assert_eq!(result.response.status_code, 401);
        assert_eq!(
            String::from_utf8(result.response.body.to_vec()).unwrap(),
            r#"{"error":"unauthorized"}"#
        );
    }

    // ---- debug tracing ---------------------------------------------------

    use crate::debug::{CaptureOptions, EdgeKind, StepOutcome, TraceRecorder, TraceSource};
    use std::time::Duration;

    fn recorder(ctx: &Context) -> TraceRecorder {
        TraceRecorder::new(ctx, CaptureOptions::default(), 100)
    }

    fn finish(rec: TraceRecorder, ctx: &Context) -> crate::debug::Trace {
        rec.finish(
            "t".to_string(),
            0,
            TraceSource::Request,
            Some("r".to_string()),
            "p".to_string(),
            ctx,
            Duration::from_millis(1),
        )
    }

    /// A node that always fails, so error-edge routing can be exercised without
    /// depending on a real plugin's failure mode.
    struct AlwaysFails;

    #[async_trait::async_trait]
    impl Plugin for AlwaysFails {
        fn plugin_type(&self) -> &str {
            "always-fails"
        }
        async fn execute(&self, ctx: Context) -> crate::plugins::PluginResult {
            Err(crate::plugins::PluginExecutionError {
                context: ctx,
                error: crate::context::GatewayError {
                    node_id: String::new(),
                    code: "BOOM".to_string(),
                    message: "exploded".to_string(),
                    metadata: HashMap::new(),
                },
            })
        }
    }

    /// Builds a graph by hand so a failing node can be injected.
    fn failing_graph(
        error_edges: HashMap<String, String>,
        catch_all: Option<String>,
    ) -> CompiledGraph {
        let mut nodes: HashMap<String, Box<dyn Plugin>> = HashMap::new();
        nodes.insert("boom".to_string(), Box::new(AlwaysFails));
        nodes.insert(
            "client".to_string(),
            plugins::create_plugin("client", &HashMap::new(), &PluginResources::empty()).unwrap(),
        );
        let mut edges: HashMap<String, HashMap<String, String>> = HashMap::new();
        if let Some(target) = error_edges.get("boom") {
            edges
                .entry("boom".to_string())
                .or_default()
                .insert("error".to_string(), target.clone());
        }
        CompiledGraph {
            nodes,
            edges,
            entry_node_id: "boom".to_string(),
            terminal_node_ids: HashSet::from(["client".to_string()]),
            catch_all_handler: catch_all,
            policy_name: "p".to_string(),
            resources: PluginResources::empty(),
        }
    }

    fn rewrite_policy() -> PolicyConfig {
        let mut cfg = HashMap::new();
        cfg.insert(
            "strip_path_prefix".to_string(),
            serde_json::Value::String("/api/v1".to_string()),
        );
        cfg.insert(
            "phase".to_string(),
            serde_json::Value::String("request".to_string()),
        );
        PolicyConfig {
            name: "traced".to_string(),
            error_handler: None,
            nodes: vec![
                NodeConfig {
                    id: "listener".to_string(),
                    node_type: "listener".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "rewrite".to_string(),
                    node_type: "proxy-rewrite".to_string(),
                    config: cfg,
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "client".to_string(),
                    node_type: "client".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
            ],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "rewrite.in".to_string(),
                },
                EdgeConfig {
                    from: "rewrite.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn test_trace_records_each_node_in_order() {
        let graph = compile_policy(&rewrite_policy(), PluginResources::empty()).unwrap();
        let ctx = test_context("/api/v1/users");
        let rec = recorder(&ctx);
        let (out, rec) = graph.execute_traced(ctx, rec).await;
        let trace = finish(rec, &out);

        let ids: Vec<&str> = trace.steps.iter().map(|s| s.node_id.as_str()).collect();
        assert_eq!(ids, vec!["rewrite", "client"]);
        assert_eq!(trace.steps[0].node_type, "proxy-rewrite");
        assert_eq!(trace.steps[0].edge, EdgeKind::Success);
        assert_eq!(trace.steps[0].next_node_id.as_deref(), Some("client"));
        // The terminal node ends the walk.
        assert_eq!(trace.steps[1].edge, EdgeKind::Terminal);
        assert_eq!(trace.steps[1].next_node_id, None);
    }

    /// The trace must show the rewrite: initial path in, rewritten path out.
    #[tokio::test]
    async fn test_trace_captures_what_the_plugin_changed() {
        let graph = compile_policy(&rewrite_policy(), PluginResources::empty()).unwrap();
        let ctx = test_context("/api/v1/users");
        let rec = recorder(&ctx);
        let (out, rec) = graph.execute_traced(ctx, rec).await;
        let trace = finish(rec, &out);

        assert_eq!(trace.initial.request.path, "/api/v1/users");
        assert_eq!(trace.steps[0].after.request.path, "/users");

        let changes = crate::debug::diff::diff(&trace.initial, &trace.steps[0].after);
        let c = changes
            .iter()
            .find(|c| c.path == "request.path")
            .expect("path change");
        assert_eq!(c.before.as_deref(), Some("/api/v1/users"));
        assert_eq!(c.after.as_deref(), Some("/users"));
    }

    /// Tracing must never change what the gateway does. If this ever fails,
    /// the debug feature has become a heisenbug generator.
    #[tokio::test]
    async fn test_tracing_does_not_alter_behaviour() {
        let policy = rewrite_policy();
        let plain = compile_policy(&policy, PluginResources::empty()).unwrap();
        let traced = compile_policy(&policy, PluginResources::empty()).unwrap();

        let untraced_out = plain.execute(test_context("/api/v1/users")).await;
        let ctx = test_context("/api/v1/users");
        let rec = recorder(&ctx);
        let (traced_out, _) = traced.execute_traced(ctx, rec).await;

        assert_eq!(untraced_out.request.path, traced_out.request.path);
        assert_eq!(
            untraced_out.response.status_code,
            traced_out.response.status_code
        );
        assert_eq!(untraced_out.response.body, traced_out.response.body);
        assert_eq!(untraced_out.message, traced_out.message);
        assert_eq!(untraced_out.errors, traced_out.errors);
    }

    #[tokio::test]
    async fn test_trace_records_error_edge() {
        let graph = failing_graph(
            HashMap::from([("boom".to_string(), "client".to_string())]),
            None,
        );
        let ctx = test_context("/x");
        let rec = recorder(&ctx);
        let (out, rec) = graph.execute_traced(ctx, rec).await;
        let trace = finish(rec, &out);

        assert_eq!(trace.steps[0].node_id, "boom");
        assert_eq!(
            trace.steps[0].outcome,
            StepOutcome::Error {
                code: "BOOM".to_string(),
                message: "exploded".to_string()
            }
        );
        assert_eq!(trace.steps[0].edge, EdgeKind::Error);
        assert_eq!(trace.steps[0].next_node_id.as_deref(), Some("client"));
        // The error was recorded on the context the step captured.
        assert_eq!(trace.steps[0].after.errors.len(), 1);
        assert_eq!(trace.steps[0].after.errors[0].node_id, "boom");
    }

    #[tokio::test]
    async fn test_trace_records_catch_all_edge() {
        let graph = failing_graph(HashMap::new(), Some("client".to_string()));
        let ctx = test_context("/x");
        let rec = recorder(&ctx);
        let (out, rec) = graph.execute_traced(ctx, rec).await;
        let trace = finish(rec, &out);
        assert_eq!(trace.steps[0].edge, EdgeKind::CatchAll);
    }

    /// The unwired-error-port case: this is the footgun the sandbox's
    /// `on_error: "stop"` mode deliberately exposes.
    #[tokio::test]
    async fn test_trace_records_unhandled_error() {
        let graph = failing_graph(HashMap::new(), None);
        let ctx = test_context("/x");
        let rec = recorder(&ctx);
        let (out, rec) = graph.execute_traced(ctx, rec).await;
        let trace = finish(rec, &out);

        assert_eq!(trace.steps.len(), 1);
        assert_eq!(trace.steps[0].edge, EdgeKind::Unhandled);
        assert_eq!(trace.steps[0].next_node_id, None);
        // ...and the engine wrote its generic 500.
        assert_eq!(out.response.status_code, 500);
        assert_eq!(trace.steps[0].after.response.status_code, 500);
    }

    // ---- per-port outcome routing -----------------------------------------

    /// A test-only plugin that emits on the "denied" outcome port when the
    /// request carries `x-deny`, else the normal success port. Stands in for
    /// a real outcome-port plugin, which doesn't exist until Task 8.
    struct DenyOnHeader;

    #[async_trait::async_trait]
    impl Plugin for DenyOnHeader {
        fn plugin_type(&self) -> &str {
            "deny-on-header"
        }
        async fn execute(&self, ctx: Context) -> crate::plugins::PluginResult {
            if ctx.request.headers.contains_key("x-deny") {
                Ok(plugins::PluginOutput::on_port(ctx, "denied"))
            } else {
                Ok(plugins::PluginOutput::success(ctx))
            }
        }
    }

    /// Builds a graph by hand with a per-port edge map, so outcome-port
    /// routing can be exercised without a real plugin type declaring one.
    fn outcome_graph() -> CompiledGraph {
        let mut nodes: HashMap<String, Box<dyn Plugin>> = HashMap::new();
        nodes.insert("n".to_string(), Box::new(DenyOnHeader));
        nodes.insert(
            "client".to_string(),
            plugins::create_plugin("client", &HashMap::new(), &PluginResources::empty()).unwrap(),
        );
        nodes.insert(
            "deny-client".to_string(),
            plugins::create_plugin("client", &HashMap::new(), &PluginResources::empty()).unwrap(),
        );
        let mut n_edges = HashMap::new();
        n_edges.insert("denied".to_string(), "deny-client".to_string());
        n_edges.insert("success".to_string(), "client".to_string());
        let mut edges = HashMap::new();
        edges.insert("n".to_string(), n_edges);
        CompiledGraph {
            nodes,
            edges,
            entry_node_id: "n".to_string(),
            terminal_node_ids: HashSet::from(["client".to_string(), "deny-client".to_string()]),
            catch_all_handler: None,
            policy_name: "p".to_string(),
            resources: PluginResources::empty(),
        }
    }

    /// A named outcome port routes to its wired target, not to success.
    #[tokio::test]
    async fn test_outcome_port_routes_to_its_edge() {
        let graph = outcome_graph();

        let mut ctx = test_context("/x");
        ctx.request
            .headers
            .insert("x-deny".to_string(), vec!["1".to_string()]);
        let rec = recorder(&ctx);
        let (out, rec) = graph.execute_traced(ctx, rec).await;
        let trace = finish(rec, &out);

        assert_eq!(trace.steps[0].node_id, "n");
        assert_eq!(trace.steps[0].edge, EdgeKind::Outcome);
        assert_eq!(trace.steps[0].port.as_deref(), Some("denied"));
        assert_eq!(trace.steps[0].next_node_id.as_deref(), Some("deny-client"));
        // The walk actually ended at deny-client, not client.
        assert_eq!(trace.steps.len(), 2);
        assert_eq!(trace.steps[1].node_id, "deny-client");
    }

    /// Without the header, the same node exits through `success` as usual.
    #[tokio::test]
    async fn test_outcome_port_falls_back_to_success() {
        let graph = outcome_graph();
        let ctx = test_context("/x");
        let rec = recorder(&ctx);
        let (out, rec) = graph.execute_traced(ctx, rec).await;
        let trace = finish(rec, &out);

        assert_eq!(trace.steps[0].edge, EdgeKind::Success);
        assert!(trace.steps[0].port.is_none());
        assert_eq!(trace.steps[0].next_node_id.as_deref(), Some("client"));
    }

    /// A real `cors` node, compiled through `compile_policy` (not a hand-built
    /// graph), with its `preflight` port wired to `client`. An OPTIONS
    /// preflight from an allowed origin must exit on the `preflight` outcome
    /// port — not `success` — with the prepared 204 reaching the final
    /// context, exercising the whole compile+execute+trace path end-to-end
    /// for an outcome-port node.
    fn cors_policy() -> PolicyConfig {
        let mut cfg = HashMap::new();
        cfg.insert(
            "allowed_origins".to_string(),
            serde_json::json!(["http://sub.domain.com"]),
        );
        PolicyConfig {
            name: "cors-preflight".to_string(),
            error_handler: None,
            nodes: vec![
                NodeConfig {
                    id: "listener".to_string(),
                    node_type: "listener".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "cors".to_string(),
                    node_type: "cors".to_string(),
                    config: cfg,
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "client".to_string(),
                    node_type: "client".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
            ],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "cors.in".to_string(),
                },
                EdgeConfig {
                    from: "cors.success".to_string(),
                    to: "client.in".to_string(),
                },
                EdgeConfig {
                    from: "cors.preflight".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn test_cors_preflight_exits_via_preflight_edge_end_to_end() {
        let graph = compile_policy(&cors_policy(), PluginResources::empty()).unwrap();

        let mut ctx = test_context("/x");
        ctx.request.method = "OPTIONS".to_string();
        ctx.request.headers.insert(
            "origin".to_string(),
            vec!["http://sub.domain.com".to_string()],
        );
        let rec = recorder(&ctx);
        let (out, rec) = graph.execute_traced(ctx, rec).await;
        let trace = finish(rec, &out);

        assert_eq!(trace.steps[0].node_id, "cors");
        assert_eq!(trace.steps[0].edge, EdgeKind::Outcome);
        assert_eq!(trace.steps[0].port.as_deref(), Some("preflight"));
        assert_eq!(trace.steps[0].next_node_id.as_deref(), Some("client"));
        assert_eq!(out.response.status_code, 204);
        assert_eq!(
            out.response.headers.get("access-control-allow-origin"),
            Some(&vec!["http://sub.domain.com".to_string()])
        );
    }

    // ---- compile-time mandatory-wiring validation -------------------------

    fn policy_missing_success_edge() -> PolicyConfig {
        PolicyConfig {
            name: "p".to_string(),
            error_handler: None,
            nodes: vec![
                NodeConfig {
                    id: "listener".to_string(),
                    node_type: "listener".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "rw".to_string(),
                    node_type: "proxy-rewrite".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
                NodeConfig {
                    id: "client".to_string(),
                    node_type: "client".to_string(),
                    config: HashMap::new(),
                    config_ref: None,
                    position: None,
                },
            ],
            edges: vec![EdgeConfig {
                from: "listener.out".to_string(),
                to: "rw.in".to_string(),
            }],
        }
    }

    /// Unwired mandatory port fails compilation with the exact message.
    #[test]
    fn test_compile_rejects_unwired_mandatory_port() {
        let policy = policy_missing_success_edge();
        let err = compile_policy(&policy, PluginResources::empty()).unwrap_err();
        assert_eq!(
            err,
            "policy 'p': output port 'success' of node 'rw' (type 'proxy-rewrite') must be wired — add an edge from 'rw.success'"
        );
    }

    /// Duplicate (node, port) edge is an explicit error, not a silent overwrite.
    #[test]
    fn test_compile_rejects_fanout() {
        let mut policy = policy_missing_success_edge();
        policy.edges.push(EdgeConfig {
            from: "rw.success".to_string(),
            to: "client.in".to_string(),
        });
        policy.edges.push(EdgeConfig {
            from: "rw.success".to_string(),
            to: "client.in".to_string(),
        });
        let err = compile_policy(&policy, PluginResources::empty()).unwrap_err();
        assert_eq!(
            err,
            "policy 'p': duplicate edge from 'rw.success' — fan-out is not supported"
        );
    }

    /// Edge from a port the type does not declare.
    #[test]
    fn test_compile_rejects_undeclared_port() {
        let mut policy = policy_missing_success_edge();
        policy.edges.push(EdgeConfig {
            from: "rw.banana".to_string(),
            to: "client.in".to_string(),
        });
        let err = compile_policy(&policy, PluginResources::empty()).unwrap_err();
        assert_eq!(
            err,
            "policy 'p': node 'rw' (type 'proxy-rewrite') has no output port 'banana'"
        );
    }

    /// error port stays optional: policy with no error edges still compiles.
    #[test]
    fn test_error_port_wiring_is_optional() {
        let mut policy = policy_missing_success_edge();
        policy.edges.push(EdgeConfig {
            from: "rw.success".to_string(),
            to: "client.in".to_string(),
        });
        assert!(compile_policy(&policy, PluginResources::empty()).is_ok());
    }
}
