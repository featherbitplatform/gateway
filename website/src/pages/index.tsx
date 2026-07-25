import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import useBaseUrl from '@docusaurus/useBaseUrl';
import Layout from '@theme/Layout';
import CodeBlock from '@theme/CodeBlock';

import styles from './index.module.css';

const POLICY_SNIPPET = `policies:
  - name: echo-policy
    error_handler: error-handler
    nodes:
      - id: listener
        type: listener
      - id: rewrite
        type: proxy-rewrite
        config: { phase: request, strip_path_prefix: /api }
      - id: backend
        type: upstream
        config:
          targets:
            - host: \${ECHO_BACKEND_HOST:-localhost}
              port: \${ECHO_BACKEND_PORT:-3000}
      - id: client
        type: client
    edges:
      - from: listener.out
        to: rewrite.in
      - from: rewrite.success
        to: backend.in
      - from: backend.success
        to: client.in`;

/** The gateway's real per-plugin categorical colors (ui/src/pluginMeta.tsx). */
const PIPELINE: {label: string; type: string; color: string}[] = [
  {label: 'listener', type: 'entry', color: '#8b5cf6'},
  {label: 'proxy-rewrite', type: 'transform', color: '#3b82f6'},
  {label: 'upstream', type: 'proxy', color: '#f59e0b'},
  {label: 'client', type: 'exit', color: '#10b981'},
];

const FEATURES: {title: string; body: string; to: string}[] = [
  {
    title: 'Node-graph routing policies',
    body: 'Each route is a directed graph of plugin nodes wired through success and error ports — declared in YAML, edited visually, validated on save.',
    to: '/docs/concepts/policies-and-graphs',
  },
  {
    title: '80+ native plugins',
    body: 'Proxying, transforms, auth (key/basic/JWT/HMAC/LDAP/OIDC), authz, rate limiting, traffic control, 17 loggers, tracing, serverless.',
    to: '/docs/reference/plugins',
  },
  {
    title: 'Lua scripting',
    body: 'Drop an execute(ctx) script into the pipeline. Scripts are validated at policy compile time and indistinguishable from native nodes.',
    to: '/docs/guides/lua-scripting',
  },
  {
    title: 'TLS, mTLS & SNI',
    body: 'TLS termination with hot-reloading certificates, per-hostname SNI certs, and mTLS that exposes the client identity — fingerprint, CN, SAN — to the graph.',
    to: '/docs/guides/tls',
  },
  {
    title: 'HTTP/2 & WebSocket',
    body: 'HTTP/2 negotiated per connection (ALPN over TLS, h2c on plaintext). WebSocket routes run the policy graph, then relay — including RFC 8441 over HTTP/2.',
    to: '/docs/guides/tls',
  },
  {
    title: 'L4 TCP/UDP streams',
    body: 'Proxy raw TCP and UDP to a load-balanced pool, with SNI-based routing for TLS passthrough — no termination required.',
    to: '/docs/guides/stream',
  },
  {
    title: 'HA clustering with etcd',
    body: 'Point the config source at etcd and replicas converge on the same routes, policies, and consumers. Stateless single-binary mode stays the default.',
    to: '/docs/guides/deployment',
  },
  {
    title: 'Hot-reload & graceful shutdown',
    body: 'Config, policies, and scripts take effect without a restart; failed reloads keep the last good config serving. On SIGTERM, in-flight requests drain before exit.',
    to: '/docs/guides/configuration',
  },
  {
    title: 'Metrics, tracing & the web UI',
    body: 'Per-route and per-node Prometheus metrics, OpenTelemetry/Zipkin tracing, health and readiness probes — plus an embedded node-graph editor.',
    to: '/docs/guides/observability',
  },
];

function Hero(): ReactNode {
  return (
    <header className={styles.hero}>
      <div className={styles.heroText}>
        <div className={styles.heroBrand}>
          {/* Intrinsic size is 511x853 (a tall mark). Pass the real ratio so the
              browser reserves the right box; CSS sets the rendered height and
              leaves width auto, so the mark is never squashed. */}
          <img
            src={useBaseUrl('/img/featherbit-mark.png')}
            alt=""
            className={styles.heroMark}
            width={511}
            height={853}
          />
          <h1 className={styles.heroTitle}>featherbit</h1>
        </div>
        <p className={styles.heroTagline}>
          A high-performance API gateway delivered as a single Rust binary.
          Routes are visual node graphs — 80+ plugins wired together through
          success and error ports — serving HTTP/1.1, HTTP/2, WebSocket, and
          raw TCP/UDP.
        </p>
        <div className={styles.heroActions}>
          <Link className="button button--primary button--lg" to="/docs/getting-started/intro">
            Get started
          </Link>
          <Link
            className="button button--secondary button--outline button--lg"
            href="https://github.com/francescodedomenico/featherbit">
            GitHub
          </Link>
        </div>
      </div>
      <div className={styles.heroCode}>
        <CodeBlock language="yaml" title="gateway.yaml">
          {POLICY_SNIPPET}
        </CodeBlock>
      </div>
    </header>
  );
}

function Pipeline(): ReactNode {
  return (
    <section className={styles.pipeline} aria-label="Request pipeline">
      <div className={styles.pipelineRow}>
        {PIPELINE.map((node, i) => (
          <div key={node.label} className={styles.pipelineStep}>
            <div
              className={styles.pipelineNode}
              style={{'--node-color': node.color} as React.CSSProperties}>
              <span className={styles.pipelineNodeType}>{node.type}</span>
              <span className={styles.pipelineNodeLabel}>{node.label}</span>
            </div>
            {i < PIPELINE.length - 1 && (
              <span className={styles.pipelineEdge} aria-hidden="true" />
            )}
          </div>
        ))}
      </div>
      <p className={styles.pipelineCaption}>
        A request flows listener → plugins → client. Every node also has an
        error port, so failures route to handlers instead of raw 500s.
      </p>
    </section>
  );
}

function Features(): ReactNode {
  return (
    <section className={styles.features}>
      {FEATURES.map((f) => (
        <Link key={f.title} to={f.to} className={styles.featureCard}>
          <h3>{f.title}</h3>
          <p>{f.body}</p>
        </Link>
      ))}
    </section>
  );
}

export default function Home(): ReactNode {
  return (
    <Layout description="A high-performance API gateway delivered as a single Rust binary. Routes are visual node graphs, serving HTTP/1.1, HTTP/2, WebSocket, and raw TCP/UDP.">
      <main className={styles.main}>
        <Hero />
        <Pipeline />
        <Features />
      </main>
    </Layout>
  );
}
