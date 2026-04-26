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
	/** Coach state string as returned by GET /status, e.g. "RUNNING(slice=0)". */
	coachState   = $state<string>('WAITING');

	/**
	 * Timestamp (ms) of the first DashboardUpdate received for the current
	 * slice.  Used by PlanPanel to track the dot’s position across the slice.
	 * Reset whenever currentSlice advances.
	 */
	sliceStartMs = $state<number | null>(null);

	/**
	 * Per-tick history of `active_connections` — up to HISTORY_MAX entries,
	 * newest last.  Persists across DONE; cleared only on explicit reset.
	 */
	connectionsHistory = $state<number[]>([]);

	/**
	 * Per-tick history of `current_bandwidth_bps` — up to HISTORY_MAX entries,
	 * newest last.  Persists across DONE; cleared only on explicit reset.
	 */
	bandwidthHistory = $state<number[]>([]);

	private static readonly HISTORY_MAX = 30;

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
		// Record the timestamp of the first update in each new slice so the
		// PlanPanel dot can compute its position within the slice band.
		// Must be checked before currentSlice is overwritten.
		if (update.current_slice !== this.currentSlice || this.sliceStartMs === null) {
			this.sliceStartMs = update.timestamp_ms;
		}

		this.currentSlice = update.current_slice;
		this.totalSlices  = update.total_slices;
		this.lastUpdated  = update.timestamp_ms;

		// Freeze global stats once the test is done so that late-arriving
		// MetricsUpdates from the agent's graceful-drain phase don't overwrite
		// the numbers that were accurate at the moment the test ended.
		if (!this.coachState.startsWith('DONE')) {
			this.globalStats = update.global_stats;
		}

		const next = new Map<number, AgentSnapshot>();
		for (const agent of update.agents) {
			next.set(agent.agent_index, agent);
		}
		this.agents = next;

		// Append to sparkline histories only while the test is running so the
		// charts don't drift during WAITING or after the test ends.
		if (this.coachState.startsWith('RUNNING')) {
			const hmax = DashboardStore.HISTORY_MAX;
			this.connectionsHistory = [
				...this.connectionsHistory,
				update.global_stats.active_connections
			].slice(-hmax);
			this.bandwidthHistory = [
				...this.bandwidthHistory,
				update.global_stats.current_bandwidth_bps
			].slice(-hmax);
		}
	}

	/**
	 * Notify the store of the current coach state (WAITING / RUNNING / DONE).
	 * Called by ControlBar after every /status poll so history gating stays
	 * in sync without requiring a separate WebSocket field.
	 */
	setCoachState(state: string): void {
		// When transitioning into RUNNING, clear the slice-start anchor so the
		// first RUNNING update sets it from scratch instead of reusing a stale
		// WAITING-era timestamp (which would make the dot start mid-slice).
		if (state.startsWith('RUNNING') && !this.coachState.startsWith('RUNNING')) {
			this.sliceStartMs = null;
		}
		this.coachState = state;
	}

	/**
	 * Clear sparkline histories.  Call when the operator issues a reset so
	 * the charts start fresh for the next test run.
	 */
	clearHistory(): void {
		this.connectionsHistory = [];
		this.bandwidthHistory   = [];
		this.sliceStartMs       = null;
	}

	/**
	 * Rotation icon for one agent card.
	 *
	 * Mirrors the Rust constants in aerogym/src/agent/session.rs:
	 *   DRAIN_FLEET_THRESHOLD = 25
	 *   ROTATION_GROUPS       = 20  (each group ≈ 5% of the fleet)
	 *   ACTIVE_GROUPS         = 12  (60% active at any time)
	 *
	 * Returns 🔥 when the agent is in its active window (running FTP
	 * transfers) and 💤 when it is in its 8-slice cooldown window
	 * (connections closed so keepalived can expire its persistence entry).
	 * Below the fleet threshold all agents stay permanently active (🔥).
	 */
	rotationIcon(agentIndex: number): '🔥' | '💤' {
		const DRAIN_FLEET_THRESHOLD = 25;
		const ROTATION_GROUPS       = 20;
		const ACTIVE_GROUPS         = 12;

		if (this.agents.size < DRAIN_FLEET_THRESHOLD) return '🔥';

		const group  = agentIndex % ROTATION_GROUPS;
		const sMod   = this.currentSlice % ROTATION_GROUPS;
		const active = (group + ROTATION_GROUPS - sMod) % ROTATION_GROUPS < ACTIVE_GROUPS;
		return active ? '🔥' : '💤';
	}
}

export const dashboard = new DashboardStore();
