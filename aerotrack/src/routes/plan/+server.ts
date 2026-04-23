import { proxyRequest } from '$lib/server/proxy';
import type { RequestHandler } from './$types';

export const GET:   RequestHandler = ({ request }) => proxyRequest('/plan', request);
export const PUT:   RequestHandler = ({ request }) => proxyRequest('/plan', request);
export const PATCH: RequestHandler = ({ request }) => proxyRequest('/plan', request);
