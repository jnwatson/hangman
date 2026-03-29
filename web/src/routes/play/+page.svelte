<script lang="ts">
	import Gallows from '$lib/components/Gallows.svelte';
	import WordDisplay from '$lib/components/WordDisplay.svelte';
	import Keyboard from '$lib/components/Keyboard.svelte';
	import WordList from '$lib/components/WordList.svelte';
	import DeathScreen from '$lib/components/DeathScreen.svelte';
	import { createGameState } from '$lib/stores/game.svelte';
	import { loadDictionary, wordsByLength } from '$lib/dictionary';
	import { page } from '$app/state';

	const game = createGameState();

	let allWords = $state<string[]>([]);
	let wordList = $derived(
		game.state ? wordsByLength(allWords, game.state.wordLength) : []
	);

	let gamesPlayed = $state(0);
	let toolsOpen = $state(false);

	let wrongCount = $derived(game.state?.wrongLetters.length ?? 0);
	let totalGuessed = $derived(game.state?.guessedLetters.size ?? 0);
	let maxWrong = $derived((game.state?.guessesAllowed ?? 6) + 1);
	let hangmanStage = $derived(Math.min(Math.round(wrongCount * 7 / maxWrong), 7));

	function getLength(): number {
		const n = page.url.searchParams.get('n');
		if (n) {
			const parsed = parseInt(n, 10);
			if (parsed >= 2 && parsed <= 28) return parsed;
		}
		return Math.floor(Math.random() * 12) + 3; // 3–14
	}

	// Load dictionary and start game on mount
	$effect(() => {
		if (!game.started && !game.loading) {
			loadDictionary().then(words => {
				allWords = words;
			});
			game.newGame(getLength());
		}
	});

	function handleGuess(letter: string) {
		game.guess(letter);
	}

	function playAgain() {
		gamesPlayed++;
		game.newGame(getLength());
	}
</script>

