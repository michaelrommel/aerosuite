import { proxyRequest } from '$lib/server/proxy';
import type { RequestHandler } from './$types';

export const GET: RequestHandler = ({ request }) => proxyRequest('/plans', request);
