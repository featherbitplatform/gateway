import { describe, expect, it } from 'vitest';
import { REDACTED, redactSecrets } from './redact';

describe('redactSecrets', () => {
  it('leaves ordinary text, placeholders and already-masked values alone', () => {
    const t = 'token_count: 3\nscope: read\ntoken: ${FEATHERBIT_MCP_READ_TOKEN}\nauthorization: <redacted>\n"port": "denied"\npassthrough: true';
    expect(redactSecrets(t)).toEqual({ text: t, count: 0 });
  });

  it('redacts secret-keyed values in JSON, YAML and header form', () => {
    const r = redactSecrets(
      '{"password":"hunter2","client_secret": "abc","api_key":"k1","refresh_token":"r"}\nsecret_key: s3cr3t\nX-Api-Key: zzz\nCookie: sid=abc; theme=dark',
    );
    expect(r.text).toBe(
      `{"password":"${REDACTED}","client_secret": "${REDACTED}","api_key":"${REDACTED}","refresh_token":"${REDACTED}"}\nsecret_key: ${REDACTED}\nX-Api-Key: ${REDACTED}\nCookie: ${REDACTED}`,
    );
    expect(r.count).toBe(7);
  });

  it('redacts auth schemes, JWTs, PEM blocks and well-known key prefixes', () => {
    // jwt.io's sample token and a placeholder AWS key id below: fake by
    // construction, and the matcher cannot be tested without strings shaped
    // like real secrets, so the whole file sits in .semgrepignore.
    const jwt = 'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c';
    const r = redactSecrets(
      `use Bearer abcdefgh12345678 or ${jwt}\nkey sk-abcdefghijklmnopqrstuvwxyz and AKIAABCDEFGHIJKLMNOP\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----`,
    );
    expect(r.text).toBe(`use Bearer ${REDACTED} or ${REDACTED}\nkey ${REDACTED} and ${REDACTED}\n${REDACTED}`);
    expect(r.count).toBe(5);
  });

  it('redacts literal secrets of 8+ chars anywhere and ignores shorter ones', () => {
    const r = redactSecrets('my key is sk-live-XYZ and short is abc', ['sk-live-XYZ', 'abc']);
    expect(r.text).toBe(`my key is ${REDACTED} and short is abc`);
    expect(r.count).toBe(1);
  });

  it('redacts prefixed secret keys without catching lookalike keys', () => {
    const r = redactSecrets('db_password: x\n"oauth_client_secret":"y"\ncustom-api-key: z\ncsrf_token=abc');
    expect(r.text).toBe(
      `db_password: ${REDACTED}\n"oauth_client_secret":"${REDACTED}"\ncustom-api-key: ${REDACTED}\ncsrf_token=${REDACTED}`,
    );
    expect(r.count).toBe(4);

    const safe = 'token_count: 3\npassthrough: true\nbypass: 1';
    expect(redactSecrets(safe)).toEqual({ text: safe, count: 0 });
  });

  it('is idempotent', () => {
    const once = redactSecrets('password: x\nBearer abcdefgh12345678').text;
    expect(redactSecrets(once)).toEqual({ text: once, count: 0 });
  });
});
