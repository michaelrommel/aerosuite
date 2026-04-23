import { text } from '@sveltejs/kit';

// Lightweight liveness probe for the ALB health check.
// Returns 200 OK with no auth required (hooks.server.ts whitelists /health).
export const GET = () => text('OK');
