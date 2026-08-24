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
