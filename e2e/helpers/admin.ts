import {request, type APIRequestContext} from '@playwright/test';

import {ADMIN_URL, ADMIN_PASS, ADMIN_USER, GATEWAY_URL} from '../playwright.config';

/** An authenticated client for the admin API (:19091). */
export async function adminApi(): Promise<APIRequestContext> {
  return request.newContext({
    baseURL: ADMIN_URL,
    httpCredentials: {username: ADMIN_USER, password: ADMIN_PASS},
  });
}

/**
 * A client for the *data plane* (:18081) -- the port real traffic arrives on.
 * Deliberately unauthenticated: the data plane has nothing to do with admin
 * credentials, and the loop tests assert on what an ordinary client would see.
 */
export async function dataPlane(): Promise<APIRequestContext> {
  return request.newContext({baseURL: GATEWAY_URL});
}

/** Route names the specs create, so a failed run can be cleaned up on the next. */
export async function deleteRouteIfPresent(api: APIRequestContext, name: string) {
  const res = await api.get('/api/routes');
  const routes = (await res.json()) as {name: string}[];
  if (routes.some((r) => r.name === name)) {
    await api.delete(`/api/routes/${name}`);
  }
}

/**
 * Polls the data plane until `check` passes.
 *
 * A policy saved through the admin API is validated, recompiled and hot-swapped;
 * the swap is quick but not synchronous with the HTTP response, so asserting on
 * live traffic immediately after a save is a race. This is the honest way to wait
 * for it -- and if the swap never lands, the test fails on timeout rather than
 * on a stale first read.
 */
export async function waitForDataPlane(
  api: APIRequestContext,
  path: string,
  check: (status: number, body: string) => boolean,
  timeoutMs = 10_000,
): Promise<{status: number; body: string}> {
  const deadline = Date.now() + timeoutMs;
  let last = {status: 0, body: ''};
  while (Date.now() < deadline) {
    const res = await api.get(path);
    last = {status: res.status(), body: await res.text()};
    if (check(last.status, last.body)) return last;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(
    `data plane never satisfied the check for ${path}; last was ${last.status} ${last.body.slice(0, 200)}`,
  );
}
