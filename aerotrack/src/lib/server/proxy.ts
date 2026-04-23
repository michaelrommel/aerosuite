/**
 * Thin HTTP proxy to the aerocoach backend.
 *
 * All REST routes in src/routes/ call proxyRequest() so they share the same
 * error handling and header forwarding.  The AEROCOACH_URL env var is the
 * only thing that needs to change between environments.
 */

import { env } from '$env/dynamic/private';

function coachBase(): string {
  return (env.AEROCOACH_URL ?? 'http://localhost:8080').replace(/\/$/, '');
}

/**
 * Forward `request` to `path` on the aerocoach server and return the
 * response as-is.  On network failure, returns a 502 JSON error so the
 * browser gets a readable message instead of a SvelteKit 500 page.
 */
export async function proxyRequest(path: string, request: Request): Promise<Response> {
  const url    = coachBase() + path;
  const isRead = request.method === 'GET' || request.method === 'HEAD';

  let res: Response;
  try {
    res = await fetch(url, {
      method:  request.method,
      headers: isRead ? {} : { 'Content-Type': request.headers.get('Content-Type') ?? 'application/json' },
      body:    isRead ? undefined : await request.arrayBuffer(),
    });
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error(`[proxy] ${request.method} ${path} failed: ${msg}`);
    return new Response(
      JSON.stringify({ error: 'aerocoach unreachable', detail: msg }),
      { status: 502, headers: { 'Content-Type': 'application/json' } }
    );
  }

  // Forward headers that clients care about.
  const headers: Record<string, string> = {};
  const ct = res.headers.get('Content-Type');
  const cd = res.headers.get('Content-Disposition');
  if (ct) headers['Content-Type'] = ct;
  if (cd) headers['Content-Disposition'] = cd;

  return new Response(res.body, { status: res.status, headers });
}
