import { proxyRequest } from '$lib/server/proxy';
import type { RequestHandler } from './$types';

// Streams the NDJSON result file; Content-Disposition forwarded so the
// browser triggers a file-save with the correct filename.
export const GET: RequestHandler = ({ request }) => proxyRequest('/results', request);
