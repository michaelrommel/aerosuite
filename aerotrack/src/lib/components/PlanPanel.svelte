<script lang="ts">
	import { dashboard } from '$lib/stores/dashboard.svelte';
	import { plan } from '$lib/stores/plan.svelte';
	import { formatBandwidth } from '$lib/utils/format';
	import type { PlanEntry, SliceSpec } from '$lib/types';

	// ── Plan directory dropdown ──────────────────────────────────────────

	let availablePlans    = $state<PlanEntry[]>([]);
	let selectError       = $state<string | null>(null);
	let confirming        = $state(false);
	let confirmError      = $state<string | null>(null);
	/** Filename of the last successfully confirmed plan, or null. */
	let confirmedFilename = $state<string | null>(null);

	// Fetch the plan list once when in WAITING state and a plan directory
	// is configured.  Re-fetches automatically whenever coachState changes.
	$effect(() => {
		if (!dashboard.coachState.startsWith('WAITING')) return;
		fetch('/plans')
			.then((r) => (r.ok ? r.json() : Promise.reject(r)))
			.then((data: PlanEntry[]) => { availablePlans = data; })
			.catch(() => { availablePlans = []; });
	});

	// Clear confirmed state on DONE → WAITING (reset) so a fresh
	// confirmation is required for the next run.
	let prevCoachState = $state(dashboard.coachState);
	$effect(() => {
		const cur = dashboard.coachState;
		if (prevCoachState.startsWith('DONE') && cur.startsWith('WAITING')) {
			confirmedFilename = null;
		}
		prevCoachState = cur;
	});

	const isWaiting       = $derived(dashboard.coachState.startsWith('WAITING'));
	const hasPlanDir      = $derived(availablePlans.length > 0);
	const showDropdown    = $derived(isWaiting && hasPlanDir);
	const currentFilename = $derived(
		availablePlans.find((p) => p.plan_id === plan.committed?.plan_id)?.filename ?? ''
	);
	/** True when the currently selected plan has been confirmed to all agents. */
	const isConfirmed     = $derived(
		confirmedFilename !== null && confirmedFilename === currentFilename
	);

	async function selectPlan(filename: string) {
		if (!filename) return;
		selectError  = null;
		confirmError = null;
		try {
			const res = await fetch('/plan/select', {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ filename }),
			});
			if (!res.ok) {
				const d = await res.json().catch(() => ({})) as { error?: string };
				selectError = d.error ?? `Failed to load plan (${res.status})`;
				return;
			}
			await plan.reload();
		} catch (e) {
			selectError = String(e);
		}
	}

	async function confirmPlan() {
		confirming   = true;
		confirmError = null;
		try {
			const res = await fetch('/plan/confirm', { method: 'POST' });
			if (!res.ok) {
				const d = await res.json().catch(() => ({})) as { error?: string };
				confirmError = d.error ?? `Confirm failed (${res.status})`;
			} else {
				confirmedFilename = currentFilename;
			}
		} catch (e) {
			confirmError = String(e);
		} finally {
			confirming = false;
		}
	}

	// ── SVG canvas ──────────────────────────────────────────────────────────
	const W = 600, H = 185;
	// PAD.top expands in edit mode so the ▲/▼ nudge buttons above high segments
	// stay inside the SVG viewBox and don't get clipped by the edit toolbar.
	const PAD    = { right: 14, bottom: 4, left: 44 };
	const IW     = W - PAD.left - PAD.right;
	const padTop = $derived(plan.isEditing ? 46 : 16);
	const IH     = $derived(H - padTop - PAD.bottom);

	// Render draft (edit mode) or committed plan (live mode).
	const displayPlan = $derived(plan.draft ?? plan.committed);
	const slices      = $derived(displayPlan?.slices ?? []);
	const maxConns    = $derived(Math.max(...slices.map((s) => s.total_connections), 1));
	const sliceDurMs  = $derived(displayPlan?.slice_duration_ms ?? 60_000);
	const bwMbps      = $derived(
		displayPlan ? (displayPlan.total_bandwidth_bps / 1_000_000).toFixed(0) : '0'
	);
	// Formatted values for the footer stat tiles.
	const durationLabel = $derived((() => {
		if (!displayPlan) return '—';
		const ms = displayPlan.slice_duration_ms;
		if (ms < 60_000) return `${(ms / 1_000).toFixed(0)} s`;
		const m = Math.floor(ms / 60_000);
		const s = Math.round((ms % 60_000) / 1_000);
		return s > 0 ? `${m} m ${s} s` : `${m} m`;
	})());
	const bandwidthLabel = $derived(
		!displayPlan ? '—'
		: displayPlan.total_bandwidth_bps === 0 ? 'Unlimited'
		: formatBandwidth(displayPlan.total_bandwidth_bps)
	);

	// xFor(i) = x coordinate of the LEFT EDGE of slice i.
	// xFor(slices.length) = right edge of the graph.
	// Each slice occupies an equal band of width IW / N.
	function xFor(idx: number): number {
		if (!slices.length) return PAD.left;
		return PAD.left + (idx / slices.length) * IW;
	}
	function yFor(conns: number): number {
		return padTop + IH - (conns / maxConns) * IH;
	}
	function yBase(): number { return padTop + IH; }

	// Slice-boundary label used only in the hover tooltip.
	function timeLabel(boundaryIdx: number): string {
		const ms = boundaryIdx * sliceDurMs;
		return ms < 120_000
			? `${(ms / 1_000).toFixed(0)} s`
			: `${(ms / 60_000).toFixed(0)} m`;
	}

	// Step-graph SVG path.
	// Each slice draws a horizontal segment from xFor(i) to xFor(i+1),
	// with a vertical riser connecting adjacent slice heights.
	const stepPath = $derived((() => {
		if (!slices.length) return '';
		let d = '';
		for (let i = 0; i < slices.length; i++) {
			const s  = slices[i];
			const x1 = xFor(s.slice_index);
			const x2 = xFor(s.slice_index + 1); // right edge of this slice
			const y  = yFor(s.total_connections);
			if (i === 0) {
				d += `M ${x1} ${y} L ${x2} ${y}`;
			} else {
				// Vertical riser at x1, then horizontal to x2.
				d += ` L ${x1} ${y} L ${x2} ${y}`;
			}
		}
		return d;
	})());

	const yFracs = [0, 0.25, 0.5, 0.75, 1.0];

	// Hover tooltip.
	let tooltip: { x: number; y: number; slice: SliceSpec } | null = $state(null);

	function onSliceEnter(e: MouseEvent, s: SliceSpec) {
		const wrap = (e.currentTarget as Element).closest('.svg-wrapper');
		if (wrap) {
			const r = wrap.getBoundingClientRect();
			tooltip = { x: e.clientX - r.left + 10, y: e.clientY - r.top - 50, slice: s };
		}
	}
	function clearTooltip() { tooltip = null; }

	// Edit helpers.
	function sliceConns(idx: number): number {
		return slices.find((s) => s.slice_index === idx)?.total_connections ?? 0;
	}
	function increment(idx: number) { plan.updateSlice(idx, sliceConns(idx) + 10); }
	function decrement(idx: number) { plan.updateSlice(idx, Math.max(0, sliceConns(idx) - 10)); }

	function onBwInput(e: Event) {
		const v = parseFloat((e.target as HTMLInputElement).value);
		if (!isNaN(v) && v > 0) plan.updateBandwidth(v * 1_000_000);
	}
