<script lang="ts">
	import PlanPanel   from './PlanPanel.svelte';
	import GlobalStats from './GlobalStats.svelte';
	import AgentGrid   from './AgentGrid.svelte';
</script>

<div class="layout">
	<!-- ── Top band (~33 vh): plan timeline + global stats ── -->
	<div class="top-band">
		<div class="plan-area">
			<PlanPanel />
		</div>
		<div class="v-divider"></div>
		<div class="stats-area">
			<GlobalStats />
		</div>
	</div>

	<!-- ── Horizontal divider ── -->
	<div class="h-divider"></div>

	<!-- ── Bottom band (~67 vh): agent grid ── -->
	<div class="bottom-band">
		<AgentGrid />
	</div>
</div>

<style>
	.layout {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-height: 0;
		overflow: hidden;
	}

	/* Top third */
	.top-band {
		display: flex;
		flex: 0 0 max(33vh, 350px);
		min-height: 0;
		overflow: hidden;
	}

	.plan-area {
		flex: 1 1 60%;
		min-width: 0;
		overflow: hidden;
	}

	.v-divider {
		width: 1px;
		background: var(--bg3);
		flex-shrink: 0;
	}

	.stats-area {
		flex: 1 1 40%;
		min-width: 0;
		overflow: hidden;
	}

	/* Middle divider line */
	.h-divider {
		height: 1px;
		background: var(--bg3);
		flex-shrink: 0;
	}

	/* Bottom two-thirds */
	.bottom-band {
		flex: 1;
		min-height: 0;
		overflow: hidden;
	}
</style>
