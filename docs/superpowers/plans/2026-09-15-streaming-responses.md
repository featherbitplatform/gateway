# Streaming Responses Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let featherbit relay a streaming upstream response (Server-Sent Events, chunked feeds) to the client unbuffered, decided automatically at policy-compile time.

**Architecture:** The upstream response body becomes an out-of-band `#[serde(skip)]` stream handle on `GatewayResponse`, so `Context` stays serializable for Lua and debug snapshots — the same pattern the WebSocket upgrade handle already uses. The policy compiler walks each `upstream` node's `success` path and marks it stream-capable only when every node between it and `client` declares it does not read the response body. The listener's response type widens from `Full<Bytes>` to `BoxBody`.

**Tech Stack:** Rust, hyper 1 + http-body-util 0.1 (`BoxBody`, `StreamBody`), tokio.

**Spec:** `docs/superpowers/specs/2026-09-15-streaming-responses-design.md`

## Global Constraints

- **Buffered responses must not change at all.** Same bytes, same `content-length`, same `timeout_ms` whole-call semantics. Any observable change to a non-streaming response is a bug.
- **`Context` must stay `Serialize`/`Deserialize`.** The stream handle is `#[serde(skip)]`. It marshals to Lua and into debug snapshots; a field that breaks that breaks both.
- **Invariant:** `response.stream.is_some()` implies `response.body` is empty. Exactly one carries content.
- **Inference walks the `success` port only.** The `error` path is taken when the upstream produced no body, so nodes there must never force the success path to buffer.
- **Inference is conservative.** `Plugin::reads_response_body()` defaults to `true`; a plugin that does not opt out forces buffering. Config presence decides, not runtime behavior.
- **Never call `reload_config`** against a live instance during manual verification; it discards live edits.
- **Do not `git commit`** until the operator gives an explicit go-ahead (standing rule). Commit steps below are written but gated on that.
- Existing suite is **1242 tests passing**; `cargo fmt --check` and `cargo clippy --all-targets` must stay clean.

---

### Task 1: Data model — `ResponseStream` and the `stream` field

**Files:**
- Modify: `src/context/mod.rs` (the `GatewayResponse` struct, ~line 58)
- Create: `src/context/stream.rs`
- Test: inline `#[cfg(test)] mod tests` in `src/context/stream.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `pub struct ResponseStream` with `ResponseStream::new(body: BoxBody<Bytes, hyper::Error>) -> Self`, `ResponseStream::hold(&mut self, guard: Box<dyn Send + 'static>)`, and `ResponseStream::into_parts(self) -> (BoxBody<Bytes, hyper::Error>, Vec<Box<dyn Send + 'static>>)`. `GatewayResponse.stream: Option<ResponseStream>`. Used by Tasks 2, 4, 5, 6.

- [ ] **Step 1: Write the failing test**

Create `src/context/stream.rs` with only the test module at first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    fn boxed(text: &str) -> BoxBody<Bytes, hyper::Error> {
        Full::new(Bytes::from(text.to_owned()))
            .map_err(|never| match never {})
            .boxed()
    }

    /// The stream handle must carry arbitrary guards (balancer in-flight,
    /// limit-conn) so they release when the stream ends, not when the node
    /// that created it returns.
    #[tokio::test]
    async fn test_stream_holds_guards_until_dropped() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicUsize::new(0));
        let mut stream = ResponseStream::new(boxed("data: hello\n\n"));
        stream.hold(Box::new(Guard(dropped.clone())));

        assert_eq!(dropped.load(Ordering::SeqCst), 0, "guard released too early");
        drop(stream);
        assert_eq!(dropped.load(Ordering::SeqCst), 1, "guard not released on drop");
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test context::stream`
Expected: FAIL to compile — `ResponseStream` does not exist. That is the correct red for a new type.

- [ ] **Step 3: Implement `ResponseStream`**

Put this above the test module in `src/context/stream.rs`:

```rust
//! The out-of-band streaming response body.
//!
//! A streaming body cannot live on `Context` as an ordinary field: `Context`
//! is `Serialize`/`Deserialize` because it marshals into Lua scripts and into
//! debug trace snapshots. The handle is therefore `#[serde(skip)]`, the same
//! way the WebSocket upgrade handle is carried out-of-band by the listener.

use bytes::Bytes;
use http_body_util::combinators::BoxBody;

/// An upstream response body being relayed to the client unbuffered, plus any
/// guards whose lifetime must match the stream rather than the node that
/// produced it (balancer in-flight counters, `limit-conn` permits).
pub struct ResponseStream {
    body: BoxBody<Bytes, hyper::Error>,
    guards: Vec<Box<dyn Send + 'static>>,
}

impl ResponseStream {
    pub fn new(body: BoxBody<Bytes, hyper::Error>) -> Self {
        Self {
            body,
            guards: Vec::new(),
        }
    }

