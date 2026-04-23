<script lang="ts">
	import { onMount } from 'svelte';
	import DashboardLayout from '$lib/components/DashboardLayout.svelte';
	import ControlBar      from '$lib/components/ControlBar.svelte';
	import Legend          from '$lib/components/Legend.svelte';
	import { dashboard }   from '$lib/stores/dashboard.svelte';
	import { plan }        from '$lib/stores/plan.svelte';

	onMount(() => {
		// Start the WebSocket connection and fetch the plan once hydrated.
		dashboard.startWs();
		plan.reload();
	});
</script>

<svelte:head>
	<title>aerotrack — load test dashboard</title>
</svelte:head>

<div class="app">
	<ControlBar />
	<DashboardLayout />
	<Legend />
</div>

<style>
	.app {
		display: flex;
		flex-direction: column;
		width: 100vw;
		height: 100vh;
		overflow: hidden;
		background: var(--bg);
		color: var(--fg);
	}
</style>
