<script lang="ts">
	import AgentBox from './AgentBox.svelte';
	import { dashboard } from '$lib/stores/dashboard.svelte';

	const slots = Array.from({ length: 100 }, (_, i) => i);
</script>

<div class="grid-wrapper">
	<div class="agent-grid">
		{#each slots as idx (idx)}
			<AgentBox snapshot={dashboard.agents.get(idx)} />
		{/each}
	</div>
</div>

<style>
	.grid-wrapper {
		width: 100%;
		height: 100%;
		overflow-y: auto;
		padding: 10px;
		box-sizing: border-box;
	}

	.agent-grid {
		display: grid;
		grid-template-columns: repeat(10, 1fr);
		gap: 5px;
	}
</style>