    /// Attaches a guard released when the stream is consumed or dropped.
    pub fn hold(&mut self, guard: Box<dyn Send + 'static>) {
        self.guards.push(guard);
    }

    /// Takes the body for transmission. Guards travel with the returned body's
    /// owner, so the caller must keep `self` alive until the body is finished.
    pub fn into_parts(self) -> (BoxBody<Bytes, hyper::Error>, Vec<Box<dyn Send + 'static>>) {
        (self.body, self.guards)
    }
}

// `Box<dyn Send>` has no Debug; the struct is only ever shown as a marker.
impl std::fmt::Debug for ResponseStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseStream")
            .field("guards", &self.guards.len())
            .finish()
    }
}
```

`into_parts` is the accessor Task 6 uses; there is deliberately no `into_body`, because the guards must be handed over alongside the body rather than dropped.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test context::stream`
Expected: PASS.

- [ ] **Step 5: Add the field to `GatewayResponse`**

In `src/context/mod.rs`, add `pub mod stream;` near the top and extend the struct:

```rust
    /// Streaming body, when the upstream response is relayed unbuffered.
    /// Skipped by serde: `Context` must stay serializable for Lua marshalling
    /// and debug snapshots. Invariant: when this is `Some`, `body` is empty.
    #[serde(skip)]
    pub stream: Option<crate::context::stream::ResponseStream>,
```

`GatewayResponse` derives `Debug, Clone, Serialize, Deserialize`. `ResponseStream` is **not** `Clone`, so `GatewayResponse`'s `Clone` derive will fail to compile. Replace the derived `Clone` with a manual impl that clones everything and sets `stream: None`:

```rust
impl Clone for GatewayResponse {
    /// Cloning drops any stream: a stream has exactly one consumer, and every
    /// caller that clones a response (debug snapshots, cache stores) wants the
    /// buffered form.
    fn clone(&self) -> Self {
        Self {
            status_code: self.status_code,
            headers: self.headers.clone(),
            body: self.body.clone(),
            stream: None,
        }
    }
}
```

Also add `stream: None` to every `GatewayResponse { .. }` literal — `Context::new` in `src/context/mod.rs` is one; `cargo check` will name the rest.

- [ ] **Step 6: Verify nothing else broke**

Run: `cargo test`
Expected: 1243 passed (1242 + the new one), 0 failed.

- [ ] **Step 7: Commit (gated on operator go-ahead)**

```bash
git add src/context/mod.rs src/context/stream.rs
git commit -m "feat(context): out-of-band streaming response body handle"
```

---

### Task 2: Outbound client — a call that returns after headers

**Files:**
- Modify: `src/outbound/mod.rs` (`OutboundResponse` ~line 58, the call body ~line 148-172)
- Test: inline tests in `src/outbound/mod.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks
- Produces: `OutboundClient::request_streaming(&self, req: OutboundRequest) -> Result<OutboundStreamingResponse, OutboundError>` where `pub struct OutboundStreamingResponse { pub status: u16, pub headers: HashMap<String, Vec<String>>, pub body: BoxBody<Bytes, hyper::Error> }`. Used by Task 5.

- [ ] **Step 1: Write the failing test**

Add to the tests module in `src/outbound/mod.rs`. It uses a server that sends headers, then stalls before the body — the existing buffered `request()` would block until the deadline; `request_streaming` must return as soon as headers arrive.

```rust
    /// The streaming call's deadline covers connect + request + response
    /// headers only. A server that sends headers and then stalls must still
    /// yield a response promptly — the buffered `request()` would block on the
    /// body until the whole-call deadline expired.
    #[tokio::test]
    async fn test_request_streaming_returns_after_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Headers only, chunked, then hold the connection open.
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                          transfer-encoding: chunked\r\n\r\n",
                    )
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });

        let client = OutboundClient::new();
        let req = OutboundRequest {
            method: http::Method::GET,
            url: format!("http://127.0.0.1:{port}/stream"),
            headers: Vec::new(),
            body: Bytes::new(),
            timeout: std::time::Duration::from_secs(5),
            ssl_verify: true,
            tls: None,
        };

        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.request_streaming(req),
        )
        .await
        .expect("request_streaming must return before the body arrives")
        .expect("streaming call succeeded");

        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.headers.get("content-type").map(|v| v[0].as_str()),
            Some("text/event-stream")
        );
    }
```

If `OutboundClient::new()` is not the real constructor, use whatever the existing tests in this file use — copy their setup exactly.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test test_request_streaming_returns_after_headers`
Expected: FAIL to compile — `request_streaming` does not exist.

- [ ] **Step 3: Implement the streaming call**

Add the response type next to `OutboundResponse`:

```rust
/// A response whose headers have arrived and whose body is still streaming.
pub struct OutboundStreamingResponse {
    pub status: u16,
    pub headers: HashMap<String, Vec<String>>,
    pub body: BoxBody<Bytes, hyper::Error>,
}
```