{#if game.state}
	<!-- Tools drawer -->
	<aside class="tools-drawer" class:open={toolsOpen}>
		<button class="tools-close" onclick={() => toolsOpen = false}>&times;</button>

		<div class="tool-section">
			<h3 class="tool-title">Best Move</h3>
			{#if game.state.gameOver}
				<span class="tool-dim">Game over</span>
			{:else if game.hintLoading}
				<div class="hint-loading">
					<span class="thinking-dot"></span>
					<span class="thinking-dot"></span>
					<span class="thinking-dot"></span>
				</div>
			{:else if game.hint}
				<div class="hint-result">
					<span class="hint-letter">{game.hint.letter}</span>
					{#if game.hint.value != null}
						<span class="hint-value">worst case: {game.hint.value} more misses</span>
					{/if}
				</div>
				{#if game.hintFailed}
					<span class="hint-caveat">approximate — some positions too complex</span>
				{/if}
			{:else if game.hintBusy}
				<span class="tool-dim">Server is busy — try again in a moment</span>
				<button class="hint-btn hint-retry" onclick={() => game.fetchHint()}>Retry</button>
			{:else if game.hintFailed}
				<span class="tool-dim">Position too complex to analyze</span>
			{:else}
				<button class="hint-btn" onclick={() => game.fetchHint()}>Reveal</button>
			{/if}
		</div>

		<WordList
			words={wordList}
			validWordsBitvec={game.state.validWordsBitvec}
			showCheat={gamesPlayed >= 1}
		/>
	</aside>
	{#if toolsOpen}
		<button class="tools-backdrop" onclick={() => toolsOpen = false}></button>
	{/if}

	<!-- GAME BOARD -->
	<div class="game-layout">
		<!-- Center: Game Area -->
		<main class="game-center">
			<div class="game-header">
				<button class="hints-toggle" onclick={() => toolsOpen = !toolsOpen}>
					<span class="hints-chevron" class:open={toolsOpen}>&#x25B8;</span> Hints
				</button>
				<h1 class="game-title">
					<span class="title-dead">Dead</span> <span class="title-letters">Letters</span>
				</h1>
				<div class="guesses-left" class:danger={game.state.guessesLeft <= 2}>
					<span class="guesses-label">Misses left</span>
					<span class="guesses-number">{game.state.guessesLeft < 0 ? 'DEAD' : game.state.guessesLeft}</span>
				</div>
			</div>

			<WordDisplay pattern={game.state.pattern} />

			<span class="word-length-label">{game.state.wordLength} letters</span>

			{#if game.state.wrongLetters.length > 0}
				<div class="wrong-letters">
					<span class="wrong-label">Wrong:</span>
					{#each game.state.wrongLetters as letter}
						<span class="wrong-letter">{letter}</span>
					{/each}
				</div>
			{/if}

			{#if game.loading}
				<div class="thinking">
					<span class="thinking-dot"></span>
					<span class="thinking-dot"></span>
					<span class="thinking-dot"></span>
				</div>
			{:else if game.state.solveStatus === 'degraded'}
				<div class="solve-status degraded">Degraded — some positions uncached</div>
			{:else if game.state.solveStatus === 'unresolved'}
				<div class="solve-status unresolved">Unresolved — referee is guessing</div>
			{/if}

			<Keyboard
				guessedLetters={game.state.guessedLetters}
				wrongLetters={game.state.wrongLetters}
				disabled={game.state.gameOver || game.loading}
				onguess={handleGuess}
			/>

			{#if game.error}
				<p class="error-msg">{game.error}</p>
			{/if}
		</main>

		<!-- Right: Hangman -->
		<aside class="panel-right">
			<div class="gallows-container">
				<Gallows stage={hangmanStage} size={220} />
			</div>
		</aside>
	</div>

	{#if game.state.gameOver}
		<DeathScreen
			exampleWord={game.state.exampleWord ?? ''}
			totalGuessed={totalGuessed}
			wrongCount={wrongCount}
			minimaxValue={game.state.minimaxValue}
			won={game.state.won}
			onplayagain={playAgain}
		/>
	{/if}
{/if}

<style>
	.hints-toggle {
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 1rem;
		letter-spacing: 0.05em;
		color: var(--text-dim, #6b6575);
		background: none;
		border: none;
		cursor: pointer;
		padding: 0.2rem 0;
		transition: color 0.2s;
		flex: 1;
		text-align: left;
	}

	.hints-toggle:hover {
		color: var(--purple-glow, #9b6dd7);
	}

	.hints-chevron {
		display: inline-block;
		transition: transform 0.2s;
		font-size: 0.8em;
	}

	.hints-chevron.open {
		transform: rotate(90deg);
	}

	/* ---- TOOLS DRAWER ---- */
	.tools-drawer {
		position: fixed;
		top: 0;
		left: 0;
		bottom: 0;
		width: 260px;
		background: var(--bg-dark, #0a0a0f);
		border-right: 1px solid var(--text-ghost, #3d3647);
		z-index: 50;
		padding: 0.75rem;
		padding-top: 2.5rem;
		display: flex;
		flex-direction: column;
		transform: translateX(-100%);
		transition: transform 0.25s ease;
	}

	.tools-drawer.open {
		transform: translateX(0);
	}

	.tools-close {
		position: absolute;
		top: 0.5rem;
		right: 0.5rem;
		background: none;
		border: none;
		color: var(--text-dim, #6b6575);
		font-size: 1.4rem;
		cursor: pointer;
		line-height: 1;
		padding: 0.2rem 0.4rem;
	}

	.tools-close:hover {
		color: var(--bone, #e8dcc8);
	}

	.tool-section {
		margin-bottom: 0.75rem;
		padding-bottom: 0.75rem;
		border-bottom: 1px solid var(--text-ghost, #3d3647);
	}

	.tool-title {
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 1.1rem;
		font-weight: 400;
		color: var(--bone-dim, #8a7f6f);
		letter-spacing: 0.05em;
		margin-bottom: 0.5rem;
	}

	.tool-dim {
		font-size: 0.85rem;
		color: var(--text-ghost, #3d3647);
		font-style: italic;
	}

	.hint-btn {
		font-family: var(--font-mono, 'Courier New', monospace);
		font-size: 0.85rem;
		color: var(--purple-glow, #9b6dd7);
		background: var(--bg-panel, #1a1a2e);
		border: 1px solid var(--purple-mid, #6b3fa0);
		border-radius: 4px;
		padding: 0.35rem 0.8rem;
		cursor: pointer;
		transition: all 0.15s ease;
	}

	.hint-btn:hover {
		background: var(--purple-deep, #2d1b4e);
	}

	.hint-retry {
		margin-top: 0.4rem;
		font-size: 0.8rem;
		padding: 0.25rem 0.6rem;
	}

	.hint-loading {
		display: flex;
		gap: 0.3rem;
		padding: 0.35rem 0;
	}

	.hint-result {
		display: flex;
		align-items: baseline;
		gap: 0.6rem;
	}

	.hint-letter {
		font-family: var(--font-mono, 'Courier New', monospace);
		font-size: 1.8rem;
		font-weight: bold;
		color: var(--purple-glow, #9b6dd7);
	}

	.hint-value {
		font-size: 0.8rem;
		color: var(--text-dim, #6b6575);
		font-style: italic;
	}

	.hint-caveat {
		display: block;
		font-size: 0.75rem;
		color: var(--text-ghost, #3d3647);
		font-style: italic;
		margin-top: 0.25rem;
	}

	.tools-backdrop {
		position: fixed;
		inset: 0;
		z-index: 40;
		background: rgba(0, 0, 0, 0.4);
		border: none;
		cursor: default;
	}

	/* ---- GAME LAYOUT ---- */
	.game-layout {
		display: grid;
		grid-template-columns: 1fr 260px;
		height: 100vh;
		gap: 1rem;
		padding: 1rem;
		overflow: hidden;
	}

	.panel-right {
		display: flex;
		align-items: flex-start;
		justify-content: center;
		padding-top: 3rem;
	}

	.game-center {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 1.5rem;
		padding-top: 2rem;
		overflow-y: auto;
		min-height: 0;
	}

	.game-header {
		width: 100%;
		display: flex;
		justify-content: space-between;
		align-items: center;
		padding: 0 1rem;
	}

	.guesses-left {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: 0.1rem;
		flex: 1;
	}

	.game-title {
		font-family: var(--font-display, 'Creepster', cursive);
		font-weight: 400;
		font-size: 1.6rem;
		letter-spacing: 0.05em;
		line-height: 1;
		margin: 0;
	}

	.game-title .title-dead {
		color: var(--blood, #8b2233);
	}

	.game-title .title-letters {
		color: var(--bone, #e8dcc8);
	}

	.word-length-label {
		font-family: var(--font-display);
		font-size: 1.1rem;
		color: var(--text-dim);
		letter-spacing: 0.05em;
	}

	.guesses-label {
		font-size: 0.75rem;
		color: var(--text-dim);
		text-transform: uppercase;
		letter-spacing: 0.1em;
	}

	.guesses-number {
		font-family: var(--font-display);
		font-size: 2.5rem;
		color: var(--bone);
		line-height: 1;
		transition: color 0.3s;
	}

	.guesses-left.danger .guesses-number {
		color: var(--blood-bright);
		animation: pulse-danger 1s ease-in-out infinite;
	}

	.wrong-letters {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		flex-wrap: wrap;
		justify-content: center;
	}

	.wrong-label {
		color: var(--text-dim);
		font-size: 0.9rem;
		font-style: italic;
	}

	.wrong-letter {
		font-family: var(--font-mono);
		font-size: 1.1rem;
		color: var(--blood-bright);
		padding: 0.2rem 0.4rem;
		background: rgba(139, 34, 51, 0.15);
		border-radius: 3px;
	}

	.solve-status {
		font-size: 0.85rem;
		font-style: italic;
		padding: 0.3rem 0.8rem;
		border-radius: 4px;
	}

	.solve-status.degraded {
		color: #e8a838;
		background: rgba(232, 168, 56, 0.1);
		border: 1px solid rgba(232, 168, 56, 0.3);
	}

	.solve-status.unresolved {
		color: #e85838;
		background: rgba(232, 88, 56, 0.1);
		border: 1px solid rgba(232, 88, 56, 0.3);
	}

	.thinking {
		display: flex;
		gap: 0.4rem;
		align-items: center;
		justify-content: center;
		height: 1.5rem;
	}

	.thinking-dot {
		width: 0.5rem;
		height: 0.5rem;
		border-radius: 50%;
		background: var(--purple-mid, #6b3fa0);
		animation: thinking-pulse 1.2s ease-in-out infinite;
	}

	.thinking-dot:nth-child(2) {
		animation-delay: 0.2s;
	}

	.thinking-dot:nth-child(3) {
		animation-delay: 0.4s;
	}

	@keyframes thinking-pulse {
		0%, 80%, 100% { opacity: 0.2; transform: scale(0.8); }
		40% { opacity: 1; transform: scale(1.2); }
	}

	.error-msg {
		color: var(--blood-bright);
		font-style: italic;
		font-size: 0.9rem;
	}

	.gallows-container {
		position: sticky;
		top: 3rem;
	}

	@keyframes pulse-danger {
		0%, 100% { opacity: 1; }
		50% { opacity: 0.5; }
	}

	/* ---- RESPONSIVE ---- */
	@media (max-width: 900px) {
		.game-layout {
			grid-template-columns: 1fr;
			grid-template-rows: auto 1fr auto;
		}

		.panel-right {
			order: -1;
			padding-top: 1rem;
		}

		.gallows-container {
			position: static;
		}
	}

</style>
