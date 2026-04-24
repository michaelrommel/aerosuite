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
		<div class="label">Active connections</div>
		<div class="value">{stats?.active_connections ?? '—'}</div>
	</div>

	<div class="card">
		<div class="label">Current bandwidth</div>
		<div class="value">{stats ? formatBandwidth(stats.current_bandwidth_bps) : '—'}</div>
	</div>
</div>

<style>
	.stats-panel {
		display: grid;
		/* 6 equal columns: row 1 cards span 3 each (half width),
		   row 2 cards span 2 each (third width) */
		grid-template-columns: repeat(6, 1fr);
		grid-template-rows: 1fr 1fr;
		gap: 10px;
		padding: 12px;
		height: 100%;
		box-sizing: border-box;
	}

	/* Row 1: two half-width cards */
	.card:nth-child(1) { grid-column: 1 / 4; }
	.card:nth-child(2) { grid-column: 4 / 7; }

	/* Row 2: three equal-width cards */
	.card:nth-child(3) { grid-column: 1 / 3; }
	.card:nth-child(4) { grid-column: 3 / 5; }
	.card:nth-child(5) { grid-column: 5 / 7; }

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