Add the method to `OutboundClient`, mirroring `request()` but applying the deadline only to the header phase:

```rust
    /// Like [`request`](Self::request) but returns as soon as the response
    /// headers arrive, leaving the body to stream. `req.timeout` bounds
    /// connect + request + headers; the caller owns any idle bound on the body.
    pub async fn request_streaming(
        &self,
        req: OutboundRequest,
    ) -> Result<OutboundStreamingResponse, OutboundError> {
        let deadline = req.timeout;
        let call = async {
            let response = self.dispatch(&req).await?;

            let status = response.status().as_u16();
            let mut headers: HashMap<String, Vec<String>> = HashMap::new();
            for (name, value) in response.headers() {
                headers
                    .entry(name.as_str().to_string())
                    .or_default()
                    .push(value.to_str().unwrap_or("").to_string());
            }
            let body = response
                .into_body()
                .map_err(|e| e)
                .boxed();

            Ok(OutboundStreamingResponse { status, headers, body })
        };

        tokio::time::timeout(deadline, call)
            .await
            .map_err(|_| OutboundError::Timeout(deadline))?
    }
```

`use http_body_util::BodyExt;` is needed for `.boxed()`.

**Do this first, as its own refactor:** the request-building and dispatch code currently inline in
`request()` (from the start of its `async` block down to and including the `.await` that yields
`Response<Incoming>`, around `src/outbound/mod.rs:130-149`) moves verbatim into

```rust
    /// Builds and dispatches the outbound request, yielding the response with
    /// its body still unread. Shared by `request` and `request_streaming` so
    /// the two can never drift in connector, TLS or header handling.
    async fn dispatch(
        &self,
        req: &OutboundRequest,
    ) -> Result<http::Response<hyper::body::Incoming>, OutboundError> { /* moved code */ }
```

`request()` then calls `self.dispatch(&req).await?` and collects, unchanged. Run `cargo test
outbound::` after the extraction and before adding `request_streaming` — the refactor must be
green on its own, so a later failure is attributable to the new method and not to the move.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test test_request_streaming_returns_after_headers`
Expected: PASS, returning well under the 3s test bound.

- [ ] **Step 5: Verify the buffered path is untouched**

Run: `cargo test outbound::`
Expected: all existing outbound tests still pass.

- [ ] **Step 6: Commit (gated)**

```bash
git add src/outbound/mod.rs
git commit -m "feat(outbound): request_streaming returns after response headers"
```

---

### Task 3: `Plugin::reads_response_body` and the opt-outs

**Files:**
- Modify: `src/plugins/mod.rs` (the `Plugin` trait, ~line 73)
- Modify: `src/plugins/native/response_rewrite.rs`, `proxy_rewrite.rs`, `client.rs`, `request_id.rs`, `traffic_label.rs`, `prometheus.rs`, `opentelemetry.rs`, `zipkin.rs`, `skywalking.rs`, `logging.rs`
- Test: inline tests in `src/plugins/native/response_rewrite.rs`

**Interfaces:**
- Consumes: nothing
- Produces: `fn reads_response_body(&self) -> bool` on the `Plugin` trait, defaulting to `true`. Used by Task 4.

- [ ] **Step 1: Write the failing test**

In `src/plugins/native/response_rewrite.rs` tests — this node is the one that decides whether a typical policy streams:

```rust
    /// `response-rewrite` only reads the body when it rewrites the body:
    /// `filters` (regex substitution) or a replacement `body`. A headers-only
    /// or status-only instance leaves the body untouched, so it must not force
    /// a streaming upstream to buffer.
    #[test]
    fn test_reads_response_body_only_when_body_is_rewritten() {
        let headers_only = inline_plugin(serde_json::json!({
            "headers": { "set": { "x-frame-options": "deny" } }
        }));
        assert!(
            !headers_only.reads_response_body(),
            "headers-only rewrite must not force buffering"
        );

        let status_only = inline_plugin(serde_json::json!({ "status_code": 204 }));
        assert!(!status_only.reads_response_body());

        let with_filters = inline_plugin(serde_json::json!({
            "filters": [{ "regex": "secret", "replace": "***" }]
        }));
        assert!(
            with_filters.reads_response_body(),
            "filters rewrite the body and must force buffering"
        );

        let with_body = inline_plugin(serde_json::json!({ "body": "replaced" }));
        assert!(with_body.reads_response_body());
    }
```

Use whatever helper the file's existing tests use to build a configured instance; `inline_plugin` is the name used elsewhere in this crate — match the local one exactly.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test test_reads_response_body_only_when_body_is_rewritten`
Expected: FAIL to compile — no such method.

- [ ] **Step 3: Add the trait method**

In `src/plugins/mod.rs`, inside `pub trait Plugin`:

