import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [sveltekit()],

	server: {
		// Proxy all aerocoach HTTP + WebSocket traffic so the Vite dev server
		// can sit in front of both the SvelteKit app and the backend.
		proxy: {
			'/ws':        { target: 'ws://localhost:8080',   ws: true,  changeOrigin: true },
			'/status':    { target: 'http://localhost:8080', changeOrigin: true },
			'/plan':      { target: 'http://localhost:8080', changeOrigin: true },
			'/start':     { target: 'http://localhost:8080', changeOrigin: true },
			'/stop':      { target: 'http://localhost:8080', changeOrigin: true },
			'/reset':     { target: 'http://localhost:8080', changeOrigin: true },
			'/bandwidth': { target: 'http://localhost:8080', changeOrigin: true },
			'/results':   { target: 'http://localhost:8080', changeOrigin: true },
			'/health':    { target: 'http://localhost:8080', changeOrigin: true },
		},
	},
});
