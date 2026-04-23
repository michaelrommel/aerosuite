/**
 * WebSocket client with automatic exponential-backoff reconnection.
 *
 * Usage:
 *   const ws = new WebSocketClient('ws://localhost:8080/ws');
 *   ws.onUpdate = (update) => dashboard.applyUpdate(update);
 *   ws.onStatusChange = (s) => console.log(s);
 *   ws.connect();
 */

import type { DashboardUpdate } from '$lib/types';

export type WsStatus = 'connecting' | 'open' | 'closed' | 'error';

export class WebSocketClient {
	private ws: WebSocket | null = null;
	private reconnectDelay = 1_000;  // ms; doubles on each failure up to maxDelay
	private readonly maxDelay = 30_000;
	private stopped = false;

	onUpdate: ((update: DashboardUpdate) => void) | null = null;
	onStatusChange: ((status: WsStatus) => void) | null = null;

	constructor(private readonly url: string) {}

	/** Open the connection (or schedule a reconnect if the URL is unreachable). */
	connect(): void {
		this.stopped = false;
		this.tryConnect();
	}

	/** Close the connection and stop reconnecting. */
	disconnect(): void {
		this.stopped = true;
		this.ws?.close();
	}

	private tryConnect(): void {
		if (this.stopped) return;
		this.onStatusChange?.('connecting');

		try {
			this.ws = new WebSocket(this.url);
		} catch {
			this.scheduleReconnect();
			return;
		}

		this.ws.onopen = () => {
			this.reconnectDelay = 1_000; // reset on successful connect
			this.onStatusChange?.('open');
		};

		this.ws.onmessage = (event: MessageEvent) => {
			try {
				const update: DashboardUpdate = JSON.parse(event.data as string);
				this.onUpdate?.(update);
			} catch {
				console.error('[WebSocketClient] failed to parse message:', event.data);
			}
		};

		this.ws.onerror = () => {
			this.onStatusChange?.('error');
		};

		this.ws.onclose = () => {
			if (!this.stopped) {
				this.onStatusChange?.('closed');
				this.scheduleReconnect();
			}
		};
	}

	private scheduleReconnect(): void {
		if (this.stopped) return;
		setTimeout(() => this.tryConnect(), this.reconnectDelay);
		this.reconnectDelay = Math.min(this.reconnectDelay * 2, this.maxDelay);
	}
}
