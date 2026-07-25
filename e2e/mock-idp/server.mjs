/**
 * A hermetic mock OpenID Connect provider for the e2e suite.
 *
 * Real IdPs (Keycloak, Auth0) mean containers, network, and minutes per run. This
 * is the smallest thing that is still a *truthful* OIDC provider: a discovery
 * document, a JWKS, an authorization endpoint that redirects back with a code,
 * and a token endpoint that exchanges that code for genuinely RS256-signed tokens.
 * The gateway cannot tell it apart from the real thing -- it fetches the JWKS and
 * verifies signatures for real.
 *
 * Endpoints:
 *   GET  /.well-known/openid-configuration   discovery
 *   GET  /jwks.json                          public keys (the gateway verifies against these)
 *   GET  /authorize                          302 back to redirect_uri with ?code&state
 *   POST /token                              code -> {id_token, access_token}
 *   GET  /mint                               TEST-ONLY: mint a bearer token to order
 *                                            (?sub&aud&iss&expired=1&wrong_key=1)
 *
 * Port 3011 (PORT overrides).
 */
import {createServer} from 'node:http';
import {generateKeyPair, exportJWK, SignJWT} from 'jose';

const PORT = Number(process.env.PORT ?? 3011);
const ISSUER = `http://127.0.0.1:${PORT}`;
const ALG = 'RS256';

// The key the gateway will trust (published in the JWKS)...
const {publicKey, privateKey} = await generateKeyPair(ALG, {extractable: true});
const jwk = {...(await exportJWK(publicKey)), kid: 'test-key-1', alg: ALG, use: 'sig'};

// ...and one it will not. Used to forge a token with a valid shape but no
// corresponding public key, which must be rejected.
const {privateKey: attackerKey} = await generateKeyPair(ALG, {extractable: true});

/** Authorization codes issued by /authorize, redeemed once at /token. */
const codes = new Map();

const json = (res, status, body) => {
  const payload = JSON.stringify(body);
  res.writeHead(status, {'content-type': 'application/json', 'content-length': Buffer.byteLength(payload)});
  res.end(payload);
};

/** Signs an id/access token. `opts.expired` and `opts.wrongKey` produce the negatives. */
async function mint({sub = 'alice', aud = 'featherbit-e2e', nonce, expired = false, wrongKey = false} = {}) {
  const now = Math.floor(Date.now() / 1000);
  const claims = {sub, email: `${sub}@example.com`, name: sub, ...(nonce ? {nonce} : {})};

  return new SignJWT(claims)
    .setProtectedHeader({alg: ALG, kid: 'test-key-1'})
    .setIssuer(ISSUER)
    .setAudience(aud)
    .setIssuedAt(expired ? now - 7200 : now)
    .setExpirationTime(expired ? now - 3600 : now + 3600) // already expired, or valid for an hour
    .sign(wrongKey ? attackerKey : privateKey);
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, ISSUER);

  if (url.pathname === '/.well-known/openid-configuration') {
    return json(res, 200, {
      issuer: ISSUER,
      authorization_endpoint: `${ISSUER}/authorize`,
      token_endpoint: `${ISSUER}/token`,
      jwks_uri: `${ISSUER}/jwks.json`,
      response_types_supported: ['code'],
      subject_types_supported: ['public'],
      id_token_signing_alg_values_supported: [ALG],
      code_challenge_methods_supported: ['S256'],
    });
  }

  if (url.pathname === '/jwks.json') {
    return json(res, 200, {keys: [jwk]});
  }

  // The browser lands here. A real IdP would show a login form; we consent
  // immediately and bounce straight back, which keeps the test deterministic.
  if (url.pathname === '/authorize') {
    const redirectUri = url.searchParams.get('redirect_uri');
    const state = url.searchParams.get('state');
    if (!redirectUri) return json(res, 400, {error: 'invalid_request', detail: 'no redirect_uri'});

    const code = `code-${Math.random().toString(36).slice(2)}`;
    codes.set(code, {
      nonce: url.searchParams.get('nonce') ?? undefined,
      codeChallenge: url.searchParams.get('code_challenge') ?? undefined,
      clientId: url.searchParams.get('client_id') ?? undefined,
    });

    const back = new URL(redirectUri);
    back.searchParams.set('code', code);
    if (state) back.searchParams.set('state', state);
    res.writeHead(302, {location: back.toString()});
    return res.end();
  }

  // The gateway calls this server-to-server to redeem the code.
  if (url.pathname === '/token' && req.method === 'POST') {
    let body = '';
    for await (const chunk of req) body += chunk;
    const form = new URLSearchParams(body);
    const code = form.get('code');
    const issued = code ? codes.get(code) : undefined;

    if (!issued) return json(res, 400, {error: 'invalid_grant'});
    codes.delete(code); // single use, like a real IdP

    // PKCE: the gateway must present a verifier when it sent a challenge.
    if (issued.codeChallenge && !form.get('code_verifier')) {
      return json(res, 400, {error: 'invalid_grant', detail: 'PKCE verifier missing'});
    }

    const aud = issued.clientId ?? 'featherbit-e2e';
    return json(res, 200, {
      token_type: 'Bearer',
      expires_in: 3600,
      id_token: await mint({aud, nonce: issued.nonce}),
      access_token: await mint({aud}),
    });
  }

  // Test-only: hand a spec a token of a given shape, so the negative cases are
  // real tokens the gateway must reject rather than random strings.
  if (url.pathname === '/mint') {
    const q = url.searchParams;
    return json(res, 200, {
      token: await mint({
        sub: q.get('sub') ?? 'alice',
        aud: q.get('aud') ?? 'featherbit-e2e',
        expired: q.get('expired') === '1',
        wrongKey: q.get('wrong_key') === '1',
      }),
    });
  }

  json(res, 404, {error: 'not_found', path: url.pathname});
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[mock-idp] issuer ${ISSUER}`);
});
