import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import useBaseUrl from '@docusaurus/useBaseUrl';
import Layout from '@theme/Layout';
import CodeBlock from '@theme/CodeBlock';
import ThemedImage from '@theme/ThemedImage';

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

/** Plugin identity colors from the shared v2 palette (custom.css). */
const PIPELINE: {label: string; type: string; color: string}[] = [
  {label: 'listener', type: 'entry', color: 'var(--fb-plugin-listener)'},
  {label: 'proxy-rewrite', type: 'transform', color: 'var(--fb-plugin-proxy)'},
  {label: 'upstream', type: 'proxy', color: 'var(--fb-plugin-upstream)'},
  {label: 'client', type: 'exit', color: 'var(--fb-plugin-client)'},
];

type Feature = {title: string; body: string; to: string};

/** The capability the whole product is built around. */
const LEAD: Feature = {
  title: 'Node-graph routing policies',
  body: 'Each route is a directed graph of plugin nodes wired through success and error ports. Declared in YAML, edited visually, validated on save.',
  to: '/docs/concepts/policies-and-graphs',
};

const SIDE: Feature[] = [
  {
    title: '80+ native plugins',
    body: 'Proxying, transforms, auth (key, basic, JWT, HMAC, LDAP, OIDC), authz, rate limiting, traffic control, 17 loggers, tracing, serverless.',
    to: '/docs/reference/plugins',
  },
  {
    title: 'Lua scripting',
    body: 'Drop an execute(ctx) script into the pipeline. Scripts are validated at policy compile time and behave like native nodes.',
    to: '/docs/guides/lua-scripting',
  },
];

const GROUPS: {heading: string; items: Feature[]}[] = [
  {
    heading: 'Protocols',
    items: [
      {
        title: 'TLS, mTLS and SNI',
        body: 'TLS termination with hot-reloading certificates, per-hostname SNI certs, and mTLS that exposes the client identity (fingerprint, CN, SAN) to the graph.',
        to: '/docs/guides/tls',
      },
      {
        title: 'HTTP/2 and WebSocket',
        body: 'HTTP/2 negotiated per connection (ALPN over TLS, h2c on plaintext). WebSocket routes run the policy graph, then relay, including RFC 8441 over HTTP/2.',
        to: '/docs/guides/tls',
      },
      {
        title: 'L4 TCP/UDP streams',
        body: 'Proxy raw TCP and UDP to a load-balanced pool, with SNI-based routing for TLS passthrough. No termination required.',
        to: '/docs/guides/stream',
      },
    ],
  },
  {
    heading: 'Operations',
    items: [
      {
        title: 'HA clustering with etcd',
        body: 'Point the config source at etcd and replicas converge on the same routes, policies and consumers. Stateless single-binary mode stays the default.',
        to: '/docs/guides/deployment',
      },
      {
        title: 'Hot-reload and graceful shutdown',
        body: 'Config, policies and scripts apply without a restart; failed reloads keep the last good config serving. On SIGTERM, in-flight requests drain before exit.',
        to: '/docs/guides/configuration',
      },
      {
        title: 'Metrics, tracing and the web UI',
        body: 'Per-route and per-node Prometheus metrics, OpenTelemetry and Zipkin tracing, health and readiness probes, plus an embedded node-graph editor.',
        to: '/docs/guides/observability',
      },
    ],
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
          <span className={styles.heroWordmark}>featherbit</span>
        </div>
        <h1 className={styles.heroTitle}>A Rust API gateway you wire as a graph</h1>
        <p className={styles.heroTagline}>
          Routes are node graphs of 80+ plugins, wired through success and error
          ports, serving HTTP/1.1, HTTP/2, WebSocket and TCP/UDP.
        </p>
        <div className={styles.heroActions}>
          <Link className="button button--primary button--lg" to="/docs/getting-started/intro">
            Get started
          </Link>
          <Link
            className="button button--secondary button--outline button--lg"
            href="https://github.com/featherbitplatform/gateway">
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
              <span className={styles.pipelineChip} aria-hidden="true" />
              <span className={styles.pipelineNodeText}>
                <span className={styles.pipelineNodeLabel}>{node.label}</span>
                <span className={styles.pipelineNodeType}>{node.type}</span>
              </span>
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

function FeatureLink({f, className}: {f: Feature; className: string}): ReactNode {
  return (
    <Link to={f.to} className={className}>
      <h3>{f.title}</h3>
      <p>{f.body}</p>
    </Link>
  );
}

function Features(): ReactNode {
  return (
    <section className={styles.features}>
      <div className={styles.featureLead}>
        <Link to={LEAD.to} className={styles.leadCard}>
          <div className={styles.leadText}>
            <h2>{LEAD.title}</h2>
            <p>{LEAD.body}</p>
          </div>
          <ThemedImage
            className={styles.leadShot}
            alt="The policy editor: a route's node graph with success and error edges"
            sources={{
              light: useBaseUrl('/img/ui/policy-graph-light.png'),
              dark: useBaseUrl('/img/ui/policy-graph-dark.png'),
            }}
          />
        </Link>
        <div className={styles.sideStack}>
          {SIDE.map((f) => (
            <FeatureLink key={f.title} f={f} className={styles.sideCard} />
          ))}
        </div>
      </div>
      <div className={styles.groups}>
        {GROUPS.map((g) => (
          <div key={g.heading} className={styles.group}>
            <h2 className={styles.groupHeading}>{g.heading}</h2>
            <div className={styles.groupList}>
              {g.items.map((f) => (
                <FeatureLink key={f.title} f={f} className={styles.groupItem} />
              ))}
            </div>
          </div>
        ))}
      </div>
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
