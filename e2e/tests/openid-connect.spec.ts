/**
 * openid-connect: bearer-token validation and the interactive Authorization Code
 * login, against the hermetic mock IdP in e2e/mock-idp/. See E2E_TESTBOOK.md.
 *
 * The plugin's 14 unit tests already cover the pieces in isolation (JWK parsing,
 * PKCE challenge, session seal, claim validation). What they cannot reach is the
 * wire: a token actually verified against a fetched JWKS, and a browser actually
 * bounced through an IdP and back with a sealed session cookie. That is here.
 */
import {test, expect, request} from '@playwright/test';

import {GATEWAY_URL, IDP_URL} from '../playwright.config';
import {dataPlane} from '../helpers/admin';

/** The echo backend reports the request as it reached the upstream. */
type Echo = {method: string; path: string; headers: Record<string, string>};

/** Asks the mock IdP for a real, correctly-signed token (or a chosen negative). */
async function mintToken(query = ''): Promise<string> {
  const res = await fetch(`${IDP_URL}/mint${query}`);
  const {token} = (await res.json()) as {token: string};
  return token;
}

/** Case-insensitive header lookup: hop-by-hop casing is not ours to rely on. */
function header(headers: Record<string, string>, name: string): string | undefined {
  const hit = Object.keys(headers).find((k) => k.toLowerCase() === name.toLowerCase());
  return hit ? headers[hit] : undefined;
}

test.describe('openid-connect: bearer mode', () => {
  test('E2E-OIDC-01: a request with no bearer token is rejected with 401', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/bearer/data');

    expect(res.status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-OIDC-02: a valid token is accepted and the claims reach the upstream', async () => {
    const traffic = await dataPlane();
    const token = await mintToken();

    const res = await traffic.get('/bearer/data', {headers: {authorization: `Bearer ${token}`}});
    expect(res.status()).toBe(200);

    const echo = (await res.json()) as Echo;
    expect(echo.path).toBe('/data');

    // set_userinfo_header: the validated claims are base64'd into x-userinfo.
    const userinfo = header(echo.headers, 'x-userinfo');
    expect(userinfo, 'x-userinfo must be forwarded to the upstream').toBeTruthy();
    const claims = JSON.parse(Buffer.from(userinfo!, 'base64').toString('utf8'));
    expect(claims.sub).toBe('alice');
    expect(claims.email).toBe('alice@example.com');

    await traffic.dispose();
  });

  /**
   * A well-formed RS256 token signed by a key that is NOT in the IdP's JWKS. The
   * gateway must fetch the JWKS and actually verify -- accepting this would mean
   * anyone can mint their own identities.
   */
  test('E2E-OIDC-03: a token signed by an unknown key is rejected', async () => {
    const traffic = await dataPlane();
    const forged = await mintToken('?wrong_key=1');

    const res = await traffic.get('/bearer/data', {headers: {authorization: `Bearer ${forged}`}});
    expect(res.status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-OIDC-04: an expired token is rejected', async () => {
    const traffic = await dataPlane();
    const expired = await mintToken('?expired=1');

    const res = await traffic.get('/bearer/data', {headers: {authorization: `Bearer ${expired}`}});
    expect(res.status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-OIDC-05: a token for a different audience is rejected', async () => {
    const traffic = await dataPlane();
    // match_with_client_id is on, so an aud that isn't our client_id must fail.
    const wrongAud = await mintToken('?aud=someone-else');

    const res = await traffic.get('/bearer/data', {headers: {authorization: `Bearer ${wrongAud}`}});
    expect(res.status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-OIDC-06: a garbage bearer value is rejected', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/bearer/data', {
      headers: {authorization: 'Bearer not-a-jwt-at-all'},
    });

    expect(res.status()).toBe(401);
    await traffic.dispose();
  });
});

test.describe('openid-connect: interactive login', () => {
  /**
   * The redirect the browser is sent on. Asserted without following it, so the
   * PKCE/state parameters are visible.
   */
  test('E2E-OIDC-07: an unauthenticated request is redirected to the IdP with PKCE', async () => {
    // maxRedirects: 0 -- we want the 302 itself, not where it lands.
    const raw = await request.newContext({baseURL: GATEWAY_URL, maxRedirects: 0});
    const res = await raw.get('/app/home');

    expect(res.status()).toBe(302);
    const location = new URL(res.headers()['location']);

    expect(location.origin + location.pathname).toBe(`${IDP_URL}/authorize`);
    expect(location.searchParams.get('client_id')).toBe('featherbit-e2e');
    expect(location.searchParams.get('response_type')).toBe('code');
    expect(location.searchParams.get('redirect_uri')).toBe(`${GATEWAY_URL}/app/callback`);
    // PKCE and CSRF protection must both be present.
    expect(location.searchParams.get('code_challenge')).toBeTruthy();
    expect(location.searchParams.get('code_challenge_method')).toBe('S256');
    expect(location.searchParams.get('state')).toBeTruthy();

    await raw.dispose();
  });

  /**
   * The whole dance, in a real browser: gateway -> IdP -> callback -> token
   * exchange -> sealed session cookie -> the upstream response finally rendered.
   * Nothing below the browser can test this.
   */
  test('E2E-OIDC-08: a browser completes the login and reaches the upstream', async ({page, context}) => {
    await page.goto(`${GATEWAY_URL}/app/home`);

    // We land on the echo backend's JSON, having been bounced through the IdP.
    const body = await page.locator('body').innerText();
    const echo = JSON.parse(body) as Echo;
    expect(echo.path).toBe('/home'); // the /app prefix was stripped, as for any route

    // The claims the IdP issued rode through to the upstream.
    const userinfo = header(echo.headers, 'x-userinfo');
    expect(userinfo).toBeTruthy();
    expect(JSON.parse(Buffer.from(userinfo!, 'base64').toString('utf8')).sub).toBe('alice');

    // And the session was sealed into a cookie, so the browser is now logged in.
    const session = (await context.cookies()).find((c) => c.name === 'oidc_session');
    expect(session, 'the oidc_session cookie must be set').toBeTruthy();
    expect(session!.value).not.toContain('alice'); // sealed, not plaintext claims
  });

  test('E2E-OIDC-09: the session cookie is replayed without a second trip to the IdP', async ({page, context}) => {
    await page.goto(`${GATEWAY_URL}/app/home`); // logs in, sets the cookie

    // Fail loudly if the gateway bounces us to the IdP again.
    let hitIdp = false;
    page.on('request', (r) => {
      if (r.url().startsWith(IDP_URL)) hitIdp = true;
    });

    await page.goto(`${GATEWAY_URL}/app/second-visit`);

    const echo = JSON.parse(await page.locator('body').innerText()) as Echo;
    expect(echo.path).toBe('/second-visit');
    expect(hitIdp, 'a valid session must not re-redirect to the identity provider').toBe(false);

    // Still the same session.
    expect((await context.cookies()).some((c) => c.name === 'oidc_session')).toBe(true);
  });

  test('E2E-OIDC-10: a forged session cookie is not accepted', async () => {
    const raw = await request.newContext({baseURL: GATEWAY_URL, maxRedirects: 0});

    // The cookie is sealed with the session secret; a made-up value must not pass.
    const res = await raw.get('/app/home', {
      headers: {cookie: 'oidc_session=not-a-real-sealed-session'},
    });

    // Rejected by being sent to log in again, rather than let through.
    expect(res.status()).toBe(302);
    expect(res.headers()['location']).toContain(`${IDP_URL}/authorize`);

    await raw.dispose();
  });
});
