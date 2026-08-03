/**
 * Universal `{{path}}` template rendering, the field-scoped suggestion
 * popover (full context vs. env-only), and the env-var name catalog.
 * See E2E_TESTBOOK.md ("Universal templates").
 *
 * Idioms reused from var-suggestions.spec.ts (openRoute, the debug trigger
 * header, PUT-policy/POST-route reset-then-seed) and editor.spec.ts (closing
 * the inspector before Save Policy is clickable).
 */
import {test, expect, type Page} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent, waitForDataPlane} from '../helpers/admin';
import {ENV_CANARY_NAME, ENV_CANARY_VALUE} from '../playwright.config';

/** Header that opts a single request into tracing (see debug.spec.ts). */
const DEBUG_HEADER = {'x-featherbit-debug': '1'};

/** Opens a route's policy on the canvas and waits for the graph to render (see editor.spec.ts). */
async function openRoute(page: Page, route: string) {
  await page.goto('/');
  await page.getByText(route, {exact: true}).click();
  await page.waitForSelector('.react-flow__node');
}

test.describe('Universal templates', () => {
  test('E2E-TPL-01: {{request.method}} renders while a literal $ passes through untouched', async () => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tpl-render-api');
    await api.delete('/api/policies/tpl-render-policy');

    // listener -> proxy-rewrite (add_headers with a {{path}} ref alongside a
    // literal $) -> echo backend -> client. add_headers only ever runs the
    // {{...}} template engine (Template::parse), never legacy $-interpolation
    // (src/plugins/native/proxy_rewrite.rs), so "$keep" must reach the
    // upstream byte-for-byte while "{{request.method}}" resolves.
    const policyRes = await api.put('/api/policies/tpl-render-policy', {
      data: {
        name: 'tpl-render-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'rewrite',
            type: 'proxy-rewrite',
            config: {
              phase: 'request',
              add_headers: [{name: 'x-tpl', value: 'm={{request.method}} $keep'}],
            },
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'rewrite.in'},
          {from: 'rewrite.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'tpl-render-api', match: {path: '/tpl-render/*', methods: ['GET']}, policy: 'tpl-render-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    const traffic = await dataPlane();
    const {body} = await waitForDataPlane(traffic, '/tpl-render/ping', (status, text) => {
      if (status !== 200) return false;
      try {
        return (JSON.parse(text) as {headers: Record<string, string>}).headers?.['x-tpl'] !== undefined;
      } catch {
        return false;
      }
    });
    const echo = JSON.parse(body) as {headers: Record<string, string>};
    // Rendered: {{request.method}} -> GET. Untouched: the literal $keep, byte-for-byte.
    expect(echo.headers['x-tpl']).toBe('m=GET $keep');

    await traffic.dispose();
    await api.delete('/api/routes/tpl-render-api');
    await api.delete('/api/policies/tpl-render-policy');
    await api.dispose();
  });

  test('E2E-TPL-02: the add_headers value field offers live {{path}} suggestions and saves', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tpl-editor-api');
    await api.delete('/api/policies/tpl-editor-policy');
    await api.delete('/api/debug/traces');

    const policyRes = await api.put('/api/policies/tpl-editor-policy', {
      data: {
        name: 'tpl-editor-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'rewrite',
            type: 'proxy-rewrite',
            config: {phase: 'request', add_headers: [{name: 'x-hdr', value: ''}]},
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'rewrite.in'},
          {from: 'rewrite.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'tpl-editor-api', match: {path: '/tpl-editor/*', methods: ['GET']}, policy: 'tpl-editor-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    // A traced GET so `rewrite`'s predecessor snapshot (the listener's
    // initial capture) has a real request.method to preview.
    const traffic = await dataPlane();
    const traced = await traffic.get('/tpl-editor/ping', {headers: DEBUG_HEADER});
    expect(traced.status()).toBe(200);
    await traffic.dispose();

    await openRoute(page, 'tpl-editor-api');
    await page.locator('.react-flow__node', {hasText: 'rewrite'}).first().click();

    // The add_headers "Value" sub-field: template: 'full', so it offers
    // request/response/message context, not just env.* -- unlike E2E-TPL-03's
    // env-only field.
    const valueField = page.getByPlaceholder('$uuid');
    await expect(valueField).toBeVisible();
    await valueField.click();
    await valueField.fill('{{re');
    await valueField.press('End');

    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    await expect(popover.getByText('request.method', {exact: true})).toBeVisible();
    // Live value from the traced GET's predecessor snapshot.
    await expect(popover.getByText('GET', {exact: true})).toBeVisible();

    // "{{re" alone also matches request.path/response.*/etc. -- narrow the
    // filter to the one row this scenario cares about before hitting Enter,
    // so insertion is deterministic regardless of the catalog's row order.
    await valueField.fill('{{request.meth');
    await valueField.press('End');
    await expect(popover.getByText('request.method', {exact: true})).toBeVisible();
    await valueField.press('Enter');
    await expect(valueField).toHaveValue('{{request.method}}');

    // The inspector overlay sits on top of the toolbar's Save Policy button.
    await page.getByRole('button', {name: 'Close'}).click();
    await page.getByRole('button', {name: 'Save Policy'}).click();

    const saved = (await (await api.get('/api/policies/tpl-editor-policy')).json()) as {
      nodes: {id: string; config?: {add_headers?: {name: string; value: string}[]}}[];
    };
    const rewrite = saved.nodes.find((n) => n.id === 'rewrite')!;
    expect(rewrite.config?.add_headers?.[0]?.value).toBe('{{request.method}}');

    await api.delete('/api/routes/tpl-editor-api');
    await api.delete('/api/policies/tpl-editor-policy');
    await api.dispose();
  });

  test('E2E-TPL-03: an env-only field offers only {{env.*}} suggestions, filterable by name', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tpl-env-api');
    await api.delete('/api/policies/tpl-env-policy');

    const policyRes = await api.put('/api/policies/tpl-env-policy', {
      data: {
        name: 'tpl-env-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'blocker',
            type: 'uri-blocker',
            config: {block_rules: ['^/tpl-env-blocked/']},
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'blocker.in'},
          {from: 'blocker.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'tpl-env-api', match: {path: '/tpl-env/*', methods: ['GET']}, policy: 'tpl-env-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    await openRoute(page, 'tpl-env-api');
    await page.locator('.react-flow__node', {hasText: 'blocker'}).first().click();

    // block_rules' item field is `template: 'env-only'`.
    const ruleField = page.getByPlaceholder('^/admin/');
    await expect(ruleField).toBeVisible();
    await ruleField.click();
    await ruleField.fill('{{');
    await ruleField.press('End');

    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    // env.LOG_LEVEL is set explicitly on the gateway's own launch env
    // (playwright.config.ts) -- a name every run is guaranteed to see.
    await expect(popover.getByText('env.LOG_LEVEL', {exact: true})).toBeVisible();
    // Only env.* is on offer: no request/response/message rows leak in.
    await expect(popover.getByText('request.method', {exact: true})).toHaveCount(0);
    await expect(popover.getByText(/^request\./)).toHaveCount(0);

    await ruleField.fill('{{env.LOG');
    await ruleField.press('End');
    await expect(popover).toBeVisible();
    await expect(popover.getByText('env.LOG_LEVEL', {exact: true})).toBeVisible();
    // A real env name that does not match the "LOG" filter must drop out.
    await expect(popover.getByText('env.ECHO_HOST', {exact: true})).toHaveCount(0);

    await api.delete('/api/routes/tpl-env-api');
    await api.delete('/api/policies/tpl-env-policy');
    await api.dispose();
  });

  test('E2E-TPL-04: GET /api/env-vars lists sorted names and never values', async () => {
    const api = await adminApi();
    const res = await api.get('/api/env-vars');
    expect(res.status()).toBe(200);

    const bodyText = await res.text();
    const body = JSON.parse(bodyText) as {names: string[]};
    const names = body.names;
    expect(Array.isArray(names)).toBeTruthy();
    expect(names.length).toBeGreaterThan(0);

    const sorted = [...names].sort();
    expect(names).toEqual(sorted);

    // The canary name (set only on the gateway's own launch env) is visible...
    expect(names).toContain(ENV_CANARY_NAME);
    // ...but its value never is: names only, never values. (Not asserting
    // the body has no bare "=" at all -- Windows has real env var *names*
    // containing one, e.g. the per-drive cwd tracker "=C:".)
    expect(bodyText).not.toContain(ENV_CANARY_VALUE);

    await api.dispose();
  });

  test('E2E-TPL-05: the expanded template editor modal browses, filters, applies, and discards on Escape', async ({
    page,
  }) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tpl-modal-api');
    await api.delete('/api/policies/tpl-modal-policy');
    await api.delete('/api/debug/traces');

    const policyRes = await api.put('/api/policies/tpl-modal-policy', {
      data: {
        name: 'tpl-modal-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'rewrite',
            type: 'proxy-rewrite',
            config: {phase: 'request', add_headers: [{name: 'x-hdr', value: ''}]},
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'rewrite.in'},
          {from: 'rewrite.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'tpl-modal-api', match: {path: '/tpl-modal/*', methods: ['GET']}, policy: 'tpl-modal-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    // A traced GET carrying a deliberately long custom header, so the
    // listener's predecessor snapshot has a `request.headers.<h>` entry whose
    // full path is long enough to need the modal's no-truncation wrapping
    // (unlike the inline popover's single-line ellipsis) to read in full.
    const traffic = await dataPlane();
    const traced = await traffic.get('/tpl-modal/ping', {
      headers: {...DEBUG_HEADER, 'x-a-very-long-custom-header-name-for-modal': 'value-123'},
    });
    expect(traced.status()).toBe(200);
    await traffic.dispose();

    await openRoute(page, 'tpl-modal-api');
    await page.locator('.react-flow__node', {hasText: 'rewrite'}).first().click();

    const headerPath = 'request.headers.x-a-very-long-custom-header-name-for-modal';
    const valueField = page.getByPlaceholder('$uuid');
    await expect(valueField).toBeVisible();
    await valueField.click();
    await valueField.fill('{{');
    await valueField.press('End');

    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    await page.locator('button[aria-label="Expand template editor"]').click();

    const modal = page.getByTestId('template-editor-modal');
    await expect(modal).toBeVisible();

    // The suggestion row for the traced custom header renders its FULL path
    // (exact-string locator, not a substring match) and its live value --
    // the modal's whole reason to exist is that this row would otherwise be
    // ellipsis-truncated in the inline popover.
    const suggestions = page.getByTestId('template-editor-suggestions');
    await expect(suggestions.getByText(headerPath, {exact: true})).toBeVisible();
    await expect(suggestions.getByText('value-123', {exact: true})).toBeVisible();

    // Scroll the panel -- the row found above must still be the same element
    // (not a stale/detached one) after scrolling the suggestion list.
    await suggestions.evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    await expect(suggestions.getByText(headerPath, {exact: true})).toBeVisible();

    // Filter in the modal's own input, then Enter-insert.
    const modalInput = page.getByTestId('template-editor-input');
    await modalInput.fill('{{request.headers.x-a-very');
    await modalInput.press('End');
    await expect(suggestions.getByText(headerPath, {exact: true})).toBeVisible();
    await modalInput.press('Enter');
    await expect(modalInput).toHaveValue(`{{${headerPath}}}`);

    // Apply -> the inspector's own field now carries the inserted path.
    // (Scoped to the dialog role, not just `page`: the Apply/Cancel buttons
    // are Dialog's own footer, a sibling of the `template-editor-modal`
    // testid div, not a descendant of it.)
    const editorDialog = page.getByRole('dialog', {name: 'Template editor'});
    await editorDialog.getByRole('button', {name: 'Apply'}).click();
    await expect(modal).toBeHidden();
    await expect(valueField).toHaveValue(`{{${headerPath}}}`);

    // Reopen via Ctrl+Space (the modal's other entry point besides the
    // popover button), edit the draft, then Escape -- the field must be
    // untouched by the discarded edit.
    await valueField.click();
    await valueField.press('Control+Space');
    await expect(modal).toBeVisible();
    const modalInputReopened = page.getByTestId('template-editor-input');
    await expect(modalInputReopened).toHaveValue(`{{${headerPath}}}`);
    await modalInputReopened.fill(`{{${headerPath}}} and then some more text`);
    await page.keyboard.press('Escape');
    await expect(modal).toBeHidden();
    await expect(valueField).toHaveValue(`{{${headerPath}}}`);

    await api.delete('/api/routes/tpl-modal-api');
    await api.delete('/api/policies/tpl-modal-policy');
    await api.dispose();
  });

  test('E2E-SV-01: set-vars composes a value once, a downstream node reuses it, and the popover previews it live', async ({
    page,
  }) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'sv-compose-api');
    await api.delete('/api/policies/sv-compose-policy');
    await api.delete('/api/debug/traces');

    // listener -> set-vars (computes `tenant` from the request header) ->
    // proxy-rewrite (reads it back via {{message.tenant}}) -> echo -> client.
    const policyRes = await api.put('/api/policies/sv-compose-policy', {
      data: {
        name: 'sv-compose-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'vars',
            type: 'set-vars',
            config: {vars: [{name: 'tenant', value: '{{request.headers.x-tenant-id}}'}]},
          },
          {
            id: 'rewrite',
            type: 'proxy-rewrite',
            config: {phase: 'request', add_headers: [{name: 'x-tenant', value: '{{message.tenant}}'}]},
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'vars.in'},
          {from: 'vars.success', to: 'rewrite.in'},
          {from: 'rewrite.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'sv-compose-api', match: {path: '/sv-compose/*', methods: ['GET']}, policy: 'sv-compose-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    // First an untraced probe, polled until the hot-swap lands (route
    // creation and the swap it triggers aren't synchronous -- see
    // waitForDataPlane's doc comment). Once it's live, a second, traced
    // request carries the real tenant header: this both proves the
    // compose-once/reuse-downstream data flow on live traffic AND seeds the
    // trace the popover's live preview reads below.
    //
    // The probe sends its own `x-tenant-id` header (any value) rather than
    // relying on `x-tenant` showing up from an *empty* render -- readiness
    // shouldn't hinge on set-vars/proxy-rewrite still emitting a header when
    // its template renders to the empty string; asserting on a real,
    // non-empty value keeps the probe honest about what "the route is live"
    // actually means here.
    const traffic = await dataPlane();
    await waitForDataPlane(
      traffic,
      '/sv-compose/ping',
      (status, text) => {
        if (status !== 200) return false;
        try {
          return (JSON.parse(text) as {headers: Record<string, string>}).headers?.['x-tenant'] !== undefined;
        } catch {
          return false;
        }
      },
      {headers: {'x-tenant-id': 'probe'}},
    );
    const traced = await traffic.get('/sv-compose/ping', {
      headers: {...DEBUG_HEADER, 'x-tenant-id': 'acme'},
    });
    expect(traced.status()).toBe(200);
    const echo = JSON.parse(await traced.text()) as {headers: Record<string, string>};
    // set-vars computed `tenant` from the header; proxy-rewrite read it back
    // via {{message.tenant}} -- the whole point of "compose once, reuse downstream".
    expect(echo.headers['x-tenant']).toBe('acme');
    await traffic.dispose();

    await openRoute(page, 'sv-compose-api');
    await page.locator('.react-flow__node', {hasText: 'rewrite'}).first().click();

    const valueField = page.getByPlaceholder('$uuid');
    await expect(valueField).toBeVisible();
    await valueField.click();
    await valueField.fill('{{me');
    await valueField.press('End');

    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    // `message.tenant` -- set by the upstream set-vars node -- is offered
    // with its live value from the traced request's predecessor snapshot.
    // The same traced request also yields a `request.headers.x-tenant-id`
    // row whose live value is independently "acme", so asserting "acme"
    // anywhere in the popover wouldn't prove the *message*-namespace preview
    // binding works -- scope the value check to the message.tenant row
    // itself (its parent is the clickable row div that also holds the
    // ValueCell span; see VarInput.tsx).
    const messageTenantName = popover.getByText('message.tenant', {exact: true});
    await expect(messageTenantName).toBeVisible();
    const messageTenantRow = messageTenantName.locator('xpath=..');
    await expect(messageTenantRow.getByText('acme', {exact: true})).toBeVisible();

    await api.delete('/api/routes/sv-compose-api');
    await api.delete('/api/policies/sv-compose-policy');
    await api.dispose();
  });

  test('E2E-SV-02: an env-only field shows the fixed-at-load footer in both the popover and the template editor modal', async ({
    page,
  }) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'sv-env-api');
    await api.delete('/api/policies/sv-env-policy');

    // proxy-rewrite's `remove_headers` items are `template: 'env-only'`
    // (unlike `add_headers`' Value field, which is `'full'` -- see E2E-TPL-02).
    const policyRes = await api.put('/api/policies/sv-env-policy', {
      data: {
        name: 'sv-env-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'rewrite',
            type: 'proxy-rewrite',
            config: {phase: 'request', remove_headers: ['x-powered-by']},
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'rewrite.in'},
          {from: 'rewrite.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'sv-env-api', match: {path: '/sv-env/*', methods: ['GET']}, policy: 'sv-env-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    await openRoute(page, 'sv-env-api');
    await page.locator('.react-flow__node', {hasText: 'rewrite'}).first().click();

    const removeHeaderField = page.getByPlaceholder('x-powered-by');
    await expect(removeHeaderField).toBeVisible();
    await removeHeaderField.click();
    await removeHeaderField.fill('{{');
    await removeHeaderField.press('End');

    // ENV_ONLY_MESSAGE (ui/src/varSuggestions.ts) can't be imported here --
    // the e2e project has no dependency on `ui`'s React/api modules -- so
    // this asserts a distinctive substring of the exact copy instead.
    const footerText = /fixed when configuration loads/;
    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    await expect(popover.getByText(footerText)).toBeVisible();

    await page.locator('button[aria-label="Expand template editor"]').click();
    const modal = page.getByTestId('template-editor-modal');
    await expect(modal).toBeVisible();
    await expect(modal.getByText(footerText)).toBeVisible();

    await api.delete('/api/routes/sv-env-api');
    await api.delete('/api/policies/sv-env-policy');
    await api.dispose();
  });
});
