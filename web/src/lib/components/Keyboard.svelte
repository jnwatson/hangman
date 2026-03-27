<script lang="ts">
	let {
		guessedLetters = new Set<string>(),
		wrongLetters = [] as string[],
		disabled = false,
		onguess,
	}: {
		guessedLetters?: Set<string>;
		wrongLetters?: string[];
		disabled?: boolean;
		onguess?: (letter: string) => void;
	} = $props();

	const rows = [
		'qwertyuiop'.split(''),
		'asdfghjkl'.split(''),
		'zxcvbnm'.split(''),
	];

	const wrongSet = $derived(new Set(wrongLetters.map(l => l.toLowerCase())));

	function handleKey(letter: string) {
		if (!disabled && !guessedLetters.has(letter)) {
			onguess?.(letter);
		}
	}

	function handleKeydown(e: KeyboardEvent) {
		if (disabled) return;
		// Don't capture keystrokes when an input/textarea is focused
		const tag = (e.target as HTMLElement)?.tagName;
		if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
		const key = e.key.toLowerCase();
		if (/^[a-z]$/.test(key) && !guessedLetters.has(key)) {
			e.preventDefault();
			onguess?.(key);
		}
	}
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="keyboard">
	{#each rows as row}
		<div class="keyboard-row">
			{#each row as letter}
				{@const used = guessedLetters.has(letter)}
				{@const wrong = wrongSet.has(letter)}
				<button
					class="key"
					class:used
					class:wrong
					class:correct={used && !wrong}
					disabled={used || disabled}
					onclick={() => handleKey(letter)}
				>
					{letter.toUpperCase()}
				</button>
			{/each}
		</div>
	{/each}
</div>

<style>
	.keyboard {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		align-items: center;
		padding: 1rem 0;
	}

	.keyboard-row {
		display: flex;
		gap: 0.3rem;
	}

	.key {
		width: 2.5rem;
		height: 3rem;
		font-family: var(--font-mono, 'Courier New', monospace);
		font-size: 1rem;
		font-weight: bold;
		color: var(--bone, #e8dcc8);
		background: var(--bg-panel, #1a1a2e);
		border: 1px solid var(--text-ghost, #3d3647);
		border-radius: 4px;
		transition: all 0.15s ease;
	}

	.key:hover:not(:disabled) {
		background: var(--purple-deep, #2d1b4e);
		border-color: var(--purple-mid, #6b3fa0);
		transform: translateY(-1px);
	}

	.key:active:not(:disabled) {
		transform: translateY(0);
	}

	.key.wrong {
		background: var(--blood, #8b2233);
		border-color: var(--blood, #8b2233);
		color: var(--bone-dim, #8a7f6f);
		opacity: 0.6;
	}

	.key.correct {
		background: var(--purple-mid, #6b3fa0);
		border-color: var(--purple-glow, #9b6dd7);
		color: var(--bone, #e8dcc8);
		opacity: 0.6;
	}

	.key:disabled {
		cursor: not-allowed;
	}

	@media (max-width: 600px) {
		.key {
			width: 2rem;
			height: 2.5rem;
			font-size: 0.85rem;
		}
	}
</style>
