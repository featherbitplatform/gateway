# Stores & Sessions UI + E2E + CI Implementation Plan (Plan 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the admin UI a Stores editor (CRUD + ping), a Sessions panel (list/revoke), and store/session pickers in plugin config forms; add e2e coverage for the redis-backed session flow; add the CI redis/valkey live-test matrix; publish the stores concept page.

**Architecture:** The UI follows its established single-screen pattern: a fourth mutually-exclusive sidebar selection (`selectedStore` → `StoresPanel`), a footer-button Dialog for Sessions (Debug-panel twin, including the 501-headless DisabledNotice treatment), and a new declarative `optionsFrom: 'stores'` mechanism on `SchemaForm` so `store` pickers stay schema-driven. e2e gains its first env-gated scenarios (`FEATHERBIT_TEST_REDIS_URL` — the same knob the Rust live tests use), running the real mock-idp interactive login with `session_storage: redis` against a redis service container in CI; no Keycloak anywhere. One small Rust task first closes a real gap: the store delete-guard doesn't scan the flat `session_store` key the UI writes.

**Tech Stack:** React 19 + TS (vite, vitest node-env pure-logic tests only, eslint react-refresh/hooks strict), Playwright, GitHub Actions service containers, Docusaurus.

**Spec:** `docs/superpowers/specs/2026-08-21-session-storage-design.md` §4 (UI: Stores editor, store pickers, Sessions panel) + §5 Testing (e2e scenarios, CI redis:7/valkey:8 matrix) as amended. Plans 1-2 shipped everything this consumes; their Plan-3 hand-off notes are quoted in this plan's task contexts.

## Global Constraints