```rust
    /// Whether this **configured instance** reads `context.response.body`.
    ///
    /// Defaults to `true`: a plugin that does not opt out forces the policy to
    /// buffer, so adding a plugin can never silently break a stream. Opting out
    /// is a deliberate statement about a specific configuration — see the
    /// streaming-responses design doc.
    fn reads_response_body(&self) -> bool {
        true
    }
```

- [ ] **Step 4: Implement the opt-outs**

`response_rewrite.rs`:

```rust
    fn reads_response_body(&self) -> bool {
        // `filters` rewrites the body; `body`/`body_base64` replaces it. Either
        // needs the buffered body. Headers and status alone do not.
        self.filters.is_some() || self.body.is_some()
    }
```

Match the real field names in that struct. For the always-false nodes — `proxy_rewrite.rs`, `client.rs`, `request_id.rs`, `traffic_label.rs`, `prometheus.rs`, `opentelemetry.rs`, `zipkin.rs`, `skywalking.rs` — add:

```rust
    fn reads_response_body(&self) -> bool {
        false
    }
```

For `logging.rs` and its sibling loggers, opt out only when the configured `log_format` does not reference the response body:

```rust
    fn reads_response_body(&self) -> bool {
        // `$resp_body` / `{{response.body}}` in the format means the logger
        // needs the buffered body.
        self.log_format_references_body
    }
```

Compute `log_format_references_body` once at config load (search the serialized `log_format` for `resp_body` and `response.body`) and store it on the struct — do not re-scan per request.

- [ ] **Step 5: Run and watch it pass**

Run: `cargo test test_reads_response_body_only_when_body_is_rewritten`
Expected: PASS.

- [ ] **Step 6: Full suite**

Run: `cargo test`
Expected: 1244 passed, 0 failed.

- [ ] **Step 7: Commit (gated)**

```bash
git add src/plugins/
git commit -m "feat(plugins): declare whether a configured node reads the response body"
```

---

### Task 4: Compile-time inference

**Files:**
- Modify: `src/graph/engine.rs` (`CompiledGraph` ~line 21, and its compile function)
- Test: inline tests in `src/graph/engine.rs`

**Interfaces:**
- Consumes: `Plugin::reads_response_body()` (Task 3)
- Produces: on `CompiledGraph`, `stream_capable: HashSet<String>` (node ids) and `buffering_reasons: Vec<BufferingReason>` where `pub struct BufferingReason { pub upstream_node_id: String, pub blocked_by_node_id: String, pub node_type: String }`. Accessors `pub fn is_stream_capable(&self, node_id: &str) -> bool` and `pub fn buffering_reasons(&self) -> &[BufferingReason]`. Used by Tasks 5 and 7.

- [ ] **Step 1: Write the failing tests**

```rust
    /// An upstream whose success path reaches `client` through header-only
    /// nodes can stream.
    #[test]
    fn test_upstream_is_stream_capable_with_header_only_tail() {
        let graph = compile_test_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "hdr", "type": "response-rewrite",
                  "config": { "headers": { "set": { "x-a": "b" } } } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "hdr.in" },
                { "from": "hdr.success", "to": "client.in" }
            ]
        }));

        assert!(graph.is_stream_capable("up"));
        assert!(graph.buffering_reasons().is_empty());
    }

    /// A body-rewriting node on the success path forces buffering, and the
    /// compiler must name it — silence here is the failure mode this feature
    /// exists to avoid.
    #[test]
    fn test_filters_force_buffering_and_are_reported() {
        let graph = compile_test_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "rw", "type": "response-rewrite",
                  "config": { "filters": [{ "regex": "a", "replace": "b" }] } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "rw.in" },
                { "from": "rw.success", "to": "client.in" }
            ]
        }));

        assert!(!graph.is_stream_capable("up"));
        let reasons = graph.buffering_reasons();
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].upstream_node_id, "up");
        assert_eq!(reasons[0].blocked_by_node_id, "rw");
    }

    /// The error path must not influence the decision: it is taken only when
    /// the upstream produced no body at all.
    #[test]
    fn test_error_path_does_not_force_buffering() {
        let graph = compile_test_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "errs", "type": "error-handler",
                  "config": { "status_code": 502, "body_template": "{}" } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "client.in" },
                { "from": "up.error", "to": "errs.in" },
                { "from": "errs.success", "to": "client.in" }
            ]
        }));

        assert!(
            graph.is_stream_capable("up"),
            "an error-handler on the error path must not block streaming"
        );
    }
```

Write `compile_test_policy(json) -> CompiledGraph` as a test helper in this module if one does not already exist, reusing whatever the file's existing compile tests use to turn a JSON policy into a `CompiledGraph`.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test graph::engine::tests::test_upstream_is_stream_capable_with_header_only_tail graph::engine::tests::test_filters_force_buffering_and_are_reported graph::engine::tests::test_error_path_does_not_force_buffering`
Expected: FAIL to compile — `is_stream_capable` / `buffering_reasons` do not exist.

- [ ] **Step 3: Implement the inference**

Add to `CompiledGraph`:

