<div class="legend">
	<span class="legend-title">Error rate:</span>

	<span class="swatch" style:background="#b8bb26"></span>
	<span class="lab">0 %</span>

	<span class="swatch" style:background="#fabd2f"></span>
	<span class="lab">&lt; 1 %</span>

	<span class="swatch" style:background="#fe8019"></span>
	<span class="lab">1 – 3 %</span>

	<span class="swatch" style:background="#fb4934"></span>
	<span class="lab">&gt; 3 %</span>
</div>

<style>
	.legend {
		display: flex;
		align-items: center;
		gap: 7px;
		padding: 4px 16px;
		background: var(--bg1);
		border-top: 1px solid var(--bg3);
		font-size: 0.65rem;
		color: var(--fg-dim);
		flex-shrink: 0;
	}

	.legend-title {
		font-weight: 600;
		margin-right: 3px;
		color: var(--fg4);
	}

	.swatch {
		display: inline-block;
		width: 9px;
		height: 9px;
		border-radius: 2px;
	}

	.lab { color: var(--fg3); }
</style>
