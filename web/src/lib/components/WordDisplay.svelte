<script lang="ts">
	let { pattern = [] }: { pattern?: string[] } = $props();
</script>

<div class="word-display">
	{#each pattern as char, i}
		<span class="letter-slot" class:revealed={char !== '_'}>
			<span class="letter-char">{char === '_' ? '\u00A0' : char}</span>
			<span class="letter-line"></span>
		</span>
	{/each}
</div>

<style>
	.word-display {
		display: flex;
		gap: 0.5rem;
		justify-content: center;
		flex-wrap: wrap;
		padding: 1rem;
	}

	.letter-slot {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 0.25rem;
		min-width: 2.5rem;
	}

	.letter-char {
		font-family: var(--font-mono, 'Courier New', monospace);
		font-size: 2.5rem;
		font-weight: bold;
		color: var(--bone, #e8dcc8);
		height: 3rem;
		display: flex;
		align-items: flex-end;
		transition: color 0.3s ease, transform 0.3s ease;
	}

	.revealed .letter-char {
		color: var(--purple-glow, #9b6dd7);
		animation: reveal 0.4s ease-out;
	}

	.letter-line {
		width: 100%;
		height: 3px;
		background: var(--bone-dim, #8a7f6f);
		border-radius: 1px;
	}

	.revealed .letter-line {
		background: var(--purple-glow, #9b6dd7);
	}

	@keyframes reveal {
		0% { transform: scale(1.4); opacity: 0.5; }
		100% { transform: scale(1); opacity: 1; }
	}

	@media (max-width: 600px) {
		.letter-slot { min-width: 1.8rem; }
		.letter-char { font-size: 1.8rem; height: 2.2rem; }
	}
</style>
