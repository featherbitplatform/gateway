/**
 * A hermetic mock for the HTTP-delegation auth plugins in the e2e suite.
 *
 * Each endpoint speaks one plugin's real contract, so the gateway cannot tell it
 * from the genuine service it fronts. Decisions are driven off the request path
 * (".../allow/..." permits, anything else denies) so a test needs no special
 * headers -- just the URL it hits.
 *
 * Endpoints (port 3012, PORT overrides):
 *   ANY  /forward-auth/verify   forward-auth: 200 + X-Auth-User on allow, 403 + X-Deny-Reason on deny
 *   POST /v1/data/e2e/allow     opa: {result:{allow, headers, reason, status}}
 *   POST /introspect            RFC 7662-style: {active} for casdoor/wolf-rbac-style token checks
 *   GET  /oauth/token           code -> token exchange for code-based auth (dingtalk/feishu-style)
 *   GET  /cas/serviceValidate   CAS ticket validation (XML), ticket "good-ticket" succeeds
 *   POST /keycloak/token        Keycloak UMA: RPT request, permits when the resource path allows
 */
import {createServer} from 'node:http';

const PORT = Number(process.env.PORT ?? 3012);

const send = (res, status, headers, body) => {
  res.writeHead(status, {...headers, 'content-length': Buffer.byteLength(body ?? '')});
  res.end(body ?? '');
};
const json = (res, status, obj, extra = {}) =>
  send(res, status, {'content-type': 'application/json', ...extra}, JSON.stringify(obj));

async function readBody(req) {
  let body = '';
  for await (const chunk of req) body += chunk;
  return body;
}

/** The path the gateway was originally asked for is what we authorize on. */
const allows = (p) => /\/allow(\/|$|\?)/.test(p ?? '');

const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);

  // ---- forward-auth ------------------------------------------------------
  // The gateway sends the original request context as X-Forwarded-* headers.
  if (url.pathname === '/forward-auth/verify') {
    const forwardedUri = req.headers['x-forwarded-uri'] ?? '';
    if (allows(forwardedUri)) {
      // 2xx = allow. upstream_headers lets the gateway copy X-Auth-User onward.
      return send(res, 200, {'x-auth-user': 'alice'}, '');
    }
    // >=300 = deny. client_headers lets the gateway mirror X-Deny-Reason back.
    return send(res, 403, {'x-deny-reason': 'forbidden-by-mock'}, 'denied');
  }

  // ---- OPA ---------------------------------------------------------------
  if (url.pathname === '/v1/data/e2e/allow' && req.method === 'POST') {
    const input = JSON.parse((await readBody(req)) || '{}')?.input ?? {};
    const path = input?.request?.path ?? '';
    if (allows(path)) {
      return json(res, 200, {result: {allow: true, headers: {'x-opa-user': 'alice'}}});
    }
    return json(res, 200, {result: {allow: false, status: 403, reason: 'denied-by-opa'}});
  }

  // ---- Casdoor RFC 7662 introspection ------------------------------------
  // POST /api/login/oauth/introspect, form token=, Basic auth. active:true allows.
  if (url.pathname === '/api/login/oauth/introspect' && req.method === 'POST') {
    const form = new URLSearchParams(await readBody(req));
    const token = form.get('token');
    if (token === 'valid-token') {
      return json(res, 200, {active: true, sub: 'alice', username: 'alice', scope: 'read'});
    }
    return json(res, 200, {active: false});
  }

  // ---- wolf-rbac access check --------------------------------------------
  // GET /wolf/rbac/access_check?resName=<path>&action=<method>. 200 = allow.
  if (url.pathname === '/wolf/rbac/access_check') {
    const resName = url.searchParams.get('resName') ?? '';
    if (allows(resName)) {
      return json(res, 200, {
        ok: true,
        data: {userInfo: {id: 123, username: 'alice', nickname: 'Alice'}},
      });
    }
    return json(res, 403, {ok: false, reason: 'denied-by-wolf'});
  }

  // ---- DingTalk: app access token, then userinfo-by-code -----------------
  // The app token step always succeeds (app credentials); the user's code is
  // validated at getuserinfo.
  if (url.pathname === '/dingtalk/accessToken' && req.method === 'POST') {
    return json(res, 200, {accessToken: 'dt-app-token', expireIn: 7200});
  }
  if (url.pathname === '/dingtalk/getuserinfo' && req.method === 'POST') {
    const {code} = JSON.parse((await readBody(req)) || '{}');
    if (code === 'good-code') {
      return json(res, 200, {errcode: 0, result: {userid: 'alice', name: 'Alice'}});
    }
    return json(res, 200, {errcode: 40078, errmsg: 'invalid code'});
  }

  // ---- Feishu: code -> user access token, then userinfo ------------------
  // The token step carries the code, so that is where a bad code is rejected
  // (Feishu signals errors with a nonzero `code` field, not an HTTP status).
  if (url.pathname === '/feishu/token' && req.method === 'POST') {
    const {code} = JSON.parse((await readBody(req)) || '{}');
    if (code === 'good-code') {
      return json(res, 200, {code: 0, access_token: 'fs-user-token', expires_in: 7200});
    }
    return json(res, 200, {code: 20037, error_description: 'invalid code'});
  }
  if (url.pathname === '/feishu/userinfo') {
    return json(res, 200, {code: 0, data: {user_id: 'alice', name: 'Alice'}});
  }

  // ---- CAS ticket validation (XML) ---------------------------------------
  if (url.pathname === '/cas/serviceValidate') {
    const ticket = url.searchParams.get('ticket');
    if (ticket === 'good-ticket') {
      return send(
        res,
        200,
        {'content-type': 'application/xml'},
        `<cas:serviceResponse xmlns:cas="http://www.yale.edu/tp/cas"><cas:authenticationSuccess><cas:user>alice</cas:user></cas:authenticationSuccess></cas:serviceResponse>`,
      );
    }
    return send(
      res,
      200,
      {'content-type': 'application/xml'},
      `<cas:serviceResponse xmlns:cas="http://www.yale.edu/tp/cas"><cas:authenticationFailure code="INVALID_TICKET"/></cas:serviceResponse>`,
    );
  }

  // ---- Keycloak UMA permission (RPT) -------------------------------------
  if (url.pathname === '/keycloak/token' && req.method === 'POST') {
    const form = new URLSearchParams(await readBody(req));
    // A resource-set request permits when the requested permission names an
    // "allow" resource; otherwise the authorization is refused.
    const permission = form.get('permission') ?? '';
    if (permission.includes('allow')) {
      return json(res, 200, {access_token: 'rpt-token', token_type: 'Bearer'});
    }
    return json(res, 403, {error: 'access_denied'});
  }

  json(res, 404, {error: 'not_found', path: url.pathname});
});

server.listen(PORT, '127.0.0.1', () => console.log(`[mock-auth] listening on :${PORT}`));
