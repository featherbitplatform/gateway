/**
 * HTTP-delegation auth plugins: forward-auth and opa, against the mock auth
 * service in e2e/mock-auth/. See E2E_TESTBOOK.md ("External auth").
 *
 * Same rationale as the OIDC suite: the unit tests cover request-building and
 * response-parsing in isolation, but never the actual callout -- the gateway
 * making a real HTTP request to the auth service, interpreting a real response,
 * and letting the request through (or not) accordingly. Decisions here are driven
 * off the path: `.../allow/...` is permitted, anything else denied.
 */
import {test, expect} from '@playwright/test';

import {dataPlane} from '../helpers/admin';

type Echo = {method: string; path: string; headers: Record<string, string>};

function header(headers: Record<string, string>, name: string): string | undefined {
  const hit = Object.keys(headers).find((k) => k.toLowerCase() === name.toLowerCase());
  return hit ? headers[hit] : undefined;
}

test.describe('forward-auth', () => {
  test('E2E-FAUTH-01: an allowed request is proxied, with the auth header forwarded', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/fauth/allow/resource');

    expect(res.status()).toBe(200);
    const echo = (await res.json()) as Echo;
    expect(echo.path).toBe('/allow/resource'); // /fauth stripped after the allow

    // upstream_headers copied X-Auth-User from the auth response onto the upstream request.
    expect(header(echo.headers, 'x-auth-user')).toBe('alice');
    await traffic.dispose();
  });

  test('E2E-FAUTH-02: a denied request is rejected with the auth service status', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/fauth/deny/resource');

    expect(res.status()).toBe(403);
    // client_headers mirrored X-Deny-Reason from the auth response back to the client.
    expect(res.headers()['x-deny-reason']).toBe('forbidden-by-mock');
    await traffic.dispose();
  });

  test('E2E-FAUTH-03: a denied request never reaches the upstream', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/fauth/deny/secret');
    // A 403 with no echo JSON body proves the upstream was not called.
    expect(res.status()).toBe(403);
    expect(await res.text()).not.toContain('"method"');
    await traffic.dispose();
  });
});

test.describe('opa', () => {
  test('E2E-OPA-01: an allow decision proxies and forwards the OPA header', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/opa/allow/thing');

    expect(res.status()).toBe(200);
    const echo = (await res.json()) as Echo;
    expect(echo.path).toBe('/allow/thing');
    // send_headers_upstream copied X-Opa-User from the decision onto the upstream.
    expect(header(echo.headers, 'x-opa-user')).toBe('alice');
    await traffic.dispose();
  });

  test('E2E-OPA-02: a deny decision is rejected with the OPA-supplied status and reason', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/opa/deny/thing');

    expect(res.status()).toBe(403);
    expect(await res.text()).toContain('denied-by-opa'); // the OPA "reason" became the body
    await traffic.dispose();
  });
});

test.describe('authz-casdoor (bearer / introspection)', () => {
  test('E2E-CASDOOR-01: a request with no bearer token is rejected', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/casdoor/thing')).status()).toBe(403);
    await traffic.dispose();
  });

  test('E2E-CASDOOR-02: an active token (per introspection) is allowed through', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/casdoor/thing', {
      headers: {authorization: 'Bearer valid-token'},
    });

    expect(res.status()).toBe(200);
    expect(((await res.json()) as Echo).path).toBe('/thing');
    await traffic.dispose();
  });

  test('E2E-CASDOOR-03: an inactive token is rejected', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/casdoor/thing', {
      headers: {authorization: 'Bearer expired-or-revoked'},
    });
    expect(res.status()).toBe(403);
    await traffic.dispose();
  });
});

test.describe('wolf-rbac', () => {
  // The plugin requires a V1#<appid>#<token> rbac token before it even calls
  // wolf-server; the mock then decides on the request path.
  const RBAC_TOKEN = 'V1#restful#valid-token';

  test('E2E-WOLF-01: an allowed resource is proxied', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/wolf/allow/thing', {headers: {'x-rbac-token': RBAC_TOKEN}});

    expect(res.status()).toBe(200);
    expect(((await res.json()) as Echo).path).toBe('/allow/thing');
    await traffic.dispose();
  });

  test('E2E-WOLF-02: a denied resource is rejected', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/wolf/deny/thing', {headers: {'x-rbac-token': RBAC_TOKEN}});
    expect(res.status()).toBeGreaterThanOrEqual(400);
    await traffic.dispose();
  });

  test('E2E-WOLF-03: a malformed rbac token is rejected before any callout', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/wolf/allow/thing', {headers: {'x-rbac-token': 'not-valid'}});
    expect(res.status()).toBeGreaterThanOrEqual(400);
    await traffic.dispose();
  });
});

