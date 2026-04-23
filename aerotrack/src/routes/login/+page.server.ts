import { fail, redirect } from '@sveltejs/kit';
import { createHmac } from 'node:crypto';
import { env } from '$env/dynamic/private';
import type { Actions, PageServerLoad } from './$types';

// ── Token helper (must stay in sync with hooks.server.ts) ─────────────────

function makeToken(): string {
	const secret = env.AUTH_SECRET ?? 'dev-insecure-secret';
	const user   = env.AUTH_USER   ?? 'admin';
	return createHmac('sha256', secret).update(user).digest('hex');
}

// ── Load ──────────────────────────────────────────────────────────────────

// Bounce already-authenticated users straight to the dashboard.
export const load: PageServerLoad = async ({ cookies, url }) => {
	if (cookies.get('_at_session') === makeToken()) {
		redirect(303, url.searchParams.get('next') ?? '/');
	}
	return {};
};

// ── Actions ───────────────────────────────────────────────────────────────

export const actions: Actions = {
	// POST /login?/login
	login: async ({ request, cookies, url }) => {
		const data = await request.formData();
		const user = (data.get('username') ?? '').toString().trim();
		const pass = (data.get('password') ?? '').toString();

		// Both checks run unconditionally to avoid leaking which field was wrong.
		const userOk = user === (env.AUTH_USER ?? 'admin');
		const passOk = pass === (env.AUTH_PASS ?? '');

		if (!userOk || !passOk) {
			// Return 401 with a generic message — don't indicate which field failed.
			return fail(401, { error: 'Invalid credentials.' });
		}

		cookies.set('_at_session', makeToken(), {
			path:     '/',
			httpOnly: true,
			// secure: true only over HTTPS; allows dev on http://localhost
			secure:   url.protocol === 'https:',
			sameSite: 'lax',
			maxAge:   60 * 60 * 8, // 8 hours
		});

		redirect(303, url.searchParams.get('next') ?? '/');
	},

	// POST /login?/logout  (called from ControlBar's logout form)
	logout: async ({ cookies }) => {
		cookies.delete('_at_session', { path: '/' });
		redirect(303, '/login');
	},
};
