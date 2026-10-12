import {useEffect, useRef, type ReactNode} from 'react';
import Layout from '@theme/Layout';
import {useColorMode} from '@docusaurus/theme-common';
import useBaseUrl from '@docusaurus/useBaseUrl';

// Generated from the Rust handlers' #[utoipa::path] annotations; kept current
// by `admin::openapi::tests::committed_openapi_spec_is_current`.
import spec from '../../../../openapi/admin-api.json';

/**
 * Scalar's standalone bundle, pinned with SRI rather than an npm dependency:
 * the reference renderer is a docs-site concern only, and this keeps it out
 * of the website lockfile (and its audit-ci gate). To upgrade, bump the
 * version and recompute the hash:
 *   curl -sL <url> | openssl dgst -sha384 -binary | openssl base64 -A
 */
const SCALAR_SRC = 'https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.72.1/dist/browser/standalone.js';
const SCALAR_SRI = 'sha384-U11tb2XnKvmwt8RlTvnwUnYgrN+ur4Xyh9htLhjajWNR/Oyl5AX5DEz00qRmlrmK';

type ScalarApp = {destroy: () => void};
type ScalarGlobal = {createApiReference: (el: Element, config: object) => ScalarApp};

declare global {
  interface Window {
    Scalar?: ScalarGlobal;
  }
}

let scalarLoading: Promise<ScalarGlobal> | undefined;

function loadScalar(): Promise<ScalarGlobal> {
  scalarLoading ??= new Promise((resolve, reject) => {
    const script = document.createElement('script');
    script.src = SCALAR_SRC;
    script.integrity = SCALAR_SRI;
    script.crossOrigin = 'anonymous';
    script.onload = () => (window.Scalar ? resolve(window.Scalar) : reject(new Error('Scalar did not load')));
    script.onerror = () => {
      scalarLoading = undefined;
      reject(new Error('Scalar failed to load'));
    };
    document.head.appendChild(script);
  });
  return scalarLoading;
}

/** Scalar reads its fonts and accent from these; reuse the site's. */
const CUSTOM_CSS = `
.scalar-app {
  --scalar-font: var(--ifm-font-family-base);
  --scalar-font-code: var(--ifm-font-family-monospace);
  --scalar-color-accent: var(--ifm-color-primary);
}
`;

function ApiReference(): ReactNode {
  const container = useRef<HTMLDivElement>(null);
  const {colorMode} = useColorMode();

  useEffect(() => {
    let app: ScalarApp | undefined;
    let cancelled = false;
    loadScalar()
      .then((scalar) => {
        if (cancelled || !container.current) return;
        app = scalar.createApiReference(container.current, {
          content: spec,
          // A static reference page: no "try it" client (the admin API
          // isn't reachable from here, and Scalar's default would route
          // requests and credentials through its hosted proxy), no hosted
          // AI agent or "Generate MCP" upsell, no telemetry, no font CDN.
          hideClientButton: true,
          hideTestRequestButton: true,
          agent: {disabled: true},
          mcp: {disabled: true},
          telemetry: false,
          withDefaultFonts: false,
          showDeveloperTools: 'never',
          hideDarkModeToggle: true,
          forceDarkModeState: colorMode,
          customCss: CUSTOM_CSS,
        });
      })
      .catch((err: Error) => {
        if (container.current) container.current.textContent = err.message;
      });
    return () => {
      cancelled = true;
      app?.destroy();
    };
  }, [colorMode]);

  return <div ref={container} />;
}

export default function AdminApiReference(): ReactNode {
  const rawSpec = useBaseUrl('/api/admin-api.json');
  return (
    <Layout
      title="Admin API reference"
      description="OpenAPI reference for the featherbit Admin API, generated from the gateway source.">
      <main>
        <ApiReference />
        <noscript>
          The interactive reference needs JavaScript. The raw OpenAPI document is at{' '}
          <a href={rawSpec}>admin-api.json</a>.
        </noscript>
      </main>
    </Layout>
  );
}