test.describe('authz-keycloak (UMA)', () => {
  // The UMA flow requires the caller to present an access token, which the plugin
  // exchanges for a decision. Its value is irrelevant to the mock (which decides
  // on the requested permission), but it must be present or the plugin rejects
  // before any callout.
  const BEARER = {authorization: 'Bearer access-token'};

  test('E2E-KC-01: a permitted resource is proxied', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/kc-allow/thing', {headers: BEARER});

    expect(res.status()).toBe(200);
    expect(((await res.json()) as Echo).path).toBe('/thing');
    await traffic.dispose();
  });

  test('E2E-KC-02: a refused permission is rejected with 403 (via the real callout)', async () => {
    const traffic = await dataPlane();
    // With a token present, the 403 comes from the mock's deny decision, not from
    // the missing-token short-circuit.
    expect((await traffic.get('/kc-deny/thing', {headers: BEARER})).status()).toBe(403);
    await traffic.dispose();
  });

  test('E2E-KC-03: a request with no bearer token is rejected before any callout', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/kc-allow/thing')).status()).toBe(403);
    await traffic.dispose();
  });
});

test.describe('cas-auth (stateless ticket validation)', () => {
  test('E2E-CAS-01: a request with no ticket is rejected', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/casauth/thing')).status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-CAS-02: a valid ticket is validated and the request proceeds', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/casauth/thing?ticket=good-ticket');

    expect(res.status()).toBe(200);
    expect(((await res.json()) as Echo).path).toBe('/thing');
    await traffic.dispose();
  });

  test('E2E-CAS-03: an invalid ticket is rejected', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/casauth/thing?ticket=bogus')).status()).toBe(401);
    await traffic.dispose();
  });
});

test.describe('dingtalk-auth (code exchange)', () => {
  test('E2E-DINGTALK-01: a valid code is exchanged and the request proceeds', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/dingtalk/thing?code=good-code');

    expect(res.status()).toBe(200);
    expect(((await res.json()) as Echo).path).toBe('/thing');
    await traffic.dispose();
  });

  test('E2E-DINGTALK-02: an unresolvable code is rejected', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/dingtalk/thing?code=bad-code')).status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-DINGTALK-03: a request with no code is rejected', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/dingtalk/thing')).status()).toBeGreaterThanOrEqual(400);
    await traffic.dispose();
  });
});

test.describe('feishu-auth (code exchange)', () => {
  test('E2E-FEISHU-01: a valid code is exchanged and the request proceeds', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/feishu/thing?code=good-code');

    expect(res.status()).toBe(200);
    expect(((await res.json()) as Echo).path).toBe('/thing');
    await traffic.dispose();
  });

  test('E2E-FEISHU-02: an unresolvable code is rejected', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/feishu/thing?code=bad-code')).status()).toBe(401);
    await traffic.dispose();
  });
});

test.describe('basic-auth (users in the UI array shape)', () => {
  // Regression guard for the UI form / plugin config mismatch: the web UI
  // serializes `users` as an array of {username, password}, which the plugin
  // used to reject as empty -- so a basic-auth node built in the editor could not
  // be saved. The basicauth-policy fixture uses that exact array shape; these
  // tests confirm it actually authenticates.
  const basic = (user: string, pass: string) => ({
    authorization: 'Basic ' + Buffer.from(`${user}:${pass}`).toString('base64'),
  });

  test('E2E-BASIC-01: valid credentials from an array-shaped users config pass', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/basicauth/thing', {headers: basic('alice', 'secret')});
    expect(res.status()).toBe(200);
    expect(((await res.json()) as Echo).path).toBe('/thing');
    await traffic.dispose();
  });

  test('E2E-BASIC-02: a wrong password is rejected', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/basicauth/thing', {headers: basic('alice', 'nope')})).status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-BASIC-03: no credentials are challenged', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/basicauth/thing');
    expect(res.status()).toBe(401);
    expect(res.headers()['www-authenticate']).toContain('Basic');
    await traffic.dispose();
  });
});

test.describe('ldap-auth (real LDAP bind)', () => {
  // HTTP Basic credentials -> a real simple bind against the mock LDAP server.
  const basic = (user: string, pass: string) => ({
    authorization: 'Basic ' + Buffer.from(`${user}:${pass}`).toString('base64'),
  });

  test('E2E-LDAP-01: valid credentials bind successfully and the request proceeds', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/ldap/thing', {headers: basic('alice', 'secret')});

    expect(res.status()).toBe(200);
    expect(((await res.json()) as Echo).path).toBe('/thing');
    await traffic.dispose();
  });

  test('E2E-LDAP-02: a wrong password fails the bind and is rejected', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/ldap/thing', {headers: basic('alice', 'wrong')});

    expect(res.status()).toBe(401);
    expect(res.headers()['www-authenticate']).toContain('Basic');
    await traffic.dispose();
  });

  test('E2E-LDAP-03: an unknown user is rejected', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/ldap/thing', {headers: basic('nobody', 'secret')})).status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-LDAP-04: a request with no credentials is challenged', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/ldap/thing');
    expect(res.status()).toBe(401);
    expect(res.headers()['www-authenticate']).toContain('Basic');
    await traffic.dispose();
  });
});
