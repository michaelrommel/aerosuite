/**
 * Reactive plan store (Svelte 5 runes).
 *
 * Manages the committed plan (last successfully fetched/applied) and the
 * in-progress draft used by PlanPanel's edit mode.
 */

import type { LoadPlan, SliceSpec } from '$lib/types';

class PlanStore {
	/** Last successfully fetched or applied plan. */
	committed = $state<LoadPlan | null>(null);

	/** Working copy during edit mode; null when not editing. */
	draft = $state<LoadPlan | null>(null);

	/** Error message from the most recent failed `applyEdit` call. */
	editError = $state<string | null>(null);

	/** True while a `reload()` fetch is in-flight. */
	loading = $state(false);

	get isEditing(): boolean {
		return this.draft !== null;
	}

	/** Fetch the current plan from `GET /plan` and update `committed`. */
	async reload(): Promise<void> {
		this.loading = true;
		try {
			const res = await fetch('/plan');
			if (!res.ok) throw new Error(`GET /plan returned ${res.status}`);
			this.committed = (await res.json()) as LoadPlan;
		} catch (e) {
			console.error('[PlanStore] reload failed:', e);
		} finally {
			this.loading = false;
		}
	}

	/** Enter edit mode: clone committed into draft. */
	enterEditMode(): void {
		if (!this.committed) return;
		// $state.snapshot() strips the Svelte reactive Proxy before cloning;
		// structuredClone throws DataCloneError on Proxy objects directly.
		this.draft     = structuredClone($state.snapshot(this.committed));
		this.editError = null;
	}

	/** Discard the draft without sending any request. */
	cancelEdit(): void {
		this.draft     = null;
		this.editError = null;
	}

	/** Update a single slice's connection count in the draft. */
	updateSlice(sliceIndex: number, totalConnections: number): void {
		if (!this.draft) return;
		const s = this.draft.slices.find((s) => s.slice_index === sliceIndex);
		if (s) s.total_connections = Math.max(0, totalConnections);
		// Re-assign to trigger Svelte reactivity.
		this.draft = { ...this.draft, slices: [...this.draft.slices] };
	}

	/** Update the bandwidth ceiling in the draft. */
	updateBandwidth(bps: number): void {
		if (!this.draft) return;
		this.draft = { ...this.draft, total_bandwidth_bps: Math.max(1, bps) };
	}

	/**
	 * Send `PATCH /plan` with the draft changes from `currentSlice` onwards.
	 * On success, commits the draft and exits edit mode.
	 * On failure, sets `editError` and stays in edit mode.
	 */
	async applyEdit(currentSlice: number): Promise<void> {
		if (!this.draft) return;
		this.editError = null;

		const updatedSlices: SliceSpec[] = this.draft.slices.filter(
			(s) => s.slice_index >= currentSlice
		);

		try {
			const res = await fetch('/plan', {
				method: 'PATCH',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({
					effective_from_slice: currentSlice,
					updated_slices:       updatedSlices,
					new_bandwidth_bps:    this.draft.total_bandwidth_bps,
				}),
			});

			if (!res.ok) {
				const body = await res.json().catch(() => ({ error: `HTTP ${res.status}` }));
				throw new Error((body as { error?: string }).error ?? `HTTP ${res.status}`);
			}

			this.committed = structuredClone($state.snapshot(this.draft));
			this.draft     = null;
		} catch (e) {
			this.editError = e instanceof Error ? e.message : String(e);
		}
	}
}

export const plan = new PlanStore();
