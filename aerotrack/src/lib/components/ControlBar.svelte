<script lang="ts">
	import { dashboard } from '$lib/stores/dashboard.svelte';
	import { plan } from '$lib/stores/plan.svelte';

	let coachState      = $state<string>('WAITING');
	let connectedAgents = $state<number>(0);

	async function fetchStatus() {
		try {
			const res = await fetch('/status');
			if (!res.ok) return;
			const data = await res.json();
			coachState      = (data.state as string) ?? 'WAITING';
			connectedAgents = (data.connected as number) ?? 0;
		} catch { /* server not reachable yet — silently ignore */ }
	}

	// Poll /status every 3 s. WS updates $dashboard but not the state badge.
	$effect(() => {
		fetchStatus();
		const id = setInterval(fetchStatus, 3_000);
		return () => clearInterval(id);
	});

	async function start() {
		const res = await fetch('/start', { method: 'POST' });
		if (!res.ok) {
			const d = await res.json().catch(() => ({})) as { error?: string };
			alert(d.error ?? `Start failed (${res.status})`);
		}
		await fetchStatus();
	}

	async function stop() {
		const res = await fetch('/stop', { method: 'POST' });
		if (!res.ok) {
			const d = await res.json().catch(() => ({})) as { error?: string };
			alert(d.error ?? `Stop failed (${res.status})`);
		}
		await fetchStatus();
	}

	async function reset() {
		const res = await fetch('/reset', { method: 'POST' });
		if (!res.ok) {
			const d = await res.json().catch(() => ({})) as { error?: string };
			alert(d.error ?? `Reset failed (${res.status})`);
		}
		await fetchStatus();
	}

	const isWaiting = $derived(coachState.startsWith('WAITING'));
	const isRunning = $derived(coachState.startsWith('RUNNING'));
	const isDone    = $derived(coachState.startsWith('DONE'));
	const stateClass = $derived(isWaiting ? 'waiting' : isRunning ? 'running' : isDone ? 'done' : '');
</script>

<header class="control-bar">
	<!-- Coach state badge -->
	<div class="state-badge {stateClass}">{coachState}</div>

	<!-- Connected agent count -->
	<div class="agents-info">
		<span class="count">{connectedAgents}</span>
		<span class="count">agents</span>
	</div>

	<!-- WebSocket status circle: solid green = connected, red outline = not -->
	<div
		class="ws-indicator"
		class:connected={dashboard.wsStatus === 'open'}
		title="WebSocket: {dashboard.wsStatus}"
	></div>

	<div class="spacer"></div>

	<!-- Action buttons — only relevant ones shown per state -->
	{#if isWaiting}
		<button class="btn start" onclick={start}>▶ Start</button>
	{/if}
	{#if isRunning}
		<button class="btn stop" onclick={stop}>■ Stop</button>
	{/if}
	{#if isDone}
		<a class="btn download" href="/results" download>⬇ Results</a>
		<button class="btn reset" onclick={reset}>↺ Reset</button>
	{/if}

	<!-- Edit Plan — always visible when a plan is loaded -->
	<button
		class="btn edit"
		onclick={() => plan.enterEditMode()}
		disabled={plan.isEditing || !plan.committed}
		title="Edit load plan"
	>
		✏ Edit Plan
	</button>

	<!-- Logout — always far-right -->
	<form method="POST" action="/login?/logout" style="display:contents">
		<button type="submit" class="btn logout" title="Sign out">⏻</button>
	</form>
</header>

<style>
	.control-bar {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 0 16px;
		height: 46px;
		background: var(--bg1);
		border-bottom: 1px solid var(--bg3);
		flex-shrink: 0;
	}

	/* ── State badge ───────────────────── */
	.state-badge {
		font-size: 0.68rem;
		font-weight: 700;
		letter-spacing: 0.1em;
		padding: 3px 10px;
		border-radius: 12px;
		border: 1px solid transparent;
	}
	.state-badge.waiting {
		color: var(--yellow-br);
		border-color: color-mix(in srgb, var(--yellow-br) 30%, transparent);
		background: color-mix(in srgb, var(--yellow)  10%, transparent);
	}
	.state-badge.running {
		color: var(--green-br);
		border-color: color-mix(in srgb, var(--green-br) 30%, transparent);
		background: color-mix(in srgb, var(--green)   10%, transparent);
	}
	.state-badge.done {
		color: var(--blue-br);
		border-color: color-mix(in srgb, var(--blue-br) 30%, transparent);
		background: color-mix(in srgb, var(--blue)    10%, transparent);
	}

	/* ── Agent count ───────────────────── */
	.agents-info {
		display: flex;
		align-items: baseline;
		gap: 3px;
		font-size: 0.76rem;
	}
	.count { font-weight: 700; font-size: 1rem; color: var(--fg); }

	/* ── WS indicator circle ──────────────────── */
	.ws-indicator {
		width: 13px;
		height: 13px;
		border-radius: 50%;
		border: 2.5px solid var(--red-br);
		background: transparent;
		flex-shrink: 0;
		transition: background 0.25s, border-color 0.25s;
	}
	.ws-indicator.connected {
		background: var(--green-br);
		border-color: var(--green-br);
	}

	.spacer { flex: 1; }

	/* ── Action buttons ────────────────── */
	.btn {
		font-size: 0.72rem;
		font-weight: 600;
		padding: 5px 13px;
		border-radius: 4px;
		cursor: pointer;
		border: 1px solid;
		text-decoration: none;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		transition: filter 0.12s;
		background: var(--bg2);
		color: var(--fg3);
	}
	.btn:hover:not(:disabled) { filter: brightness(1.18); }
	.btn:disabled { opacity: 0.35; cursor: not-allowed; }

	.btn.start    { color: var(--green-br);  border-color: color-mix(in srgb, var(--green-br)  35%, transparent); }
	.btn.stop     { color: var(--red-br);    border-color: color-mix(in srgb, var(--red-br)    35%, transparent); }
	.btn.reset    { color: var(--fg3);       border-color: var(--bg4); }
	.btn.download { color: var(--blue-br);   border-color: color-mix(in srgb, var(--blue-br)   35%, transparent); }
	.btn.edit     { color: var(--yellow-br); border-color: color-mix(in srgb, var(--yellow-br) 35%, transparent); }
	.btn.logout   { color: var(--fg-dim);    border-color: var(--bg4); font-size: 0.85rem; padding: 5px 9px; }
	.btn.logout:hover { color: var(--red-br); border-color: color-mix(in srgb, var(--red-br) 40%, transparent); }
</style>