- **Branch:** create `feature/stores-sessions-ui` off `feature/server-side-sessions` (PRs #28/#29 open; this stacks — Task 1 Step 0).
- Conventional Commits, **no Co-Authored-By**. `git add` explicit paths only (dirty unrelated files exist).
- Rust gates (Tasks 1, 10 verification): `cargo fmt`; `cargo clippy --all-targets -- -D warnings`; `cargo clippy --no-default-features --all-targets -- -D warnings`; `cargo check --no-default-features`; full `cargo test`.
- UI gates (Tasks 2-7): from `ui/`: `npm run lint` (react-refresh: component files export ONLY components; react-hooks exhaustive-deps is an error), `npm test` (vitest, node env, pure logic only — NO jsdom/testing-library exist and none may be added), `npm run build` (tsc strict: `verbatimModuleSyntax` — type-only imports MUST use `import type`; `noUnusedLocals/Parameters`; `noFallthroughCasesInSwitch`).
- UI patterns that bind every UI task: mutations are `await api.X(); await loadData(); setToast({tone:'success',...})` in try / `setToast({tone:'error', title, message: `${e}`})` in catch — never optimistic; panels are dumb (state seeded from props, remounted via `key=`); every interactive control gets an `aria-label` (e2e's only stable hooks); inline styles with `var(--token)` — no new CSS classes; new modules carry a `@module` TSDoc header (typedoc).
- e2e: scenarios registered in `e2e/E2E_TESTBOOK.md` (`| ID | Scenario | Expected |` rows; `**Browser.**` prefix for browser-driven); new area codes `E2E-STORE` and `E2E-SESS` (both free); specs idempotent (delete-if-present at top, teardown at bottom); env-gated tests use `test.skip(!process.env.FEATHERBIT_TEST_REDIS_URL, 'FEATHERBIT_TEST_REDIS_URL not set')` — the suite's FIRST env gate, mirroring the Rust convention.
- The e2e fixture gateway must keep booting with NO redis running: store clients connect lazily (`RedisStoreClient::build` does no I/O), and the fixture store URL uses `${FEATHERBIT_TEST_REDIS_URL:-redis://127.0.0.1:6379}`. NEVER pass an empty-string env var to the gateway (empty ≠ unset for `${VAR:-default}`) — use a conditional spread in playwright.config.ts.
- Server contracts (from `src/admin/stores.rs` / `sessions.rs`, verified): stores list is a BARE array; store delete referenced → `409 {"error":"in_use","referrers":[...]}`; ping → `{status:"ok",latency_ms,version}` | 400/502/504 | **501 headless**; sessions: `GET /api/sessions?store=…` → `{sessions:[…],next_cursor}` (400 missing store / 404 unknown / 502 / **501**); `DELETE /api/sessions/{store}/{id}`; `DELETE /api/sessions?store=&subject=` → `{revoked:n}`. The UI `request<T>` throws `Error("<status>: <body>")` — structured handling parses that string.
- Config keys the pickers write (from `server_session::parse_backend`): flat `session_storage` (`cookie`|`redis`) and `session_store` (store name); empty string = unset (safe). Nested `session.storage` WINS over flat — the pickers' hints must say so.

---

### Task 1: (Rust) referrer guard scans the flat `session_store` key

**Files:**
- Modify: `src/admin/stores.rs` (`config_references` + one test)

**Interfaces:**
- Consumes: existing `config_references(config, name) -> bool` (checks flat `store`, nested `session.store`, workflow `rules[].actions[][1].store`).
- Produces: it additionally matches flat `session_store` — the exact key the UI schema (Task 4) writes. Referrer strings unchanged.

- [ ] **Step 0: Create the branch**

```bash
git checkout -b feature/stores-sessions-ui feature/server-side-sessions
```

- [ ] **Step 1: Write the failing test**

Next to `test_delete_referenced_store_is_409_with_referrers` in `src/admin/stores.rs`'s test module:

```rust
    /// The UI's SchemaForm writes the FLAT `session_store` key (the nested
    /// `session.store` form is hand-written YAML); the delete guard must see
    /// both, or a store referenced only by a UI-authored session plugin
    /// config could be deleted out from under it.
    #[tokio::test]
    async fn test_delete_store_referenced_by_flat_session_store_is_409() {
        let state = test_state(
            r#"
stores:
  - name: s1
    type: redis
    url: redis://127.0.0.1:6379
plugin_configs:
  - name: ui-oidc
    type: openid-connect
    config: { session_storage: redis, session_store: s1 }
"#,
        );
        let (status, body) = send(
            &state,
            Request::delete("/api/stores/s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "in_use");
        let refs: Vec<String> = body["referrers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(refs.contains(&"plugin_config 'ui-oidc'".to_string()), "{refs:?}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test test_delete_store_referenced_by_flat_session_store_is_409`
Expected: FAIL — delete succeeds (200) because only flat `store`/nested `session.store` are scanned.

- [ ] **Step 3: Implement**

In `config_references`, add one disjunct alongside the flat `store` check:

```rust
        config.get("store").and_then(|v| v.as_str()) == Some(name)
            || config.get("session_store").and_then(|v| v.as_str()) == Some(name)
```

(keep the nested `session.store` and workflow-rules checks unchanged).

- [ ] **Step 4: Run + gates + commit**

Run: the new test, then `cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo check --no-default-features && cargo clippy --no-default-features --all-targets -- -D warnings`

```bash
git add src/admin/stores.rs
git commit -m "fix(admin): delete guard recognizes the flat session_store key"
```

---

### Task 2: (UI) types, API client, pure helpers + unit tests

**Files:**
- Modify: `ui/src/types/index.ts` (three interfaces after `PluginConfigDef` ~:126)
- Modify: `ui/src/api/client.ts` (two endpoint groups)
- Create: `ui/src/apiError.ts`
- Modify: `ui/src/format.ts` (add `formatUnixTime`)
- Test: `ui/src/apiError.test.ts`, `ui/src/format.test.ts`

**Interfaces:**
- Produces (every later UI task consumes verbatim):
  - `interface StoreConfig { name: string; type: string; description?: string; url: string; password?: string; key_prefix: string; topology?: string; urls?: string[]; connect_timeout_ms: number; tls?: StoreTlsConfig }`, `interface StoreTlsConfig { ca_cert_path?: string }`, `interface SessionMeta { id: string; subject: string; plugin: string; policy: string; route: string; created_at: number; expires_at: number }`, `interface SessionPage { sessions: SessionMeta[]; next_cursor: string | null }`, `interface StorePing { status: string; latency_ms: number; version: string }`
  - `api.listStores(): Promise<StoreConfig[]>`; `api.createStore(store: StoreConfig)`; `api.updateStore(name: string, store: StoreConfig)`; `api.deleteStore(name: string)`; `api.pingStore(name: string): Promise<StorePing>`; `api.listSessions(filter: { store: string; subject?: string; plugin?: string; limit?: number; cursor?: string }): Promise<SessionPage>`; `api.deleteSession(store: string, id: string)`; `api.deleteSessionsBySubject(store: string, subject: string): Promise<{ revoked: number }>`
  - `parseApiError(e: unknown): { status: number | null; error: string; referrers: string[]; raw: string }` (in `apiError.ts`)
  - `formatUnixTime(secs: number): string` (in `format.ts`)

- [ ] **Step 1: Write the failing unit tests**

Create `ui/src/apiError.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { parseApiError } from './apiError';

describe('parseApiError', () => {
  it('extracts status, error and referrers from a 409 in_use body', () => {
    const e = new Error(
      '409: {"error":"in_use","referrers":["plugin_config \'shared-lc\'","policy \'p\' node \'n\'"]}',
    );
    expect(parseApiError(e)).toEqual({
      status: 409,
      error: 'in_use',
      referrers: ["plugin_config 'shared-lc'", "policy 'p' node 'n'"],
      raw: e.message,
    });
  });

  it('extracts status and error from a plain error envelope', () => {
    const e = new Error('501: {"error":"this binary was built without the redis-store feature"}');
    const parsed = parseApiError(e);
    expect(parsed.status).toBe(501);
    expect(parsed.error).toBe('this binary was built without the redis-store feature');
    expect(parsed.referrers).toEqual([]);
  });

  it('degrades gracefully on non-JSON bodies and non-Error values', () => {
    expect(parseApiError(new Error('502: upstream said no')).status).toBe(502);
    expect(parseApiError(new Error('502: upstream said no')).error).toBe('upstream said no');
    expect(parseApiError('boom')).toEqual({ status: null, error: 'boom', referrers: [], raw: 'boom' });
  });
});
```

Create `ui/src/format.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { formatUnixTime } from './format';

describe('formatUnixTime', () => {
  it('renders a unix-seconds timestamp as a locale date-time string', () => {
    // Fixed instant; assert on the parts that are locale-stable.
    const s = formatUnixTime(1_755_820_800); // 2026-08-22T00:00:00Z
    expect(s).toMatch(/2026/);
  });
  it('renders 0 as a dash (unset)', () => {
    expect(formatUnixTime(0)).toBe('—');
  });
});
```

Run: `cd ui && npm test` — Expected: FAIL (modules/functions missing).

- [ ] **Step 2: Implement the helpers**

Create `ui/src/apiError.ts`:

```ts
/**
 * Structured view over the Admin API client's stringly errors.
 *
 * `request<T>` throws `Error("<status>: <body>")`; this parses that shape so
 * panels can special-case 409 in_use (referrer lists), 501 (headless build),
 * and 502 (store outage) instead of dumping raw JSON into a toast.
 *
 * @module
 */

export interface ParsedApiError {
  /** HTTP status, or null when the message doesn't carry one. */
  status: number | null;
  /** The body's `error` field when it is JSON, else the raw body text. */
  error: string;
  /** The body's `referrers` list (409 in_use), else empty. */
  referrers: string[];
  /** The full original message, for fallback display. */
  raw: string;
}

/** Parses an unknown thrown value into {@link ParsedApiError}. */
export function parseApiError(e: unknown): ParsedApiError {
  const raw = e instanceof Error ? e.message : String(e);
  const m = raw.match(/^(\d{3}): ([\s\S]*)$/);
  if (!m) return { status: null, error: raw, referrers: [], raw };
  const status = Number(m[1]);
  const body = m[2];
  try {
    const json = JSON.parse(body) as { error?: unknown; referrers?: unknown };
    return {
      status,
      error: typeof json.error === 'string' ? json.error : body,
      referrers: Array.isArray(json.referrers)
        ? json.referrers.filter((r): r is string => typeof r === 'string')
        : [],
      raw,
    };
  } catch {
    return { status, error: body, referrers: [], raw };
  }
}
```

In `ui/src/format.ts`, append:

```ts
/** Renders a unix-seconds timestamp as a short locale date-time; 0 = "—". */
export function formatUnixTime(secs: number): string {
  if (!secs) return '—';
  return new Date(secs * 1000).toLocaleString();
}
```

- [ ] **Step 3: Types + client methods**

`ui/src/types/index.ts` — after `PluginConfigDef`, house style (TSDoc naming the Rust source on the interface AND every field):

```ts
/**
 * A named shared store: a redis/valkey connection referenced by plugin
 * config (`store` / `session_store`).
 *
 * @remarks
 * Mirrors src/config/gateway.rs::StoreConfig (YAML key `type` ↔ `store_type`);
 * served raw — `${ENV}` placeholders are never resolved by the Admin API.
 */
export interface StoreConfig {
  /** Unique name, referenced by plugin config. */
  name: string;
  /** Backend type: `redis` or `valkey` (aliases for the same RESP backend). */
  type: string;
  /** Optional human-readable description shown in the sidebar. */
  description?: string;
  /** Connection URL (`redis://` or `rediss://`); may hold `${ENV}` placeholders. */
  url: string;
  /** Optional password; overrides any password embedded in the URL. */
  password?: string;
  /** Namespace prefix for every key this store writes (server default `fb`). */
  key_prefix: string;
  /** Reserved for HA topologies; v1 accepts only `standalone`. */
  topology?: string;
  /** Reserved for HA topologies; rejected in v1. */
  urls?: string[];
  /** Connect/response timeout in milliseconds (server default 2000). */
  connect_timeout_ms: number;
  /** Optional TLS options for `rediss://`. */
  tls?: StoreTlsConfig;
}

/**
 * TLS options for a `rediss://` store.
 *
 * @remarks Mirrors src/config/gateway.rs::StoreTlsConfig.
 */
export interface StoreTlsConfig {
  /** PEM CA bundle path for a private CA. */
  ca_cert_path?: string;
}

/**
 * Metadata envelope for one server-side session (payloads never leave the store).
 *
 * @remarks Mirrors src/sessions/mod.rs::SessionMeta; served by src/admin/sessions.rs.
 */
export interface SessionMeta {
  /** The session id (the value in the browser's cookie). */
  id: string;
  /** Authenticated subject; may be empty (opaque token with no claims). */
  subject: string;
  /** Plugin type that established the session. */
  plugin: string;
  /** Policy name the session was established under. */
  policy: string;
  /** Route name the session was established under. */
  route: string;
  /** Unix seconds. */
  created_at: number;
  /** Unix seconds. */
  expires_at: number;
}

/**
 * One page of session metadata.
 *
 * @remarks Mirrors src/sessions/mod.rs::SessionPage (cursor = Redis SCAN cursor).
 */
export interface SessionPage {
  /** The page's sessions. */
  sessions: SessionMeta[];
  /** Opaque cursor for the next page; null when exhausted. */
  next_cursor: string | null;
}

/**
 * Result of a store connectivity check.
 *
 * @remarks Inline JSON from src/admin/stores.rs::ping_store.
 */
export interface StorePing {
  /** Always `ok` on success. */
  status: string;
  /** Round-trip latency in milliseconds. */
  latency_ms: number;
  /** Server version (redis_version / valkey_version). */
  version: string;
}
```

`ui/src/api/client.ts` — two groups, mirroring the plugin-configs block's style (import the new types with the existing `import type { ... }` list):

```ts
  // Stores
  /** `GET /api/stores` — returns all named stores (raw `${ENV}` placeholders, never resolved). */
  listStores: () => request<StoreConfig[]>('/api/stores'),
  /** `POST /api/stores` — creates a store; 409 when the name exists. */
  createStore: (store: StoreConfig) =>
    request('/api/stores', { method: 'POST', body: JSON.stringify(store) }),
  /** `PUT /api/stores/{name}` — upserts the named store. */
  updateStore: (name: string, store: StoreConfig) =>
    request(`/api/stores/${name}`, { method: 'PUT', body: JSON.stringify(store) }),
  /** `DELETE /api/stores/{name}` — removes the store; 409 `in_use` with referrers while referenced. */
  deleteStore: (name: string) => request(`/api/stores/${name}`, { method: 'DELETE' }),
  /** `POST /api/stores/{name}/ping` — connectivity check; 502/504 unreachable, 501 headless build. */
  pingStore: (name: string) =>
    request<StorePing>(`/api/stores/${name}/ping`, { method: 'POST' }),

  // Sessions
  /** `GET /api/sessions?store=…` — one page of session metadata; 501 on headless builds. */
  listSessions: (filter: {
    store: string;
    subject?: string;
    plugin?: string;
    limit?: number;
    cursor?: string;
  }) => {
    const q = new URLSearchParams();
    q.set('store', filter.store);
    if (filter.subject) q.set('subject', filter.subject);
    if (filter.plugin) q.set('plugin', filter.plugin);
    if (filter.limit) q.set('limit', String(filter.limit));
    if (filter.cursor) q.set('cursor', filter.cursor);
    return request<SessionPage>(`/api/sessions?${q.toString()}`);
  },
  /** `DELETE /api/sessions/{store}/{id}` — revokes one session. */
  deleteSession: (store: string, id: string) =>
    request(`/api/sessions/${store}/${id}`, { method: 'DELETE' }),
  /** `DELETE /api/sessions?store=…&subject=…` — revokes every session for a subject. */
  deleteSessionsBySubject: (store: string, subject: string) => {
    const q = new URLSearchParams({ store, subject });
    return request<{ revoked: number }>(`/api/sessions?${q.toString()}`, { method: 'DELETE' });
  },
```

- [ ] **Step 4: Run + commit**

Run: `cd ui && npm test && npm run lint && npm run build`
Expected: 5 new unit tests pass; lint/build clean.

```bash
git add ui/src/types/index.ts ui/src/api/client.ts ui/src/apiError.ts ui/src/apiError.test.ts ui/src/format.ts ui/src/format.test.ts
git commit -m "feat(ui): stores/sessions API client, types, and error parsing"
```

---

### Task 3: (UI) `optionsFrom` dynamic options in SchemaForm

**Files:**
- Modify: `ui/src/pluginConfig.ts` (`FieldSchema` gains `optionsFrom`)
- Modify: `ui/src/components/SchemaForm.tsx` (prop + select-case + object-recursion forwarding + TSDoc list)
- Modify: `ui/src/components/NodeInspector.tsx` (~:500-506, thread the prop)
- Modify: `ui/src/components/PluginConfigPanel.tsx` (~:124, thread the prop)
- Modify: `ui/src/App.tsx` (pass `stores` to both; `stores` joins `loadData`'s `Promise.all`)

**Interfaces:**
- Consumes: `api.listStores`, `StoreConfig` (Task 2).
- Produces: `FieldSchema.optionsFrom?: 'stores'`; `SchemaFormProps.dynamicOptions?: Record<string, FieldOption[]>`; App state `const [stores, setStores] = useState<StoreConfig[]>([])` + `storeOptions: FieldOption[]` memo (`{value: s.name, label: `${s.name} (${s.type})`}`); `NodeInspector` and `PluginConfigPanel` each gain a `stores: StoreConfig[]` prop and pass `dynamicOptions={{ stores: storeOptions }}` down.

- [ ] **Step 1: Schema + form mechanism**

`ui/src/pluginConfig.ts` — in `FieldSchema` (with TSDoc, after `options`):

```ts
  /**
   * Source of dynamic options for `select` fields, resolved at render time
   * from SchemaForm's `dynamicOptions` prop (e.g. `'stores'` = the declared
   * `stores:` names). Merged AFTER `options`, so a static empty-choice entry
   * can precede the dynamic list.
   */
  optionsFrom?: 'stores';
```

`ui/src/components/SchemaForm.tsx`:
1. `SchemaFormProps` gains `/** Dynamic option lists for `optionsFrom` selects (key = source name). */ dynamicOptions?: Record<string, FieldOption[]>;` and the component destructures it.
2. The `select` case becomes:

```tsx
      case 'select': {
        const opts = [
          ...normalizeOptions(field.options),
          ...(field.optionsFrom ? (dynamicOptions?.[field.optionsFrom] ?? []) : []),
        ];
        return (
          <select
            value={(current as string) ?? (field.default as string) ?? ''}
            onChange={(e) => set(field.key, e.target.value)}
            style={{ ...inputStyle, appearance: 'auto' }}
            aria-label={field.label}
          >
            {opts.map((opt) => (
              <option key={opt.value} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </select>
        );
      }
```

3. The recursive `'object'` case (~:600-605) forwards `dynamicOptions={dynamicOptions}` alongside `varContext` (missing this breaks nested forms — the report calls it out explicitly).
4. Extend the per-field TSDoc list (~:270-311) with one line for `optionsFrom`.

- [ ] **Step 2: Thread from App**

`App.tsx`: add `stores` to the required `Promise.all` in `loadData` (`api.listStores()` → `setStores`), a `const storeOptions = useMemo(() => stores.map((s) => ({ value: s.name, label: `${s.name} (${s.type})` })), [stores]);`, and pass `stores`/`dynamicOptions` per the Interfaces block. `NodeInspector.tsx` and `PluginConfigPanel.tsx` accept and forward (`<SchemaForm ... dynamicOptions={{ stores: storeOptions }} />` — PluginConfigPanel receives `storeOptions: FieldOption[]` as a prop since it has no other store knowledge; NodeInspector likewise). Import types with `import type`.

- [ ] **Step 3: Run + commit**

Run: `cd ui && npm test && npm run lint && npm run build`
Expected: clean (behavior lands with Task 4's schema entries; this task is mechanism only — existing forms unchanged since no schema uses `optionsFrom` yet).

```bash
git add ui/src/pluginConfig.ts ui/src/components/SchemaForm.tsx ui/src/components/NodeInspector.tsx ui/src/components/PluginConfigPanel.tsx ui/src/App.tsx
git commit -m "feat(ui): dynamic select options (optionsFrom) threaded through SchemaForm"
```

---

### Task 4: (UI) schema entries — redis policy, session storage pickers, dingtalk/feishu session keys

**Files:**
- Modify: `ui/src/pluginConfig.ts` only

**Interfaces:**
- Consumes: `optionsFrom: 'stores'` (Task 3).
- Produces: the exact flat keys the Rust side reads (`server_session::parse_backend`): `session_storage`, `session_store`; limit-count's `policy`/`store`; openid-connect's `session_refresh`.

- [ ] **Step 1: limit-count** (entry at ~:465-475): change the policy line and add `store` after it:

```ts
    { key: 'policy', label: 'Policy', type: 'select', options: ['local', 'redis'], default: 'local', hint: 'redis = cluster-shared counters via a named store' },
    { key: 'store', label: 'Store', type: 'select', options: [{ value: '', label: '(none)' }], optionsFrom: 'stores', hint: 'required when policy is redis: a declared stores: entry' },
```

- [ ] **Step 2: the three SSO plugins** — in `openid-connect`, `cas-auth`, `authz-casdoor`, insert directly after each `session_secret` line:

```ts
    { key: 'session_storage', label: 'Session storage', type: 'select', options: [{ value: 'cookie', label: 'cookie (client-side, default)' }, { value: 'redis', label: 'redis (server-side, revocable)' }], default: 'cookie', hint: 'nested session.storage in YAML overrides this flat key' },
    { key: 'session_store', label: 'Session store', type: 'select', options: [{ value: '', label: '(none)' }], optionsFrom: 'stores', hint: 'required when storage is redis: a declared stores: entry' },
```

openid-connect additionally gains, after `session_cookie_lifetime`:

```ts
    { key: 'session_refresh', label: 'Token refresh', type: 'switch', switchLabel: 'Lock-coordinated refresh (redis storage only)', default: true },
```

- [ ] **Step 3: dingtalk-auth / feishu-auth** — append the full session block to both entries (defaults per plugin: cookie name `dingtalk_session` / `feishu_session`):

```ts
    { key: 'session_secret', label: 'Session secret', type: 'text', placeholder: '${SESSION_SECRET}', hint: 'set to enable session mode (302 login + session cookie); blank = validate the code on every request', template: 'env-only' },
    { key: 'redirect_uri', label: 'Redirect URI', type: 'text', placeholder: 'https://login.example.com/start', hint: 'session mode: 302 target when no code and no session are present' },
    { key: 'session_storage', label: 'Session storage', type: 'select', options: [{ value: 'cookie', label: 'cookie (client-side, default)' }, { value: 'redis', label: 'redis (server-side, revocable)' }], default: 'cookie', hint: 'nested session.storage in YAML overrides this flat key' },
    { key: 'session_store', label: 'Session store', type: 'select', options: [{ value: '', label: '(none)' }], optionsFrom: 'stores', hint: 'required when storage is redis' },
    { key: 'session_cookie_name', label: 'Session cookie name', type: 'text', default: 'dingtalk_session', hint: 'session mode only' },
    { key: 'session_cookie_path', label: 'Session cookie path', type: 'text', default: '/', hint: 'session mode only' },
    { key: 'session_cookie_lifetime', label: 'Session lifetime (s)', type: 'number', default: 86400, hint: 'session mode only' },
```

(feishu: `default: 'feishu_session'` on the cookie-name line. Verify the flat cookie keys against the plugins' `session_cookie_str` readers — they are the same `session_cookie_<field>` fallbacks as the other three plugins.)

- [ ] **Step 4: Run + commit**

Run: `cd ui && npm test && npm run lint && npm run build`

```bash
git add ui/src/pluginConfig.ts
git commit -m "feat(ui): store and session-storage pickers across the six store-aware plugin schemas"
```

---

### Task 5: (UI) Stores sidebar section + App wiring + dialogs

**Files:**
- Modify: `ui/src/App.tsx` (selection state, handlers, create/delete dialogs)
- Modify: `ui/src/components/Sidebar.tsx` (Stores section between Plugin Configs and the footer)

**Interfaces:**
- Consumes: `api.{listStores,createStore,deleteStore}`, `parseApiError`, `StoreConfig` (Task 2); `stores` state (Task 3).
- Produces: `selectedStore: string | null` as the FOURTH mutually-exclusive selection (all four handlers clear the other three); `Sidebar` props `stores, selectedStore, onSelectStore, onCreateStore, onDeleteStore`; App handlers `handleSelectStore`, `handleCreateStore` (dialog: name + type select redis/valkey + url), `submitCreateStore`, `submitDeleteStore` — all following the exact `await api.X(); await loadData(); setToast(...)` triad. Delete special-case: on 409 `in_use`, the error toast message is `Referenced by: ${parsed.referrers.join(', ')}`.

- [ ] **Step 1: App state + handlers**

Clone the plugin-configs triad (App.tsx:308-345) verbatim-adapted: dialog state `createStoreOpen/newStoreName/newStoreType/newStoreUrl`, `deleteStoreTarget`. `submitCreateStore` calls `api.createStore({ name, type, url, key_prefix: 'fb', connect_timeout_ms: 2000 })`; `submitDeleteStore`'s catch:

```tsx
    } catch (e) {
      const parsed = parseApiError(e);
      setToast({
        tone: 'error',
        title: 'Failed to delete store',
        message:
          parsed.error === 'in_use'
            ? `Referenced by: ${parsed.referrers.join(', ')}`
            : `${e}`,
      });
    }
```

All four `handleSelect*` handlers now clear THREE siblings (update the comment at App.tsx:169-172 accordingly). Main panel ternary gains a `selectedStoreDef ? <StoresPanel .../> :` arm (component lands in Task 6 — in THIS task render a placeholder `null` arm is NOT acceptable; instead do Tasks 5+6 as one commit if the intermediate state won't compile cleanly, or gate the ternary arm behind the Task 6 import. Simplest: implement Task 5 and 6 in sequence but commit them together if needed — the commit split below assumes StoresPanel exists as a stub first; see Step 2).

- [ ] **Step 2: Stub panel to keep the tree green**

Create `ui/src/components/StoresPanel.tsx` as a minimal compiling component (name + "coming next task" body is NOT allowed by the no-placeholder rule — instead Task 6's full panel is REQUIRED before committing Task 5's App wiring. **Execution note: Tasks 5 and 6 are one review unit; implement both, commit as two commits in order (Sidebar/App first only if it compiles standalone — it will not, so use ONE commit for both tasks labeled per Task 6's message and note it in the report).** The controller should dispatch Tasks 5+6 to a single implementer.

- [ ] **Step 3: Sidebar section**

Clone the Plugin Configs block (Sidebar.tsx:309-389) as a Stores section: eyebrow `Stores`, `aria-label="New store"` on the button, row primary text `s.name`, secondary `s.description || `${s.type} · ${s.url}``, delete button `aria-label={`Delete store ${s.name}`}`. Create dialog (in App) mirrors the plugin-config one: `DialogField` name (`placeholder="sessions-redis"`), type `<select>` with `redis`/`valkey`, `DialogField` url (`placeholder="redis://127.0.0.1:6379"`, `mono`). Delete dialog prose: "Delete store <code>{name}</code>? Deletion is blocked while any plugin config references it."

- [ ] **Step 4: Run + commit** (jointly with Task 6 — see execution note)

---

### Task 6: (UI) StoresPanel with ping

**Files:**
- Create: `ui/src/components/StoresPanel.tsx`

**Interfaces:**
- Consumes: `StoreConfig`, `StorePing`, `api.{updateStore,pingStore}`, `parseApiError` (Task 2).
- Produces: `<StoresPanel key={def.name} def={StoreConfig} onSave={(s: StoreConfig) => Promise<void>} onError={(title, message) => void} />` — dumb panel, remounted by key, exports ONLY the component (react-refresh).

- [ ] **Step 1: Implement the panel**

Follow `PluginConfigPanel.tsx`'s shell exactly (max-width 560 centered card, label/input styles from that file). Local state seeded from `def`: `type`, `description`, `url`, `password`, `keyPrefix`, `connectTimeoutMs` — plain controlled inputs (labels: Type [select redis/valkey], Description, URL [mono, hint: "${ENV} placeholders stay raw — resolved only when the client is built"], Password [mono, hint: "optional; ${ENV} recommended"], Key prefix, Connect timeout (ms) [number]). `topology`/`urls`/`tls` are NOT editable in v1 — render nothing for them, but PRESERVE them on save (`onSave({ ...def, type, description: description || undefined, url, password: password || undefined, key_prefix: keyPrefix, connect_timeout_ms: connectTimeoutMs })` keeps `def.tls`/`def.topology`/`def.urls` via the spread).

Ping block above the save button:

```tsx
  const [ping, setPing] = useState<{ state: 'idle' | 'busy' } | { state: 'ok'; result: StorePing } | { state: 'fail'; message: string }>({ state: 'idle' });

  const handlePing = async () => {
    setPing({ state: 'busy' });
    try {
      setPing({ state: 'ok', result: await api.pingStore(def.name) });
    } catch (e) {
      const parsed = parseApiError(e);
      setPing({
        state: 'fail',
        message:
          parsed.status === 501
            ? 'This gateway build has no redis-store support.'
            : parsed.status === 504
              ? `Timed out: ${parsed.error}`
              : parsed.error,
      });
    }
  };
```

Rendered as a bordered row: a `DialogButton`-style button `aria-label="Ping store"` ("Ping"), then inline result — ok: `✓ {latency_ms} ms · v{version}` in `var(--success, var(--accent))`; fail: the message in `var(--error)`. Ping tests the SAVED config (server reads `gw.stores`), so the row carries the hint "pings the last saved configuration". Save button label `Save Store` calling `onSave`.

- [ ] **Step 2: Run + commit (Tasks 5+6 together)**

Run: `cd ui && npm test && npm run lint && npm run build`

```bash
git add ui/src/App.tsx ui/src/components/Sidebar.tsx ui/src/components/StoresPanel.tsx
git commit -m "feat(ui): stores editor — sidebar section, panel with ping, create/delete dialogs"
```

---

### Task 7: (UI) Sessions panel

**Files:**
- Create: `ui/src/components/SessionsPanel.tsx`
- Modify: `ui/src/App.tsx` (open state + mount), `ui/src/components/Sidebar.tsx` (footer button beside Debug)

**Interfaces:**
- Consumes: `api.{listSessions,deleteSession,deleteSessionsBySubject}`, `SessionMeta`/`SessionPage`, `parseApiError`, `formatUnixTime`, `stores` (App state), the shared `Dialog`/`DialogButton`.
- Produces: `<SessionsPanel open onClose stores={StoreConfig[]} onError={(title, message) => void} />`; Sidebar props `onOpenSessions: () => void` + footer button `aria-label="Sessions"` (icon `KeyRound` from lucide-react), always rendered, dimmed with `title="Server-side sessions — requires a declared redis/valkey store"` when `stores.length === 0`.

- [ ] **Step 1: Implement**

Structure = DebugPanel's Dialog skeleton (not a main-panel view — inherits dialog shortcut-safety):
- Header row of controls: store `<select aria-label="Session store">` (options from `stores`, first store preselected; empty state when none: prose "No stores declared — create one in the sidebar first."), subject filter `<input aria-label="Filter by subject" placeholder="subject">` with an Apply button, Refresh `DialogButton`.
- `refresh` useCallback (guarded by `open && store`): `api.listSessions({ store, subject: subject || undefined, limit: 50 })` → `setPage`; catch: `const parsed = parseApiError(e);` — 501 → `setHeadless(true)` (renders a DisabledNotice-style block: "This gateway build has no redis-store support — sessions require the default build."); 404 → onError('Unknown store', store); else onError('Failed to load sessions', parsed.error). `useEffect` on `[open, store, refresh]` to load on open/store-change. No polling (unlike traces — revocation lists don't need liveness; a Refresh button suffices).
- Rows: DebugPanel's stacked-card pattern — primary line `{s.subject || '(no subject)'} · {s.plugin}`, secondary `{s.policy && `${s.policy} · `}created {formatUnixTime(s.created_at)} · expires {formatUnixTime(s.expires_at)}`, right-aligned revoke button `aria-label={`Revoke session ${s.id}`}` (X icon, `var(--error)`) → `api.deleteSession(store, s.id)` then `refresh()`, toast via onError only on failure (success just disappears from the list — add a small `revokedCount` text flash instead of a toast to avoid toast spam).
- Load more: when `page.next_cursor`, a "Load more" `DialogButton` fetching with `cursor: page.next_cursor` and APPENDING to the list.
- Footer: "Revoke all for subject…" `DialogButton variant="danger"` enabled only when the subject filter is non-empty → confirm inline (two-step button: first click arms it, label becomes `Confirm revoke all for "${subject}"`, second click calls `api.deleteSessionsBySubject(store, subject)` → toast-ish inline text `Revoked N sessions` → refresh) + Close.
- Empty states distinguished: "No sessions in this store." vs "No sessions match this subject." (DebugPanel precedent).

- [ ] **Step 2: Wire App + Sidebar**

App: `const [sessionsOpen, setSessionsOpen] = useState(false);`, mount `<SessionsPanel open={sessionsOpen} onClose={() => setSessionsOpen(false)} stores={stores} onError={(title, message) => setToast({ tone: 'error', title, message })} />` beside DebugPanel; Sidebar gains `onOpenSessions` and the footer button ABOVE the Debug button, styled identically to it.

- [ ] **Step 3: Run + commit**

Run: `cd ui && npm test && npm run lint && npm run build`

```bash
git add ui/src/components/SessionsPanel.tsx ui/src/App.tsx ui/src/components/Sidebar.tsx
git commit -m "feat(ui): sessions panel — list, filter, revoke, revoke-by-subject"
```

---

### Task 8: (e2e) fixture store + ungated stores/sessions scenarios

**Files:**
- Modify: `e2e/fixtures/gateway.yaml` (top-level `stores:` + one dead-store entry)
- Create: `e2e/tests/stores-sessions.spec.ts` (ungated half)
- Modify: `e2e/E2E_TESTBOOK.md` (new `## Stores & sessions — tests/stores-sessions.spec.ts` section)

**Interfaces:**
- Consumes: admin endpoints (Global Constraints), UI hooks from Tasks 5-7 (`aria-label`s: "New store", "Ping store", "Sessions", "Session store", `Delete store ${name}`), helpers `adminApi`/`deleteRouteIfPresent`.
- Produces: fixture `stores:` entries `e2e-redis` (`url: ${FEATHERBIT_TEST_REDIS_URL:-redis://127.0.0.1:6379}`, `key_prefix: fbe2e`) and `e2e-dead` (`url: redis://127.0.0.1:1`, `connect_timeout_ms: 300`); scenario IDs `E2E-STORE-01..04`.

- [ ] **Step 1: Fixture** — add to `e2e/fixtures/gateway.yaml` (top level, near `consumers:`):

```yaml
stores:
  # Lazily-connecting: the suite boots fine with no redis running. The gated
  # session scenarios (E2E-SESS-*) skip unless FEATHERBIT_TEST_REDIS_URL is set.
  - name: e2e-redis
    type: redis
    url: ${FEATHERBIT_TEST_REDIS_URL:-redis://127.0.0.1:6379}
    key_prefix: fbe2e
  # Deliberately unreachable, for the ping-failure scenario.
  - name: e2e-dead
    type: redis
    url: redis://127.0.0.1:1
    connect_timeout_ms: 300
```

- [ ] **Step 2: Ungated scenarios** (full test bodies; follow the house spec idioms — delete-if-present, api.dispose, getByRole/aria-label locators):

- `E2E-STORE-01` (API): `GET /api/stores` lists the two fixture stores raw (`${FEATHERBIT_TEST_REDIS_URL:-…}` text verbatim in `url` — the never-resolve invariant); `POST` a duplicate `e2e-redis` → 409; `PUT` then `DELETE` a scratch store `e2e-tmp` round-trips.
- `E2E-STORE-02` (API): referenced-delete — `PUT /api/plugin-configs/e2e-store-ref` (`type: limit-count`, `config: {count: 1, time_window: 60, policy: redis, store: e2e-redis}`), `DELETE /api/stores/e2e-redis` → 409 with referrer `plugin_config 'e2e-store-ref'`; cleanup deletes the plugin config, and does NOT delete e2e-redis (fixture-owned).
- `E2E-STORE-03` (**Browser.**): create store `e2e-ui-store` via the sidebar dialog (name/type/url `redis://127.0.0.1:1`), see it in the sidebar, open it, click Ping (`getByRole('button', {name: 'Ping store'})`) → the failure message appears (either timeout or refused — assert `page.getByText(/Timed out|refused|connect/i)` is visible); delete it via the sidebar delete button + confirm dialog.
- `E2E-STORE-04` (API): sessions endpoint validation without redis — `GET /api/sessions` → 400; `GET /api/sessions?store=nope` → 404 (both unconditional; they never touch a backend).

- [ ] **Step 3: Testbook section** — new section before "Deliberately out of scope", with the four rows (and amend the out-of-scope etcd bullet to note redis now has gated in-suite coverage). Also state the section's env-gate convention for the E2E-SESS rows Task 9 adds.

- [ ] **Step 4: Run + commit**

Run: `cd e2e && npm test` (full suite — the four new scenarios plus all 129 existing pass with NO redis running; gated ones don't exist yet).

```bash
git add e2e/fixtures/gateway.yaml e2e/tests/stores-sessions.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "test(e2e): stores CRUD/ping/referrer scenarios and fixture stores"
```

---

### Task 9: (e2e) redis-gated session scenarios

**Files:**
- Modify: `e2e/fixtures/gateway.yaml` (redis-session OIDC route against mock-idp)
- Modify: `e2e/playwright.config.ts` (conditional env pass-through)
- Modify: `e2e/tests/stores-sessions.spec.ts` (gated describe block)
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: the mock-idp (`http://127.0.0.1:3011` — discovery/JWKS/authorize/token with REAL RS256); the existing interactive-OIDC fixture policy and spec (crib the login-drive mechanics from `e2e/tests/openid-connect.spec.ts`'s interactive scenarios and the fixture's existing interactive OIDC policy — reuse its authorize/callback choreography EXACTLY, only the session storage differs).
- Produces: route `oidc-redis` (`/oidc-redis/*`) → policy `oidc-redis-policy`: the existing interactive openid-connect node config duplicated with `session_storage: redis`, `session_store: e2e-redis`, `session_cookie_name: oidc_redis_session`, `redirect_uri: http://127.0.0.1:18081/oidc-redis/callback`, all four ports wired; scenarios `E2E-SESS-01..03`.

- [ ] **Step 1: playwright env pass-through** — in the gateway webServer `env` block:

```ts
        // Present only when the caller exported it: an EMPTY string would
        // defeat the fixture's ${FEATHERBIT_TEST_REDIS_URL:-...} default.
        ...(process.env.FEATHERBIT_TEST_REDIS_URL
          ? {FEATHERBIT_TEST_REDIS_URL: process.env.FEATHERBIT_TEST_REDIS_URL}
          : {}),
```

- [ ] **Step 2: Fixture route/policy** — duplicate the existing interactive OIDC policy block (same mock-idp endpoints/client/secret/session_secret), renamed ids, with the four config deltas above. Wire `success`/`denied`/`redirect`/`error` exactly as the original.

- [ ] **Step 3: Gated scenarios** — top of the describe block:

```ts
test.describe('Redis-backed sessions', () => {
  test.skip(!process.env.FEATHERBIT_TEST_REDIS_URL, 'FEATHERBIT_TEST_REDIS_URL not set');
```

- `E2E-SESS-01`: full interactive login on `/oidc-redis/echo` (drive the 302 → mock-idp authorize → callback exactly as the existing interactive OIDC scenario does); after login, assert the `oidc_redis_session` cookie value is a BARE 32-char lowercase-hex id (`/^[0-9a-f]{32}$/` — not a sealed blob), and the next request passes without touching the idp.
- `E2E-SESS-02`: `GET /api/sessions?store=e2e-redis` lists the session (subject from the mock-idp's sub claim; `plugin: 'openid-connect'`; `policy: 'oidc-redis-policy'`; `route: 'oidc-redis'` — the `__route`/`__policy` attribution asserted end to end); response has NO payload-like fields.
- `E2E-SESS-03` (**Browser.**): open the Sessions panel (footer button), pick store `e2e-redis`, see the row, click its revoke button (`aria-label` from Task 7); then assert the data plane re-enters login: `waitForDataPlane`-style poll on `/oidc-redis/echo` with the old cookie expecting a 302 to the idp.
- Teardown: `DELETE /api/sessions?store=e2e-redis&subject=<sub>` (idempotent cleanup).

- [ ] **Step 4: Run + commit**

Run WITHOUT redis: `cd e2e && npm test` — the three report as skipped, everything else green. Then WITH redis:

```bash
docker run -d --rm -p 16379:6379 --name fb-e2e-redis redis:7
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 npm test
docker stop fb-e2e-redis
```
Expected: all scenarios incl. E2E-SESS-01..03 pass.

```bash
git add e2e/fixtures/gateway.yaml e2e/playwright.config.ts e2e/tests/stores-sessions.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "test(e2e): redis-backed session login, listing, and revocation scenarios"
```

---

### Task 10: (CI) redis/valkey live matrix + e2e redis service

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the four env-gated Rust live tests (self-skip without `FEATHERBIT_TEST_REDIS_URL`); the gated e2e scenarios (Task 9). All redis-store-gated code compiles under `--no-default-features --features redis-store` (no ui/dist needed — verify once locally before writing the job).
- Produces: job `redis-live` (first `services:`/`strategy.matrix` in the repo) + a redis service on the existing `e2e` job.

- [ ] **Step 1: The matrix job** — insert after the `rust` job:

```yaml
  # Live-backend tests against real RESP servers. The env-gated tests
  # (FEATHERBIT_TEST_REDIS_URL) self-skip in every other job; here they run,
  # once per backend, proving the redis AND valkey claims. No UI build needed:
  # redis-store without the ui feature.
  redis-live:
    name: redis live tests (${{ matrix.backend.name }})
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        backend:
          - { name: redis-7, image: "redis:7" }
          - { name: valkey-8, image: "valkey/valkey:8" }
    services:
      redis:
        image: ${{ matrix.backend.image }}
        ports: ["6379:6379"]
        options: >-
          --health-cmd "redis-cli ping || valkey-cli ping"
          --health-interval 5s
          --health-timeout 3s
          --health-retries 20
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2

      - run: cargo test --locked --no-default-features --features redis-store
        env:
          FEATHERBIT_TEST_REDIS_URL: redis://127.0.0.1:6379
```

- [ ] **Step 2: e2e job gains redis** — add to the `e2e` job:

```yaml
    services:
      redis:
        image: redis:7
        ports: ["6379:6379"]
        options: >-
          --health-cmd "redis-cli ping"
          --health-interval 5s
          --health-timeout 3s
          --health-retries 20
```

and on its `Run e2e` step:

```yaml
        env:
          FEATHERBIT_TEST_REDIS_URL: redis://127.0.0.1:6379
```

(update the job's "No containers required" comment: the redis service unlocks the gated E2E-SESS scenarios; everything else remains process-local).

- [ ] **Step 3: Local verification** — `--no-default-features --features redis-store` must pass locally with a live redis:

```bash
docker run -d --rm -p 16379:6379 --name fb-ci-check redis:7
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 cargo test --locked --no-default-features --features redis-store 2>&1 | tail -3
docker stop fb-ci-check
```
Expected: green, with the four live tests actually running (they print nothing about skipping).

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: redis/valkey live-test matrix and e2e redis service"
```

---

### Task 11: (docs) concepts page, sidebar, roadmap, CLAUDE.md + final verification

**Files:**
- Create: `website/docs/concepts/stores.md`
- Modify: `website/sidebars.ts` (after `'concepts/plugin-configs'`)
- Modify: `website/docs/reference/roadmap.md`, `CLAUDE.md`

- [ ] **Step 1: The concept page** — front-matter `title: Shared Stores & Sessions`, `description: Named redis/valkey connections powering cluster-accurate rate limiting and revocable server-side sessions.` Body (follow plugin-configs.md's style: defining opening paragraph, `## H2` sections, yaml examples, field/endpoint tables, relative links): sections `## Declaring a store` (full YAML example + the StoreConfig field table incl. raw-`${ENV}` semantics and the v1 topology restriction), `## What uses stores` (limit-count/workflow `policy: redis`; the five session plugins' `session.storage: redis` with a link to each reference page), `## Server-side sessions` (id-cookie + sealed-at-rest model, 503 failure contract, refresh, the flat-vs-nested key precedence), `## Managing at runtime` (the `/api/stores` + `/api/sessions` endpoint tables from admin-api.md, the UI Stores editor/Sessions panel, revocation = store-backed only), `## Observability` (the two `gateway_*_store_errors_total` metrics).
- [ ] **Step 2: Spec §5 amendments + roadmap + CLAUDE.md** — spec (`docs/superpowers/specs/2026-08-21-session-storage-design.md` §5, inline "Amendment (as shipped):" style): (a) the interactive-login e2e runs against the suite's hermetic mock-idp (real RS256/JWKS), not the Keycloak realm in `tests/` — Keycloak stays a manual local playground; (b) restart-survival is covered by the Rust store round-trip tests, not e2e (the harness shares one gateway process per run). Roadmap: mark the UI panels/e2e/CI-matrix follow-ups shipped; remaining follow-ups (Sentinel/Cluster, rate-limit/limit-conn distributed backends, lua store API, etc.) unchanged. CLAUDE.md: extend the UI bullet (Stores editor, Sessions panel, store pickers) and the Shared-stores bullet (CI matrix exists); mention `E2E-STORE`/`E2E-SESS` in the e2e sentence if it names areas.
- [ ] **Step 3: Full verification**

```bash
cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings
cargo check --no-default-features && cargo clippy --no-default-features --all-targets -- -D warnings
cd ui && npm run lint && npm test && npm run build && cd ..
cd website && npm run build && cd ..
cd e2e && npm test && cd ..    # without redis: gated scenarios skip
```

- [ ] **Step 4: Commit**

```bash
git add website/docs/concepts/stores.md website/sidebars.ts website/docs/reference/roadmap.md CLAUDE.md
git commit -m "docs: shared stores & sessions concept page; roadmap and project docs updates"
```

---

## Out of scope (tracked)

- Workflow's limit-count `store` picker (raw-textarea rules panel — documented no-picker, per Plan 1's note).
- A capability-probe endpoint for headless builds (the panels infer from 501 — YAGNI until a second consumer needs it).
- Gateway-restart survival e2e (single shared gateway process per suite run — architecture of the harness; covered by the Rust store round-trip tests instead).
- jsdom/component tests for the panels (the repo's testing split is deliberate: vitest = pure logic, Playwright = behavior).
- Consumers panel (still UI-unwired; unrelated).