```rust
    /// Ids of `upstream` nodes whose `success` path reaches `client` without
    /// passing any node that reads the response body.
    stream_capable: HashSet<String>,
    /// Why each non-capable upstream must buffer, for operator-visible reporting.
    buffering_reasons: Vec<BufferingReason>,
```

```rust
/// Records that one node on an upstream's success path forces buffering.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferingReason {
    pub upstream_node_id: String,
    pub blocked_by_node_id: String,
    pub node_type: String,
}
```

At the end of compilation, after `nodes` and `edges` are built:

```rust
        let mut stream_capable = HashSet::new();
        let mut buffering_reasons = Vec::new();

        for (node_id, plugin) in &nodes {
            if plugin.plugin_type() != "upstream" {
                continue;
            }
            // Walk only the success port: the error path is taken when the
            // upstream produced no body, so nodes there are irrelevant.
            let mut blocked_by: Option<(&String, &str)> = None;
            let mut seen: HashSet<&String> = HashSet::new();
            let mut queue: Vec<&String> = edges
                .get(node_id)
                .and_then(|ports| ports.get("success"))
                .into_iter()
                .collect();

            while let Some(current) = queue.pop() {
                if !seen.insert(current) {
                    continue;
                }
                let Some(p) = nodes.get(current) else { continue };
                if p.reads_response_body() {
                    blocked_by = Some((current, p.plugin_type()));
                    break;
                }
                if let Some(ports) = edges.get(current) {
                    queue.extend(ports.values());
                }
            }

            match blocked_by {
                None => {
                    stream_capable.insert(node_id.clone());
                }
                Some((blocker, node_type)) => buffering_reasons.push(BufferingReason {
                    upstream_node_id: node_id.clone(),
                    blocked_by_node_id: blocker.clone(),
                    node_type: node_type.to_string(),
                }),
            }
        }
```

Note `client` must opt out (Task 3) or every walk terminates as blocked. Add the accessors:

```rust
    pub fn is_stream_capable(&self, node_id: &str) -> bool {
        self.stream_capable.contains(node_id)
    }

    pub fn buffering_reasons(&self) -> &[BufferingReason] {
        &self.buffering_reasons
    }
```

- [ ] **Step 4: Run and watch them pass**

Run the same three test names.
Expected: PASS.

- [ ] **Step 5: Log the reasons at compile time**

Where compilation finishes, emit one line per reason:

```rust
        for reason in &buffering_reasons {
            tracing::info!(
                policy = %policy_name,
                upstream = %reason.upstream_node_id,
                blocked_by = %reason.blocked_by_node_id,
                node_type = %reason.node_type,
                "response buffering: upstream cannot stream because a downstream node reads the response body"
            );
        }
```

- [ ] **Step 6: Full suite**

Run: `cargo test`
Expected: 1247 passed, 0 failed.

- [ ] **Step 7: Commit (gated)**

```bash
git add src/graph/engine.rs
git commit -m "feat(graph): infer at compile time which upstreams can stream"
```

---

### Task 5: Upstream streaming branch, idle timeout, and guards

**Files:**
- Modify: `src/plugins/native/upstream.rs`
- Modify: `src/graph/engine.rs` (set the reserved message key before executing a stream-capable node)
- Test: inline tests in `src/plugins/native/upstream.rs`

**Interfaces:**
- Consumes: `ResponseStream` (Task 1), `request_streaming` (Task 2), `is_stream_capable` (Task 4)
- Produces: reserved context key `__may_stream` (bool); `stream_idle_timeout_ms` upstream config key (default `60000`); and in `src/outbound/idle.rs` two helpers used by Task 6 — `pub fn idle_timeout_body(body: BoxBody<Bytes, hyper::Error>, idle: Duration) -> BoxBody<Bytes, hyper::Error>` and `pub fn body_holding(body: BoxBody<Bytes, hyper::Error>, guards: Vec<Box<dyn Send + 'static>>) -> BoxBody<Bytes, hyper::Error>`.

- [ ] **Step 1: Write the failing test**

```rust
    /// With `__may_stream` set, the node must hand back a stream rather than a
    /// buffered body — and must leave `body` empty, per the invariant.
    #[tokio::test]
    async fn test_upstream_streams_when_permitted() {
        let (port, _rx) = spawn_request_line_capture().await;
        let mut ctx = ctx_with_query("/stream", vec![]);
        ctx.message
            .insert("__may_stream".to_string(), serde_json::json!(true));

        let out = plugin_at(port).execute(ctx).await.unwrap();

        assert!(out.context.response.stream.is_some(), "expected a stream");
        assert!(
            out.context.response.body.is_empty(),
            "invariant: body must be empty when stream is set"
        );
    }

    /// Without the key, behaviour is exactly as today: buffered body, no stream.
    #[tokio::test]
    async fn test_upstream_buffers_when_not_permitted() {
        let (port, _rx) = spawn_request_line_capture().await;
        let ctx = ctx_with_query("/stream", vec![]);

        let out = plugin_at(port).execute(ctx).await.unwrap();

        assert!(out.context.response.stream.is_none());
    }
```

