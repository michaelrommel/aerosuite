<script lang="ts">
	import { dashboard } from '$lib/stores/dashboard.svelte';
	import { formatBytes, formatBandwidth } from '$lib/utils/format';
	import { errorRateColor } from '$lib/utils/colors';

	const stats    = $derived(dashboard.globalStats);
	const errColor = $derived(stats ? errorRateColor(stats.overall_error_rate) : '#b8bb26');
</script>

<div class="stats-panel">
	<div class="card">
		<div class="label">Files transferred</div>
		<div class="value">{stats?.total_success ?? '—'}</div>
	</div>

	<div class="card">
		<div class="label">Bytes transferred</div>
		<div class="value">{stats ? formatBytes(stats.total_bytes_transferred) : '—'}</div>
	</div>

	<div class="card">
		<div class="label">Error rate</div>
		<div class="value" style:color={errColor}>
			{stats ? (stats.overall_error_rate * 100).toFixed(2) + '%' : '—'}
		</div>
	</div>

	<div class="card">
		<div class="label">Current bandwidth</div>
		<div class="value">{stats ? formatBandwidth(stats.current_bandwidth_bps) : '—'}</div>
	</div>
</div>

<style>
	.stats-panel {
		display: grid;
		grid-template-columns: 1fr 1fr;
		grid-template-rows: 1fr 1fr;
		gap: 10px;
		padding: 12px;
		height: 100%;
		box-sizing: border-box;
	}

	.card {
		background: var(--bg2);
		border: 1px solid var(--bg3);
		border-radius: 6px;
		padding: 12px 14px;
		display: flex;
		flex-direction: column;
		justify-content: center;
		gap: 6px;
	}

	.label {
		font-size: 0.66rem;
		text-transform: uppercase;
		letter-spacing: 0.07em;
		color: var(--fg4);
	}

	.value {
		font-size: 1.5rem;
		font-weight: 700;
		color: var(--fg);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
</style>
