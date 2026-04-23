// Route guard: every request that isn't headed for /login is checked for a
// valid session cookie.  No session → redirect to /login with ?next= so the
// user lands back where they were after authenticating.
//
// Session token = HMAC-SHA256(AUTH_SECRET, AUTH_USER).
// Stateless — no session store needed.  Invalidated automatically when the
// container restarts with a fresh AUTH_SECRET.

import { redirect } from '@sveltejs/kit';
import { createHmac, timingSafeEqual } from 'node:crypto';
import { env } from '$env/dynamic/private';
import type { Handle } from '@sveltejs/kit';

// ── Helpers ───────────────────────────────────────────────────────────────

/** Derive the expected session token from runtime environment variables. */
function expectedToken(): string {
	const secret = env.AUTH_SECRET ?? 'dev-insecure-secret';
	const user   = env.AUTH_USER   ?? 'admin';
	return createHmac('sha256', secret).update(user).digest('hex');
}

/**
 * Constant-time string comparison.
 * Falls back to `false` if lengths differ (avoids throwing from
 * `timingSafeEqual` on mismatched buffer sizes while still resisting
 * timing attacks on equal-length inputs).
 */
function safeEq(a: string, b: string): boolean {
	// Intentional short-circuit on length: length is not secret information
	// because HMAC hex output is always 64 chars, and we control `b`.
	if (a.length !== b.length) return false;
	try {
		return timingSafeEqual(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8'));
	} catch {
		return false;
	}
}

// ── Handle ────────────────────────────────────────────────────────────────

export const handle: Handle = async ({ event, resolve }) => {
	// Public paths — login page and ALB health check, no auth required.
	if (
		event.url.pathname.startsWith('/login') ||
		event.url.pathname === '/health'
	) {
		return resolve(event);
	}

	const cookie = event.cookies.get('_at_session') ?? '';

	if (!safeEq(cookie, expectedToken())) {
		const next = encodeURIComponent(event.url.pathname + event.url.search);
		redirect(303, `/login?next=${next}`);
	}

	return resolve(event);
};