`spawn_request_line_capture`, `ctx_with_query` and `plugin_at` already exist in this file's test module (added with the query-string fix).

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test test_upstream_streams_when_permitted`
Expected: FAIL — `stream` is `None` because the node always buffers.

- [ ] **Step 3: Implement the branch**

In `UpstreamPlugin::execute`, after the WebSocket branch and after `uri` is built:

```rust
        let may_stream = ctx
            .message
            .get("__may_stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if may_stream {
            let guard = self.balancer.owned_acquire(target_idx);
            // Identical to the buffered path's `OutboundRequest` literal
            // (`src/plugins/native/upstream.rs`, the `let outbound = OutboundRequest {`
            // block): same method, uri, headers, body, timeout, ssl_verify and tls.
            // Extract that literal into a `fn outbound_request(&self, ctx, uri, method)`
            // helper and call it from both branches so they cannot drift.
            let outbound = self.outbound_request(&ctx, uri, method);
            match self.client.request_streaming(outbound).await {
                Ok(resp) => {
                    ctx.response.status_code = resp.status;
                    ctx.response.headers = resp.headers;
                    ctx.response.body = Bytes::new();
                    let mut stream = ResponseStream::new(idle_timeout_body(
                        resp.body,
                        self.stream_idle_timeout,
                    ));
                    stream.hold(Box::new(guard));
                    ctx.response.stream = Some(stream);
                    return Ok(PluginOutput::success(ctx));
                }
                Err(e) => {
                    // Reuse the buffered path's error mapping verbatim: extract
                    // its `match &e { OutboundError::Timeout(d) => ... }` block
                    // into `fn map_outbound_error(e: OutboundError) -> (String, String)`
                    // and call it from both branches. A streaming failure here
                    // happened before any byte reached the client, so it is an
                    // ordinary error-port exit exactly like the buffered case.
                    let (code, message) = map_outbound_error(e);
                    return Err(PluginExecutionError::new(ctx, code, message));
                }
            }
        }
```

`owned_acquire` already exists on `Balancer` for precisely this — a guard that outlives the borrowing scope. Do not use `acquire`.

Add `stream_idle_timeout_ms` to `from_config` (default `60_000`) stored as `stream_idle_timeout: Duration`.

Create `src/outbound/idle.rs` with two wrappers, each with its own unit test:

```rust
/// Wraps a body so it errors when no frame arrives for `idle`. The timer
/// resets on every frame, so a steady stream survives indefinitely while a
/// silent one is reaped.
pub fn idle_timeout_body(
    body: BoxBody<Bytes, hyper::Error>,
    idle: Duration,
) -> BoxBody<Bytes, hyper::Error>;

/// Wraps a body so `guards` are dropped only when the body finishes or is
/// itself dropped. This is what keeps balancer in-flight counters and
/// `limit-conn` permits held for the stream's real lifetime instead of
/// releasing when the node returns.
pub fn body_holding(
    body: BoxBody<Bytes, hyper::Error>,
    guards: Vec<Box<dyn Send + 'static>>,
) -> BoxBody<Bytes, hyper::Error>;
```

Tests: a body yielding one frame then stalling must error after the idle bound; a body yielding a
frame every 50ms with a 500ms bound must complete; `body_holding` must not drop its guards until the
body is exhausted.

- [ ] **Step 4: Run and watch them pass**

Run: `cargo test upstream::`
Expected: all pass, including the two new tests and the existing 12.

- [ ] **Step 5: Set the key in the engine**

In `src/graph/engine.rs`, immediately before invoking a node's `execute`:

```rust
        if self.is_stream_capable(node_id) {
            ctx.message
                .insert("__may_stream".to_string(), serde_json::json!(true));
        }
