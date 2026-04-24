<script lang="ts">
	import { dashboard } from '$lib/stores/dashboard.svelte';
	import { formatBytes, formatBandwidth } from '$lib/utils/format';
	import { errorRateColor } from '$lib/utils/colors';
	import Sparkline from './Sparkline.svelte';

	const stats    = $derived(dashboard.globalStats);
	const errColor = $derived(stats ? errorRateColor(stats.overall_error_rate) : '#b8bb26');
</script>

<div class="stats-panel">

	<!-- ── Row 1, left: Active connections + sparkline ──────────────── -->
	<div class="card card-sparkline">
		<div class="text-group">
			<div class="label">Active connections</div>
			<div class="value">{stats?.active_connections ?? '—'}</div>
		</div>
		<Sparkline
			history={dashboard.connectionsHistory}
			formatValue={(v) => String(v)}
			color="var(--cyan)"
		/>
	</div>

	<!-- ── Row 1, right: Current bandwidth + sparkline ──────────────── -->
	<div class="card card-sparkline">
		<div class="text-group">
			<div class="label">Current bandwidth</div>
			<div class="value">{stats ? formatBandwidth(stats.current_bandwidth_bps) : '—'}</div>
		</div>
		<Sparkline
			history={dashboard.bandwidthHistory}
			formatValue={(v) => formatBandwidth(v)}
			color="var(--blue)"
		/>
	</div>

	<!-- ── Row 2, left: Error rate ──────────────────────────────────── -->
	<div class="card">
		<div class="label">Error rate</div>
		<div class="value" style:color={errColor}>
			{stats ? (stats.overall_error_rate * 100).toFixed(2) + '%' : '—'}
		</div>
	</div>

	<!-- ── Row 2, centre: Files transferred ─────────────────────────── -->
	<div class="card">
		<div class="label">Files transferred</div>
		<div class="value">{stats?.total_success ?? '—'}</div>
	</div>

	<!-- ── Row 2, right: Bytes transferred ──────────────────────────── -->
	<div class="card">
		<div class="label">Bytes transferred</div>
		<div class="value">{stats ? formatBytes(stats.total_bytes_transferred) : '—'}</div>
	</div>

</div>

<style>
	.stats-panel {
		display: grid;
		/*
		 * 6 equal columns.
		 * Row 1 (sparkline cards) gets more height than row 2.
		 * The ratio 3:2 works well at 33 vh: ~138 px / ~92 px.
		 */
		grid-template-columns: repeat(6, 1fr);
		grid-template-rows: 3fr 2fr;
		gap: 10px;
		padding: 12px;
		height: 100%;
		box-sizing: border-box;
	}

	/* Row 1: two half-width cards (each spans 3 columns) */
	.card:nth-child(1) { grid-column: 1 / 4; }
	.card:nth-child(2) { grid-column: 4 / 7; }

	/* Row 2: three equal-width cards (each spans 2 columns) */
	.card:nth-child(3) { grid-column: 1 / 3; }
	.card:nth-child(4) { grid-column: 3 / 5; }
	.card:nth-child(5) { grid-column: 5 / 7; }

	/* ── Base card ───────────────────────────────────────────────────── */
	.card {
		background: var(--bg2);
		border: 1px solid var(--bg3);
		border-radius: 6px;
		padding: 10px 14px;
		display: flex;
		flex-direction: column;
		/*
		 * Bottom-row cards: centre label + value vertically in the card,
		 * matching the original design.
		 */
		justify-content: center;
		gap: 5px;
		min-height: 0;
		overflow: visible;
	}

	/* ── Top-row cards carry a sparkline ─────────────────────────────── */
	.card-sparkline {
		/*
		 * Push the text group to the top and the sparkline to the bottom,
		 * with all leftover space between them.
		 */
		justify-content: space-between;
		overflow: clip;
	}

	/* ── Text group (label + value) inside a sparkline card ──────────── */
	.text-group {
		display: flex;
		flex-direction: column;
		gap: 5px;
	}

	/* ── Typography ──────────────────────────────────────────────────── */
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
