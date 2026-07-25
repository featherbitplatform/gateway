/**
 * Plugin taxonomy for the Add-Node drawer — the same category grouping and
 * order the documentation sidebar uses (website/sidebars.ts). The drawer and
 * the docs are two views of one taxonomy, so keep this list in sync when a
 * plugin is added or recategorized.
 *
 * `listener` and `script` are intentionally absent: the listener is the fixed
 * graph entry point, and scripted plugins are added from concrete files in
 * their own drawer section.
 *
 * @module pluginCategories
 */

/** An ordered group of plugin types shown under one collapsible heading. */
export interface PluginCategory {
  /** Heading text, matching the docs sidebar category label. */
  label: string;
  /** Plugin type names in this category, in display order. */
  types: string[];
}

/**
 * Ordered categories mirroring the docs "Plugins" sidebar. Any plugin type
 * returned by the API that is not listed here falls into a synthesized
 * "Other" group (see {@link groupPluginsByCategory}), so the drawer never
 * silently drops a node type it does not yet know how to categorize.
 */
export const PLUGIN_CATEGORIES: PluginCategory[] = [
  {
    label: 'Structural & core proxy',
    types: [
      'client',
      'upstream',
      'proxy-rewrite',
      'response-rewrite',
      'body-transformer',
      'degraphql',
      'redirect',
      'echo',
      'gzip',
      'brotli',
      'request-id',
      'real-ip',
    ],
  },
  {
    label: 'Error handling & mocking',
    types: ['error-handler', 'error-page', 'exit-transformer', 'mocking'],
  },
  {
    label: 'Security & access control',
    types: [
      'cors',
      'csrf',
      'ip-restriction',
      'ua-restriction',
      'referer-restriction',
      'uri-blocker',
      'request-size-limit',
      'request-validation',
      'data-mask',
    ],
  },
  {
    label: 'Traffic control',
    types: [
      'rate-limit',
      'limit-count',
      'limit-conn',
      'api-breaker',
      'traffic-split',
      'proxy-mirror',
      'proxy-cache',
      'fault-injection',
      'workflow',
      'traffic-label',
    ],
  },
  {
    label: 'Authentication & consumers',
    types: [
      'key-auth',
      'basic-auth',
      'jwt-auth',
      'hmac-auth',
      'jwe-decrypt',
      'multi-auth',
      'ldap-auth',
      'consumer-restriction',
      'acl',
      'attach-consumer-label',
    ],
  },
  {
    label: 'External auth & authorization',
    types: [
      'forward-auth',
      'opa',
      'authz-casbin',
      'authz-keycloak',
      'authz-casdoor',
      'openid-connect',
      'cas-auth',
      'wolf-rbac',
      'dingtalk-auth',
      'feishu-auth',
    ],
  },
  {
    label: 'Serverless & FaaS',
    types: [
      'serverless-pre-function',
      'serverless-post-function',
      'oas-validator',
      'aws-lambda',
      'azure-functions',
      'openwhisk',
      'openfunction',
    ],
  },
  {
    label: 'Observability & logging',
    types: [
      'logging',
      'http-logger',
      'tcp-logger',
      'udp-logger',
      'syslog',
      'file-logger',
      'error-log-logger',
      'elasticsearch-logger',
      'clickhouse-logger',
      'loki-logger',
      'splunk-hec-logging',
      'datadog',
      'loggly',
      'google-cloud-logging',
      'sls-logger',
      'tencent-cloud-cls',
      'skywalking-logger',
      'lago',
    ],
  },
  {
    label: 'Tracing & metrics',
    types: ['prometheus', 'opentelemetry', 'zipkin', 'skywalking'],
  },
];

/** Label used for plugin types not listed in {@link PLUGIN_CATEGORIES}. */
export const OTHER_CATEGORY = 'Other';

/**
 * Groups plugin entries into {@link PLUGIN_CATEGORIES} order, keeping each
 * category's declared type order and dropping empty categories. Any entry
 * whose type is not in a declared category is collected into a trailing
 * "Other" group so nothing is lost.
 *
 * @typeParam T - Any object carrying a `type` field.
 * @param items - Plugin entries to group (already filtered/searched by caller).
 * @returns Non-empty `{ label, items }` groups in display order.
 */
export function groupPluginsByCategory<T extends {type: string}>(
  items: T[],
): {label: string; items: T[]}[] {
  const byType = new Map<string, T>();
  for (const item of items) byType.set(item.type, item);

  const groups: {label: string; items: T[]}[] = [];
  const claimed = new Set<string>();

  for (const category of PLUGIN_CATEGORIES) {
    const matched: T[] = [];
    for (const type of category.types) {
      const item = byType.get(type);
      if (item) {
        matched.push(item);
        claimed.add(type);
      }
    }
    if (matched.length > 0) groups.push({label: category.label, items: matched});
  }

  const leftovers = items.filter((item) => !claimed.has(item.type));
  if (leftovers.length > 0) groups.push({label: OTHER_CATEGORY, items: leftovers});

  return groups;
}
