# ── Stage 1: builder ──────────────────────────────────────────────────────────
# Full Node.js image so all devDependencies (TypeScript, Vite, Svelte compiler…)
# are available for the build step.  Nothing from this layer ships in the final
# image.
FROM node:22-alpine AS builder

WORKDIR /build

# Copy the lock-file and manifest first so npm ci is cached as long as
# dependencies haven't changed, even if source files have.
COPY aerotrack/package.json aerotrack/package-lock.json ./

# Install every dependency (including devDeps — needed by Vite / svelte-check).
RUN npm ci

# Copy the rest of the source tree.
COPY aerotrack/ .

# Compile the SvelteKit app with adapter-node.
# Output layout:
#   build/index.js      ← Node.js server entrypoint
#   build/handler.js    ← request handler (used by custom Node wrappers)
#   build/client/       ← static assets (JS bundles, CSS, hashed filenames)
#   build/server/       ← SSR server chunks
RUN npm run build


# ── Stage 2: runner ────────────────────────────────────────────────────────────
# Minimal Node.js runtime.  server.js (the custom entry point) uses the `ws`
# package at runtime, so we do need node_modules — but only production deps.
FROM node:22-alpine

# ── Non-root user ──────────────────────────────────────────────────────────────
RUN adduser -D aerotrack

WORKDIR /aerotrack

# Install only production dependencies (just `ws` and its peers; all devDeps
# are excluded).  Copy manifests first so this layer is cached on dep changes.
COPY --from=builder /build/package.json /build/package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts

# The compiled SvelteKit bundle.
COPY --from=builder --chown=aerotrack:aerotrack /build/build ./build

# Custom server entry point (WebSocket proxy + SvelteKit HTTP handler).
COPY --from=builder --chown=aerotrack:aerotrack /build/server.js .

USER aerotrack

# ── Runtime configuration ──────────────────────────────────────────────────────
# PORT        — TCP port the HTTP server listens on inside the container.
#               Map to whatever host / ALB target-group port you prefer.
#
# ORIGIN      — REQUIRED in production.  Set to the public URL the browser
#               uses to reach the app (e.g. https://aerotrack.yourdomain.com).
#               adapter-node uses this for CSRF protection; requests whose
#               Origin header doesn't match will be rejected with 403.
#
# AUTH_USER   — Login username shown on the sign-in form.  Default: admin.
# AUTH_PASS   — Login password.  No default — must be set explicitly.
# AUTH_SECRET — Random secret used to sign the session cookie.
#               Generate with: openssl rand -hex 32
#               Changing this value invalidates all existing sessions.
#
# AEROCOACH_URL — Internal URL of the aerocoach HTTP server.  The WebSocket
#                  proxy derives the WS URL from this automatically:
#                  http://… → ws://…/ws
#                  Example: http://10.0.1.42:8080  or  http://aerocoach.local:8080
#
# BODY_SIZE_LIMIT — Optional.  adapter-node default is 512 kB.
ENV PORT=3000 \
    NODE_ENV=production

EXPOSE 3000

CMD ["node", "server.js"]
