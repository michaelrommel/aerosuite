<script lang="ts">
	import type { AgentSnapshot } from '$lib/types';
	import { formatBytes } from '$lib/utils/format';
	import { errorRateColor } from '$lib/utils/colors';
	import { dashboard } from '$lib/stores/dashboard.svelte';

	interface Props { snapshot: AgentSnapshot | undefined }
	let { snapshot }: Props = $props();

	const errorRate = $derived(
		snapshot
			? snapshot.error_count / Math.max(snapshot.success_count + snapshot.error_count, 1)
			: 0
	);
	const errorPct = $derived((errorRate * 100).toFixed(2));
	const errColor = $derived(errorRateColor(errorRate));
	const pace     = $derived(snapshot ? dashboard.paceIcon(snapshot.current_slice) : '🐇');
</script>

{#if !snapshot}
	<div class="agent-box inactive" aria-label="inactive slot"></div>
{:else}
	<div class="agent-box active" class:disconnected={!snapshot.connected}>
		<div class="header">
			<span class="pace">{pace}</span>
			<span class="agent-id">{snapshot.agent_id}</span>
			<span class="slice-badge">{snapshot.current_slice} / {dashboard.totalSlices}</span>
		</div>

		<div class="meta">
			<span class="meta-line">{snapshot.private_ip || '—'}</span>
			{#if snapshot.instance_id}
				<span class="meta-line">{snapshot.instance_id.slice(-12)}</span>
			{/if}
		</div>

		<div class="divider"></div>

		<div class="totals">
			<span>📁 {snapshot.success_count}</span>
			<span>▲ {formatBytes(snapshot.bytes_transferred)}</span>
		</div>

		<div class="prominent">
			<span class="running">{snapshot.active_connections}</span>
			<span class="err-rate" style:color={errColor}>{errorPct}%</span>
		</div>
	</div>
{/if}

<style>
	.agent-box {
		border-radius: 5px;
		padding: 6px 8px;
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
		overflow: hidden;
		font-size: 0.68rem;
	}

	.inactive {
		background: var(--bg1);
		opacity: 0.25;
		min-height: 80px;
	}

	.active {
		background: var(--bg2);
		border: 1px solid var(--bg3);
		color: var(--fg3);
	}

	.active.disconnected {
		opacity: 0.45;
		border-color: var(--bg3);
	}

	.header {
		display: flex;
		align-items: center;
		gap: 4px;
	}

	.pace    { font-size: 0.72rem; }
	.agent-id {
		flex: 1;
		font-weight: 700;
		font-size: 0.72rem;
		color: var(--blue-br);
	}
	.slice-badge {
		font-size: 0.6rem;
		color: var(--fg-dim);
		white-space: nowrap;
	}

	.meta { display: flex; flex-direction: column; gap: 1px; }
	.meta-line {
		color: var(--fg-dim);
		font-size: 0.6rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.divider { border-top: 1px solid var(--bg3); margin: 2px 0; }

	.totals {
		display: flex;
		justify-content: space-between;
		color: var(--fg4);
		font-size: 0.64rem;
	}

	.prominent {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
	}

	/* large bold "running transfers" count */
	.running {
		font-size: 1.05rem;
		font-weight: 800;
		color: var(--fg);
	}

	/* error rate — colour comes from errorRateColor() via inline style */
	.err-rate {
		font-size: 0.88rem;
		font-weight: 700;
	}
</style>
