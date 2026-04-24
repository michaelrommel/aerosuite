<!--
	Sparkline — a compact bar-chart history strip.

	- Renders up to MAX_BARS bars, newest on the right.
	- When fewer entries exist the left side is empty (right-fill pattern).
	- Bar heights are normalised to the maximum value in the visible window;
	  a minimum pixel height ensures zero-valued bars are still visible as a
	  thin line.
	- Hovering a bar shows a tooltip with the formatted value.
-->
<script lang="ts">
	const MAX_BARS = 30;

	interface Props {
		/** Raw numeric history, newest last.  Sliced to MAX_BARS if longer. */
		history: number[];
		/** Format a raw value for the hover tooltip. */
		formatValue?: (v: number) => string;
		/** CSS color for the bars (any valid CSS colour expression). */
		color?: string;
	}

	let {
		history    = [],
		formatValue = (v: number) => v.toString(),
		color      = 'var(--blue)'
	}: Props = $props();

	/** Maximum value in the visible window; at least 1 to avoid divide-by-zero. */
	const maxVal = $derived(history.length > 0 ? Math.max(1, ...history) : 1);

	/**
	 * Padded array of MAX_BARS slots.
	 * Leading slots are `null` (rendered as empty space) so that data always
	 * appears on the right-hand side of the chart.
	 */
	const padded = $derived(
		history.length >= MAX_BARS
			? history.slice(-MAX_BARS)
			: [...Array<null>(MAX_BARS - history.length).fill(null), ...history]
	);

	/** Bar height as a CSS percentage string, with a 3 % floor for visibility. */
	function barPct(value: number): string {
		return `${Math.max(3, Math.round((value / maxVal) * 100))}%`;
	}
</script>

<div class="sparkline" aria-hidden="true">
	{#each padded as value}
		<div class="slot">
			{#if value !== null}
				<div class="bar" style:height={barPct(value)} style:background={color}>
					<span class="tip">{formatValue(value)}</span>
				</div>
			{/if}
		</div>
	{/each}
</div>

<style>
	.sparkline {
		display: flex;
		align-items: flex-end;
		gap: 2px;
		height: 100%;
		width: 100%;
		overflow: visible;
		flex-shrink: 0;
	}

	.slot {
		flex: 1;
		height: 100%;
		display: flex;
		align-items: flex-end;
		/* Tooltip positioned relative to this slot. */
		position: relative;
	}

	.bar {
		width: 100%;
		min-height: 2px;
		border-radius: 1px 1px 0 0;
		position: relative;
		cursor: default;
		transition: filter 0.08s;
	}

	.bar:hover {
		filter: brightness(1.3);
	}

	/* ── Tooltip ─────────────────────────────────────────────────────── */

	.tip {
		visibility: hidden;
		position: absolute;
		/* Appear above the bar with a small gap. */
		bottom: calc(100% + 4px);
		left: 50%;
		transform: translateX(-50%);
		background: var(--bg1);
		color: var(--fg);
		border: 1px solid var(--bg4);
		border-radius: 3px;
		padding: 2px 6px;
		font-size: 0.6rem;
		font-weight: 600;
		white-space: nowrap;
		pointer-events: none;
		z-index: 200;
	}

	.bar:hover .tip {
		visibility: visible;
	}
</style>
