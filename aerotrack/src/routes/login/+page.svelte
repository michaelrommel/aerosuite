<script lang="ts">
	import type { ActionData } from './$types';

	interface Props { form: ActionData }
	let { form }: Props = $props();

	let submitting = $state(false);
	let usernameEl = $state<HTMLInputElement | null>(null);

	$effect(() => { usernameEl?.focus(); });
</script>

<svelte:head>
	<title>aerotrack — sign in</title>
</svelte:head>

<div class="page">
	<div class="card">
		<!-- Wordmark -->
		<div class="wordmark">
			<span class="logo-a">aero</span><span class="logo-b">track</span>
		</div>
		<p class="subtitle">Load test dashboard</p>

		<!-- Error banner -->
		{#if form?.error}
			<div class="error-banner" role="alert">{form.error}</div>
		{/if}

		<!-- Login form -->
		<form
			method="POST"
			action="?/login"
			onsubmit={() => { submitting = true; }}
		>
			<label class="field">
				<span class="field-label">Username</span>
				<input
					bind:this={usernameEl}
					type="text"
					name="username"
					autocomplete="username"
					autocapitalize="none"
					required
				/>
			</label>

			<label class="field">
				<span class="field-label">Password</span>
				<input
					type="password"
					name="password"
					autocomplete="current-password"
					required
				/>
			</label>

			<button type="submit" class="submit-btn" disabled={submitting}>
				{submitting ? 'Signing in…' : 'Sign in'}
			</button>
		</form>
	</div>
</div>

<style>
	/* ── Full-screen centred layout ───────────────────────────────────────── */
	.page {
		min-height: 100vh;
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--bg);
		padding: 24px;
	}

	/* ── Card ─────────────────────────────────────────────────────────────── */
	.card {
		width: 100%;
		max-width: 360px;
		background: var(--bg2);
		border: 1px solid var(--bg4);
		border-radius: 8px;
		padding: 36px 32px 32px;
		display: flex;
		flex-direction: column;
		gap: 20px;
	}

	/* ── Wordmark ─────────────────────────────────────────────────────────── */
	.wordmark {
		font-size: 1.7rem;
		font-weight: 800;
		letter-spacing: -0.02em;
		text-align: center;
	}
	.logo-a { color: var(--fg); }
	.logo-b { color: var(--yellow-br); }

	.subtitle {
		text-align: center;
		font-size: 0.75rem;
		color: var(--fg-dim);
		margin-top: -14px; /* tighten gap after wordmark */
		letter-spacing: 0.04em;
	}

	/* ── Error banner ─────────────────────────────────────────────────────── */
	.error-banner {
		background: color-mix(in srgb, var(--red) 18%, transparent);
		border: 1px solid color-mix(in srgb, var(--red-br) 45%, transparent);
		border-radius: 5px;
		color: var(--red-br);
		font-size: 0.78rem;
		padding: 8px 12px;
		text-align: center;
	}

	/* ── Form fields ──────────────────────────────────────────────────────── */
	form {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}

	.field {
		display: flex;
		flex-direction: column;
		gap: 5px;
	}

	.field-label {
		font-size: 0.7rem;
		text-transform: uppercase;
		letter-spacing: 0.07em;
		color: var(--fg4);
	}

	.field input {
		background: var(--bg);
		border: 1px solid var(--bg4);
		border-radius: 4px;
		color: var(--fg);
		font-size: 0.92rem;
		padding: 8px 10px;
		outline: none;
		transition: border-color 0.15s;
		width: 100%;
	}

	.field input:focus {
		border-color: var(--yellow);
	}

	/* ── Submit button ────────────────────────────────────────────────────── */
	.submit-btn {
		margin-top: 4px;
		background: var(--yellow);
		color: var(--bg);
		border: none;
		border-radius: 4px;
		font-size: 0.9rem;
		font-weight: 700;
		padding: 10px;
		cursor: pointer;
		transition: filter 0.12s;
		letter-spacing: 0.03em;
	}

	.submit-btn:hover:not(:disabled) { filter: brightness(1.12); }
	.submit-btn:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
