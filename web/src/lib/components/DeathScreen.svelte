<script lang="ts">
	let {
		exampleWord = '',
		totalGuessed = 0,
		wrongCount = 0,
		minimaxValue = null as number | null,
		won = false,
		onplayagain,
	}: {
		exampleWord?: string;
		totalGuessed?: number;
		wrongCount?: number;
		minimaxValue?: number | null;
		won?: boolean;
		onplayagain?: () => void;
	} = $props();

	let visible = $state(false);

	$effect(() => {
		// Delay appearance for death animation
		const timer = setTimeout(() => { visible = true; }, won ? 500 : 2000);
		return () => clearTimeout(timer);
	});
</script>

<div class="overlay" class:visible>
	<div class="death-card" class:won>
		{#if won}
			<h2 class="death-title win-title">YOU SURVIVED</h2>
			<p class="death-subtitle">Against all odds.</p>
		{:else}
			<h2 class="death-title">YOU WERE HANGED</h2>
			<p class="death-subtitle">As was foretold.</p>
		{/if}

		<div class="stats">
			{#if exampleWord}
				<div class="stat-row example-word">
					<span class="stat-label">The word was:</span>
					<span class="stat-value word">{exampleWord.toUpperCase()}</span>
				</div>
			{/if}

			<div class="stat-row">
				<span class="stat-label">Letters guessed</span>
				<span class="stat-value">{totalGuessed}</span>
			</div>

			<div class="stat-row">
				<span class="stat-label">Wrong guesses</span>
				<span class="stat-value wrong">{wrongCount}</span>
			</div>

			{#if minimaxValue != null}
				<div class="minimax-note">
					<p>
						No strategy can beat this game with fewer than
						<strong>{minimaxValue}</strong>
						{minimaxValue === 1 ? 'miss' : 'misses'}.
					</p>
				</div>
			{/if}
		</div>

		<button class="play-again-btn" onclick={onplayagain}>
			{won ? 'PLAY AGAIN' : 'TRY AGAIN'}
		</button>

		<a href="/about" class="about-link">How does this work?</a>
	</div>
</div>

<style>
	.overlay {
		position: fixed;
		inset: 0;
		z-index: 100;
		display: flex;
		align-items: center;
		justify-content: center;
		background: rgba(0, 0, 0, 0);
		backdrop-filter: blur(0px);
		pointer-events: none;
		transition: all 0.8s ease;
	}

	.overlay.visible {
		background: rgba(0, 0, 0, 0.75);
		backdrop-filter: blur(4px);
		pointer-events: auto;
	}

	.death-card {
		background: var(--bg-card, #13131f);
		border: 1px solid var(--blood, #8b2233);
		border-radius: 8px;
		padding: 2.5rem;
		max-width: 420px;
		width: 90%;
		text-align: center;
		opacity: 0;
		transform: translateY(20px);
		transition: all 0.6s ease 0.2s;
	}

	.death-card.won {
		border-color: var(--success, #4a8b5c);
	}

	.overlay.visible .death-card {
		opacity: 1;
		transform: translateY(0);
	}

	.death-title {
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 2.2rem;
		font-weight: 400;
		color: var(--blood-bright, #c0392b);
		letter-spacing: 0.08em;
		margin-bottom: 0.25rem;
	}

	.win-title {
		color: var(--success, #4a8b5c);
	}

	.death-subtitle {
		font-style: italic;
		color: var(--text-dim, #6b6575);
		font-size: 1rem;
		margin-bottom: 1.5rem;
	}

	.stats {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
		margin-bottom: 2rem;
	}

	.stat-row {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		padding: 0.25rem 0;
		border-bottom: 1px solid var(--text-ghost, #3d3647);
	}

	.example-word {
		flex-direction: column;
		align-items: center;
		gap: 0.25rem;
		border-bottom: none;
		margin-bottom: 0.5rem;
	}

	.stat-label {
		color: var(--text-dim, #6b6575);
		font-size: 0.95rem;
	}

	.stat-value {
		color: var(--bone, #e8dcc8);
		font-family: var(--font-mono, 'Courier New', monospace);
		font-size: 1.1rem;
	}

	.stat-value.word {
		font-size: 1.8rem;
		color: var(--purple-glow, #9b6dd7);
		letter-spacing: 0.15em;
	}

	.stat-value.wrong {
		color: var(--blood-bright, #c0392b);
	}

	.minimax-note {
		background: var(--bg-panel, #1a1a2e);
		border-radius: 4px;
		padding: 0.75rem;
		margin-top: 0.5rem;
	}

	.minimax-note p {
		font-style: italic;
		color: var(--text-dim, #6b6575);
		font-size: 0.9rem;
		line-height: 1.4;
	}

	.minimax-note strong {
		color: var(--purple-glow, #9b6dd7);
	}

	.play-again-btn {
		padding: 0.8rem 3rem;
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 1.3rem;
		letter-spacing: 0.1em;
		color: var(--bone, #e8dcc8);
		background: var(--accent-blue, #4a7fb5);
		border-radius: 4px;
		transition: all 0.2s ease;
	}

	.play-again-btn:hover {
		background: var(--accent-blue-glow, #5b9fd4);
		transform: scale(1.05);
	}

	.about-link {
		display: inline-block;
		margin-top: 1rem;
		font-family: var(--font-body, Georgia, serif);
		font-size: 0.85rem;
		color: var(--text-ghost, #3d3647);
		text-decoration: none;
		transition: color 0.2s;
	}

	.about-link:hover {
		color: var(--purple-glow, #9b6dd7);
	}
</style>