```

Document `__may_stream` alongside the other reserved keys (`__policy`, `__route`, `__request_start_ms`).

- [ ] **Step 6: Full suite**

Run: `cargo test`
Expected: 1250 passed, 0 failed.

- [ ] **Step 7: Commit (gated)**

```bash
git add src/plugins/native/upstream.rs src/graph/engine.rs src/outbound/idle.rs
git commit -m "feat(upstream): stream the response when the policy permits it"
```

---

### Task 6: Transport — widen the response body type

**Files:**
- Modify: `src/server/listener.rs` (`build_response` ~line 482, the handler signature ~line 180, and the 404/error builders)
- Test: inline tests in `src/server/listener.rs`

**Interfaces:**
- Consumes: `ResponseStream::into_parts` (Task 1)
- Produces: `build_response(status, headers, trace_id, body: Bytes, stream: Option<ResponseStream>) -> Response<BoxBody<Bytes, hyper::Error>>`

- [ ] **Step 1: Write the failing test**

```rust
    /// A buffered response must keep its exact `content-length` and bytes —
    /// widening the body type must be invisible to every non-streaming route.
    #[tokio::test]
    async fn test_buffered_response_is_unchanged() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), vec!["text/plain".to_string()]);

        let resp = build_response(200, &headers, None, Bytes::from("hello"), None);

        assert_eq!(resp.status(), 200);
        let collected = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(collected, Bytes::from("hello"));
    }

    /// A streamed response carries the stream's bytes and sets no content-length.
    #[tokio::test]
    async fn test_streamed_response_carries_stream_body() {
        use http_body_util::{BodyExt, Full};
        let body = Full::new(Bytes::from("data: one\n\n"))
            .map_err(|never| match never {})
            .boxed();

        let resp = build_response(200, &HashMap::new(), None, Bytes::new(), Some(ResponseStream::new(body)));

        assert!(resp.headers().get("content-length").is_none());
        let collected = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(collected, Bytes::from("data: one\n\n"));
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test test_streamed_response_carries_stream_body`
Expected: FAIL to compile — `build_response` takes four arguments and returns `Full<Bytes>`.

- [ ] **Step 3: Widen the type**

Change the handler signature from `Result<Response<Full<Bytes>>, hyper::Error>` to `Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error>`, and `build_response` to take the extra `stream` argument. At the end:

```rust
    let body = match stream {
        Some(s) => {
            let (body, guards) = s.into_parts();
            // Guards must outlive the body; attach them to it so they drop
            // when the response finishes or the client disconnects.
            crate::outbound::idle::body_holding(body, guards)
        }
        None => Full::new(body).map_err(|never| match never {}).boxed(),
    };
    response_builder.body(body)
```

Every other `Response::builder().body(Full::new(..))` in this file (the 404 path, the 500 fallback, the WebSocket 101) needs the same `.map_err(..).boxed()` treatment. `cargo check` will list them.

- [ ] **Step 4: Run and watch them pass**

Run: `cargo test server::listener`
Expected: PASS, including the existing listener tests.

- [ ] **Step 5: Note streamed bodies in debug traces**

Spec section 8 requires a trace to record status and headers plus a note that the body streamed —
capturing the body would mean consuming it. In `src/debug/trace.rs`, where the final response is
snapshotted, record the body as a marker rather than bytes when `response.stream.is_some()`:

```rust
        let body_note = if ctx.response.stream.is_some() {
            // Never read the stream to snapshot it: the client is the only
            // legitimate consumer, and reading here would swallow the events.
            BodySnapshot::Streamed
        } else {
            BodySnapshot::Captured(ctx.response.body.clone())
        };
```

Match the existing snapshot type in that module — if it stores `{len, text}`, add a `streamed: bool`
flag instead of a new enum, whichever is the smaller change.

Write the test first, in `src/debug/` :

```rust
    /// A streamed response must be traceable without consuming the stream:
    /// headers and status are recorded, the body is flagged rather than read.
    #[test]
    fn test_trace_marks_streamed_body_without_consuming_it() {
        let mut ctx = Context::new(test_request());
        ctx.response.status_code = 200;
        ctx.response.stream = Some(ResponseStream::new(empty_boxed_body()));

        let snapshot = snapshot_response(&ctx);

        assert_eq!(snapshot.status_code, 200);
        assert!(snapshot.streamed, "streamed body must be flagged");
        assert_eq!(snapshot.body_len, 0, "a streamed body is never captured");
        assert!(ctx.response.stream.is_some(), "snapshotting must not take the stream");
    }
```

This also closes a diagnostic gap called out in spec section 1: today a hung stream produces no
trace at all, because the graph never returns. With streaming the graph returns at headers, so these
requests become visible in the debug panel for the first time.

- [ ] **Step 6: Full suite**

Run: `cargo test`
Expected: 1253 passed, 0 failed.

- [ ] **Step 7: Commit (gated)**

```bash
git add src/server/listener.rs src/outbound/idle.rs src/debug/
git commit -m "feat(server): emit streaming response bodies"
```

---

### Task 7: Surface forced buffering to operators

**Files:**
- Modify: `src/admin/policies.rs` (the validate endpoint)
- Modify: `src/mcp/tools/` (the `validate_policy` tool result)
- Test: inline tests in `src/admin/policies.rs`

**Interfaces:**
- Consumes: `CompiledGraph::buffering_reasons()` (Task 4)
- Produces: a `buffering` array on the validate response: `[{ "upstream": "up", "blocked_by": "rw", "node_type": "response-rewrite" }]`

- [ ] **Step 1: Write the failing test**

```rust
    /// Validating a policy whose upstream cannot stream must say so, naming the
    /// node responsible. An operator who wires gzip onto an SSE route learns it
    /// here rather than from "notifications stopped working".
    #[tokio::test]
    async fn test_validate_reports_forced_buffering() {
        let body = validate_policy_json(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "rw", "type": "response-rewrite",
                  "config": { "filters": [{ "regex": "a", "replace": "b" }] } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "rw.in" },
                { "from": "rw.success", "to": "client.in" }
            ]
        }))
        .await;

        assert_eq!(body["valid"], serde_json::json!(true));
        assert_eq!(body["buffering"][0]["upstream"], serde_json::json!("up"));
        assert_eq!(body["buffering"][0]["blocked_by"], serde_json::json!("rw"));
    }