</script>

<div class="plan-panel">
	<!-- Header row -->
	<div class="panel-header">
		<span class="title">Load Plan</span>

		{#if showDropdown}
			<!-- Plan directory dropdown (WAITING + dir configured) -->
			<select
				class="plan-select"
				value={currentFilename}
				disabled={confirming}
				onchange={(e) => selectPlan((e.target as HTMLSelectElement).value)}
			>
				{#each availablePlans as entry}
					<option value={entry.filename}>{entry.plan_id}</option>
				{/each}
			</select>
			{#if selectError}
				<span class="select-error">⚠ {selectError}</span>
			{/if}
			{#if confirmError}
				<span class="select-error">⚠ {confirmError}</span>
			{/if}
		{:else if displayPlan}
			<span class="plan-id">{displayPlan.plan_id}</span>
		{/if}

		<div class="hdr-actions">
			{#if !plan.isEditing}
				{#if hasPlanDir}
					<!-- Plan-dir mode: Confirm Plan / Confirmed status button -->
					{#if isConfirmed || isWaiting}
						<button
							class="btn {isConfirmed ? 'confirmed' : 'accent'}"
							disabled={confirming || isConfirmed || !isWaiting || !plan.committed}
							onclick={confirmPlan}
							title={isConfirmed
								? 'Plan confirmed — agents have been notified'
								: 'Push selected plan to all agents'}
						>
							{#if confirming}⏳ Confirming…
							{:else if isConfirmed}✓ Confirmed
							{:else}✓ Confirm Plan{/if}
						</button>
					{/if}
				{:else if isWaiting}
					<!-- Single-file mode: Reload only in WAITING -->
					<button class="btn" onclick={() => plan.reload()} title="Reload plan from server">
						↺ Reload
					</button>
				{/if}
				<button class="btn accent" onclick={() => plan.enterEditMode()}>
					✏ Edit Plan
				</button>
			{/if}
		</div>
	</div>

	<!-- Edit mode toolbar -->
	{#if plan.isEditing}
		<div class="edit-bar">
			<label>
				Bandwidth (Mbit/s):
				<input type="number" min="0.001" step="any" value={bwMbps} oninput={onBwInput} />
			</label>
			<div class="spacer"></div>
			{#if plan.editError}
				<span class="edit-error">⚠ {plan.editError}</span>
			{/if}
			<button class="btn" onclick={() => plan.cancelEdit()}>✕ Cancel</button>
			<button class="btn accent" onclick={() => plan.applyEdit(dashboard.currentSlice)}>
				✓ Apply
			</button>
		</div>
	{/if}

	<!-- SVG step graph -->
	{#if !displayPlan}
		<div class="empty">No plan loaded — set AEROCOACH_PLAN_FILE or PUT /plan</div>
	{:else}
		<!-- svelte-ignore a11y_no_static_element_interactions -->
		<div class="svg-wrapper" role="img" aria-label="load plan timeline" onmouseleave={clearTooltip}>
			<svg viewBox="0 0 {W} {H}" width="100%" height="100%" preserveAspectRatio="xMidYMid meet">

				<!-- Horizontal grid lines + Y labels -->
				{#each yFracs as frac}
					{@const val = Math.round(maxConns * frac)}
					{@const gy  = yFor(val)}
					<line x1={PAD.left} y1={gy} x2={PAD.left + IW} y2={gy}
						stroke="#3c3836" stroke-width="1" />
					<text x={PAD.left - 5} y={gy + 4} text-anchor="end"
						fill="#928374" font-size="9" font-family="monospace">{val}</text>
				{/each}


				<!-- Completed-slice fill -->
				{#each slices as s, i}
					{#if s.slice_index < dashboard.currentSlice && i < slices.length - 1}
						{@const x1 = xFor(s.slice_index)}
						{@const x2 = xFor(slices[i + 1].slice_index)}
						{@const y1 = yFor(s.total_connections)}
						{@const yb = yBase()}
						<polygon points="{x1},{y1} {x2},{y1} {x2},{yb} {x1},{yb}"
							fill="#458588" opacity="0.18" />
					{/if}
				{/each}

				<!-- Main step path -->
				<path d={stepPath} fill="none" stroke="#458588" stroke-width="1.8" />

				<!-- Active slice segment (brighter, thicker) -->
				{#if slices.length > 1}
					{@const cur  = slices.find((s) => s.slice_index === dashboard.currentSlice)}
					{@const nxt  = slices.find((s) => s.slice_index === dashboard.currentSlice + 1)}
					{#if cur && nxt}
						<line x1={xFor(cur.slice_index)} y1={yFor(cur.total_connections)}
							  x2={xFor(nxt.slice_index)} y2={yFor(cur.total_connections)}
							stroke="#83a598" stroke-width="3" />
					{/if}
				{/if}

				<!-- Current-slice dot: tracks time progress across the active slice band -->
				{#each slices as s}
					{#if s.slice_index === dashboard.currentSlice}
						{@const x0       = xFor(s.slice_index)}
						{@const x1       = xFor(s.slice_index + 1)}
						{@const progress = (dashboard.coachState.startsWith('RUNNING')
								&& dashboard.sliceStartMs !== null
								&& dashboard.lastUpdated  !== null)
							? Math.min(1, (dashboard.lastUpdated - dashboard.sliceStartMs) / sliceDurMs)
							: 0.5}
						<circle
							cx={x0 + progress * (x1 - x0)}
							cy={yFor(s.total_connections)}
							r="5.5"
							fill="#d79921" stroke="#151819" stroke-width="2" />
					{/if}
				{/each}

				<!-- Per-slice hit areas (full band width) + edit nudge buttons -->
				{#each slices as s}
					{@const sx   = xFor(s.slice_index)}
					{@const sw   = xFor(s.slice_index + 1) - sx}
					{@const scx  = sx + sw / 2}
					{@const sy   = yFor(s.total_connections)}
					{@const past = s.slice_index < dashboard.currentSlice}
					<!-- svelte-ignore a11y_no_static_element_interactions -->
					<rect x={sx} y={padTop} width={sw} height={IH} fill="transparent"
						onmouseenter={(e) => onSliceEnter(e, s)} onmouseleave={clearTooltip} />
					{#if plan.isEditing && !past}
						<!-- svelte-ignore a11y_click_events_have_key_events -->
						<g>
							<rect x={scx - 7} y={sy - 33} width="14" height="13" rx="2"
								fill="#282828" stroke="#504945" stroke-width="1"
								style="cursor:pointer"
								onclick={() => increment(s.slice_index)} />
							<text x={scx} y={sy - 23} text-anchor="middle"
								fill="#a89984" font-size="9" pointer-events="none">▲</text>
							<rect x={scx - 7} y={sy - 18} width="14" height="13" rx="2"
								fill="#282828" stroke="#504945" stroke-width="1"
								style="cursor:pointer"
								onclick={() => decrement(s.slice_index)} />
							<text x={scx} y={sy - 8} text-anchor="middle"
								fill="#a89984" font-size="9" pointer-events="none">▼</text>
						</g>
					{/if}
				{/each}
			</svg>

			<!-- Hover tooltip -->
			{#if tooltip}
				<div class="tooltip" style:left="{tooltip.x}px" style:top="{tooltip.y}px">
					Slice {tooltip.slice.slice_index}<br />
					{tooltip.slice.total_connections} connections<br />
					{timeLabel(tooltip.slice.slice_index)} – {timeLabel(tooltip.slice.slice_index + 1)}
				</div>
			{/if}
		</div>

		<!-- Footer: plan parameters in stat-tile style -->
		<div class="plan-footer">
			<div class="stat-tile">
				<div class="stat-label">Slice Duration</div>
				<div class="stat-value">{durationLabel}</div>
			</div>
			<div class="stat-tile">
				<div class="stat-label">Total Bandwidth</div>
				<div class="stat-value">{bandwidthLabel}</div>
			</div>
		</div>
	{/if}
</div>

<style>
	.plan-panel {
		display: flex;
		flex-direction: column;
		height: 100%;
		padding: 8px 12px 6px;
		box-sizing: border-box;
		overflow: hidden;
	}

	.panel-header {
		display: flex;
		align-items: center;
		gap: 8px;
		margin-bottom: 4px;
		flex-shrink: 0;
	}

	.title {
		font-size: 0.68rem;
		text-transform: uppercase;
		letter-spacing: 0.09em;
		color: var(--fg-dim);
	}

	.plan-id {
		flex: 1;
		font-size: 0.74rem;
		font-weight: 600;
		color: var(--blue-br);
	}

	.plan-select {
		flex: 1;
		min-width: 0;
		background: var(--bg2);
		border: 1px solid var(--bg4);
		border-radius: 4px;
		color: var(--blue-br);
		font-size: 0.74rem;
		font-weight: 600;
		padding: 2px 6px;
		cursor: pointer;
		appearance: none;
		-webkit-appearance: none;
		background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='10' height='6'%3E%3Cpath d='M0 0l5 6 5-6z' fill='%23928374'/%3E%3C/svg%3E");
		background-repeat: no-repeat;
		background-position: right 7px center;
		padding-right: 22px;
	}
	.plan-select:hover { border-color: var(--blue); }
	.plan-select:focus { outline: none; border-color: var(--blue-br); }

	.select-error {
		color: var(--red-br);
		font-size: 0.66rem;
	}

	.hdr-actions { display: flex; gap: 5px; }

	/* ── Edit bar ─────────────────────────────── */
	.edit-bar {
		display: flex;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
		background: var(--bg2);
		border: 1px solid var(--bg4);
		border-radius: 5px;
		padding: 5px 10px;
		margin-bottom: 5px;
		font-size: 0.72rem;
		color: var(--fg3);
		flex-shrink: 0;
	}

	.edit-bar label {
		display: flex;
		align-items: center;
		gap: 6px;
	}

	.edit-bar input {
		width: 80px;
		background: var(--bg);
		border: 1px solid var(--bg4);
		border-radius: 3px;
		color: var(--fg);
		padding: 2px 6px;
		font-size: 0.76rem;
		/* Remove native spinner arrows */
		-moz-appearance: textfield;
		appearance: textfield;
	}
	.edit-bar input::-webkit-inner-spin-button,
	.edit-bar input::-webkit-outer-spin-button {
		-webkit-appearance: none;
		margin: 0;
	}

	.spacer { flex: 1; }

	.edit-error {
		color: var(--red-br);
		font-size: 0.68rem;
	}

	/* ── SVG wrapper ──────────────────────────── */
	.svg-wrapper {
		flex: 1;
		min-height: 0;
		position: relative;
		overflow: hidden;
	}

	.empty {
		flex: 1;
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--fg-dim);
		font-style: italic;
		font-size: 0.76rem;
	}

	/* ── Hover tooltip ────────────────────────── */
	.tooltip {
		position: absolute;
		background: var(--bg2);
		border: 1px solid var(--bg4);
		border-radius: 4px;
		padding: 5px 9px;
		font-size: 0.68rem;
		color: var(--fg3);
		pointer-events: none;
		white-space: nowrap;
		z-index: 20;
		line-height: 1.5;
	}

	/* ── Shared mini buttons ──────────────────── */
	.btn {
		background: var(--bg2);
		border: 1px solid var(--bg4);
		border-radius: 4px;
		color: var(--fg3);
		font-size: 0.69rem;
		padding: 3px 8px;
		cursor: pointer;
		transition: background 0.12s;
	}
	.btn:hover { background: var(--bg4); }

	.btn.accent {
		background: var(--bg1);
		border-color: var(--yellow);
		color: var(--yellow-br);
	}
	.btn.accent:hover { background: var(--bg2); }

	.btn.confirmed {
		background: var(--bg1);
		border-color: var(--green-br);
		color: var(--green-br);
		cursor: default;
	}

	/* ── Footer stat tiles ───────────────────────────── */
	.plan-footer {
		display: flex;
		gap: 8px;
		padding-top: 6px;
		flex-shrink: 0;
	}

	.stat-tile {
		flex: 1;
		background: var(--bg2);
		border: 1px solid var(--bg3);
		border-radius: 5px;
		padding: 5px 10px;
		display: flex;
		flex-direction: column;
		gap: 3px;
	}

	.stat-label {
		font-size: 0.60rem;
		text-transform: uppercase;
		letter-spacing: 0.07em;
		color: var(--fg4);
	}

	.stat-value {
		font-size: 1.0rem;
		font-weight: 700;
		color: var(--fg);
		white-space: nowrap;
	}
</style>
