// Custom Node.js server entry point.
//
// Combines the SvelteKit request handler (from build/handler.js) with a
// WebSocket proxy that:
//   - accepts browser connections on  GET /ws  (with session-cookie auth)
//   - maintains one persistent upstream WebSocket to aerocoach
//   - fans every aerocoach DashboardUpdate out to all connected browsers
//
// REST API calls (/status, /plan, /start, …) are handled by SvelteKit proxy
// routes in src/routes/ — no special treatment needed here.

import { createServer }                       from 'node:http';
import { createHmac, timingSafeEqual }        from 'node:crypto';
import { WebSocketServer, WebSocket }         from 'ws';
import { handler }                            from './build/handler.js';

// ── Config ────────────────────────────────────────────────────────────────

const PORT       = parseInt(process.env.PORT        ?? '3000', 10);
const COACH_HTTP = (process.env.AEROCOACH_URL       ?? 'http://localhost:8080').replace(/\/$/, '');
const COACH_WS   = COACH_HTTP.replace(/^http/, 'ws') + '/ws';

// ── Session auth (mirrors hooks.server.ts) ────────────────────────────────
// WebSocket upgrades bypass SvelteKit hooks, so we replicate the check here.

function expectedToken() {
  const secret = process.env.AUTH_SECRET ?? 'dev-insecure-secret';
  const user   = process.env.AUTH_USER   ?? 'admin';
  return createHmac('sha256', secret).update(user).digest('hex');
}

function parseCookies(header = '') {
  return Object.fromEntries(
    header.split(';')
      .map(s => s.trim().split('='))
      .filter(([k]) => k)
      .map(([k, ...v]) => [k.trim(), decodeURIComponent(v.join('=').trim())])
  );
}

function isAuthenticated(req) {
  const session  = parseCookies(req.headers.cookie)['_at_session'] ?? '';
  const expected = expectedToken();
  if (session.length !== expected.length) return false;
  try {
    return timingSafeEqual(Buffer.from(session), Buffer.from(expected));
  } catch {
    return false;
  }
}

// ── Upstream WebSocket (aerotrack → aerocoach) ────────────────────────────

/** All currently connected browser WebSocket clients. */
const clients    = new Set();
let   upstream   = null;
let   retryDelay = 2_000;

function connectUpstream() {
  console.log(`[ws-proxy] connecting to aerocoach: ${COACH_WS}`);

  // Node.js 22 built-in WebSocket — no extra package needed for the client leg.
  const ws = new WebSocket(COACH_WS);
  upstream = ws;

  ws.addEventListener('open', () => {
    retryDelay = 2_000;
    console.log('[ws-proxy] upstream connected');
  });

  ws.addEventListener('message', ({ data }) => {
    // Fan out to every authenticated browser client.
    const msg = typeof data === 'string' ? data : data.toString();
    for (const client of clients) {
      if (client.readyState === WebSocket.OPEN) client.send(msg);
    }
  });

  ws.addEventListener('close', ({ code }) => {
    upstream = null;
    console.log(`[ws-proxy] upstream closed (${code}), retry in ${retryDelay} ms`);
    setTimeout(connectUpstream, retryDelay);
    retryDelay = Math.min(retryDelay * 2, 30_000);
  });

  ws.addEventListener('error', ({ message }) => {
    // 'close' fires immediately after 'error' — reconnect logic lives there.
    console.error(`[ws-proxy] upstream error: ${message}`);
  });
}

// ── HTTP server ───────────────────────────────────────────────────────────

const server = createServer(handler);         // SvelteKit handles all HTTP
const wss    = new WebSocketServer({ noServer: true });

server.on('upgrade', (req, socket, head) => {
  const { pathname } = new URL(req.url, 'http://localhost');

  if (pathname !== '/ws') {
    socket.write('HTTP/1.1 404 Not Found\r\n\r\n');
    socket.destroy();
    return;
  }

  if (!isAuthenticated(req)) {
    socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n');
    socket.destroy();
    return;
  }

  wss.handleUpgrade(req, socket, head, (ws) => {
    clients.add(ws);
    console.log(`[ws-proxy] browser client connected (total: ${clients.size})`);
    ws.on('close', () => {
      clients.delete(ws);
      console.log(`[ws-proxy] browser client disconnected (total: ${clients.size})`);
    });
  });
});

server.listen(PORT, '0.0.0.0', () => {
  console.log(`Listening on http://0.0.0.0:${PORT}`);
  connectUpstream();
});