```

Use the file's existing helper for driving the validate handler; match its name and signature exactly.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test test_validate_reports_forced_buffering`
Expected: FAIL — no `buffering` key in the response.

- [ ] **Step 3: Add the field**

Serialize `buffering_reasons()` into the validate response alongside `valid` and `errors`, and into the MCP `validate_policy` tool result so an agent sees it too.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test test_validate_reports_forced_buffering`
Expected: PASS.

- [ ] **Step 5: Full suite and commit (gated)**

Run: `cargo test` — expected 1254 passed.

```bash
git add src/admin/policies.rs src/mcp/
git commit -m "feat(admin): report which node forces response buffering"
```

---

### Task 8: End-to-end SSE proof, and docs

**Files:**
- Test: `src/server/listener.rs` (integration test alongside the existing WebSocket ones)
- Modify: `website/docs/reference/plugins/upstream.md`
- Modify: `website/docs/reference/roadmap.md`
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: everything above
- Produces: nothing downstream

- [ ] **Step 1: Write the failing test — the one that matters**

This asserts the property that is impossible today: an event reaches the client **while the upstream is still open**.

```rust
    /// The whole point of the feature: an SSE event must reach the client
    /// before the upstream closes the response. Today the gateway buffers, so
    /// the client sees nothing until the server hangs up — this test fails on
    /// any buffering implementation, which is exactly what makes it worth having.
    #[tokio::test]
    async fn test_sse_event_arrives_before_upstream_closes() {
        // Upstream: sends one event, waits 10s, then closes.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\nd\r\ndata: first\n\n\r\n",
                ).await;
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });

        // Gateway: listener -> upstream -> header-only response-rewrite -> client.
        let gw_port = spawn_gateway_with_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "127.0.0.1", "port": up_port }] } },
                { "id": "hdr", "type": "response-rewrite",
                  "config": { "headers": { "set": { "x-gw": "1" } } } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "hdr.in" },
                { "from": "hdr.success", "to": "client.in" }
            ]
        })).await;

        // Read the first event with a 3s bound — well inside the upstream's 10s hold.
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_first_sse_event(gw_port, "/stream"),
        )
        .await
        .expect("first event must arrive while the upstream is still open");

        assert_eq!(got, "data: first");
    }
```

Write `spawn_gateway_with_policy` and `read_first_sse_event` as helpers in this test module, modelled on the existing WebSocket integration tests in this file (they already spin up a gateway on an ephemeral port).

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test test_sse_event_arrives_before_upstream_closes`
Expected: FAIL by timing out at 3s — proof the buffering behaviour is what the test catches. If it passes before Tasks 1-6 are in, the test is not testing what it claims.

- [ ] **Step 3: Confirm it passes with the feature in place**

Run the same test with Tasks 1-6 complete.
Expected: PASS in well under 3s.

- [ ] **Step 4: Add the idle-timeout and buffered-parity integration tests**

Two more in the same module: a stream that goes silent must be terminated after `stream_idle_timeout_ms`; a stream sending an event every 200ms must survive past a deliberately short `timeout_ms`, proving the deadline no longer covers the body.

- [ ] **Step 5: Update the docs**

In `website/docs/reference/plugins/upstream.md`: document `stream_idle_timeout_ms`, the changed meaning of `timeout_ms` on a streaming response, that streaming is inferred rather than configured, that a body-reading node on the success path forces buffering and is reported, and that a mid-stream failure cannot become an error response.

In `website/docs/reference/roadmap.md`: add a row for streaming responses marked **Implemented**, with the known limitations from spec §9 as follow-ups.

In `e2e/E2E_TESTBOOK.md`: add `E2E-STREAM-01` (SSE event arrives before close) and `E2E-STREAM-02` (a policy with gzip buffers, and validate reports it).

- [ ] **Step 6: Full verification**

Run: `cargo test` — expected 1257 passed, 0 failed.
Run: `cargo fmt --check` — expected clean.
Run: `cargo clippy --all-targets` — expected no warnings.

- [ ] **Step 7: Commit (gated)**

```bash
git add src/server/listener.rs website/docs e2e/E2E_TESTBOOK.md
git commit -m "test(stream): prove SSE reaches the client before the upstream closes"
```

---

## Deferred / explicitly out of scope

Request-body streaming (uploads); `X-Accel-Buffering` support; streaming through `proxy-cache`; compressing streams; the `opentelemetry` `http.target` query-string omission noted during the audit that led here.
