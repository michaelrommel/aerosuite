/**
 * Reactive dashboard state (Svelte 5 runes).
 *
 * A singleton `DashboardStore` that owns the WebSocket connection and
 * applies incoming `DashboardUpdate` payloads to reactive $state fields.
 * Import `dashboard` wherever live data is needed.
 */

import { WebSocketClient } from './websocket';
import type { AgentSnapshot, DashboardUpdate, GlobalStats } from '$lib/types';
import type { WsStatus } from './websocket';

// Build the WebSocket URL from the current browser origin so it works
// both in development (Vite proxy) and production (same host/port).
function makeWsUrl(): string {
	if (typeof window === 'undefined') return 'ws://localhost:8080/ws';
	const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
	return `${proto}//${window.location.host}/ws`;
}

class DashboardStore {
	/** All known agents, keyed by agent_index (0–99). */
	agents = $state<Map<number, AgentSnapshot>>(new Map());

	globalStats  = $state<GlobalStats | null>(null);
	currentSlice = $state<number>(0);
	totalSlices  = $state<number>(0);
	lastUpdated  = $state<number | null>(null);
	wsStatus     = $state<WsStatus>('connecting');

	private client: WebSocketClient;

	constructor() {
		this.client = new WebSocketClient(makeWsUrl());
		this.client.onUpdate       = (u) => this.applyUpdate(u);
		this.client.onStatusChange = (s) => { this.wsStatus = s; };
		// connect() is called from a $effect in the root layout/page to ensure
		// it only runs in the browser (not during SSR).
	}

	/** Call once when the app mounts in the browser. */
	startWs(): void {
		this.client.connect();
	}

	/** Apply one DashboardUpdate received over the WebSocket. */
	applyUpdate(update: DashboardUpdate): void {
		this.currentSlice = update.current_slice;
		this.totalSlices  = update.total_slices;
		this.globalStats  = update.global_stats;
		this.lastUpdated  = update.timestamp_ms;

		const next = new Map<number, AgentSnapshot>();
		for (const agent of update.agents) {
			next.set(agent.agent_index, agent);
		}
		this.agents = next;
	}

	/**
	 * Pace icon for one agent: 🐇 if within one slice of the master clock,
	 * 🐢 if lagging by more than one slice.
	 */
	paceIcon(agentSlice: number): '🐇' | '🐢' {
		return agentSlice >= this.currentSlice - 1 ? '🐇' : '🐢';
	}
}

export const dashboard = new DashboardStore();
