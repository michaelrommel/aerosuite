import { proxyRequest } from '$lib/server/proxy';
import type { RequestHandler } from './$types';

export const POST: RequestHandler = ({ request }) => proxyRequest('/stop', request);
