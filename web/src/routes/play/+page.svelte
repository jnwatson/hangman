<script lang="ts">
	import Gallows from '$lib/components/Gallows.svelte';
	import WordDisplay from '$lib/components/WordDisplay.svelte';
	import Keyboard from '$lib/components/Keyboard.svelte';
	import WordList from '$lib/components/WordList.svelte';
	import DeathScreen from '$lib/components/DeathScreen.svelte';
	import { createGameState } from '$lib/stores/game.svelte';
	import { loadDictionary, wordsByLength } from '$lib/dictionary';

	const game = createGameState();

	let allWords = $state<string[]>([]);
	let wordList = $derived(
		game.state ? wordsByLength(allWords, game.state.wordLength) : []
	);

	let gamesPlayed = $state(0);

	let wrongCount = $derived(game.state?.wrongLetters.length ?? 0);
	let totalGuessed = $derived(game.state?.guessedLetters.size ?? 0);
	let maxWrong = $derived((game.state?.guessesAllowed ?? 6) + 1);
	let hangmanStage = $derived(Math.min(Math.round(wrongCount * 7 / maxWrong), 7));

	function randomLength(): number {
		return Math.floor(Math.random() * 12) + 3; // 3–14
	}

	// Load dictionary and start game on mount
	$effect(() => {
		if (!game.started && !game.loading) {
			loadDictionary().then(words => {
				allWords = words;
			});
			game.newGame(randomLength());
		}
	});

	function handleGuess(letter: string) {
		game.guess(letter);
	}

	function playAgain() {
		gamesPlayed++;
		game.newGame(randomLength());
	}
</script>

{#if game.state}
	<!-- GAME BOARD -->
	<div class="game-layout">
		<!-- Left: Word List -->
		<aside class="panel-left">
			<WordList
				words={wordList}
				validWordsBitvec={game.state.validWordsBitvec}
				showCheat={gamesPlayed >= 1}
			/>
		</aside>

		<!-- Center: Game Area -->
		<main class="game-center">
			<div class="game-header">
				<a href="/" class="back-link">&larr;</a>
				<span class="word-length-label">{game.state.wordLength} letters</span>
				<div class="guesses-left" class:danger={game.state.guessesLeft <= 2}>
					<span class="guesses-label">Guesses left</span>
					<span class="guesses-number">{game.state.guessesLeft}</span>
				</div>
			</div>

			<WordDisplay pattern={game.state.pattern} />

			{#if game.state.wrongLetters.length > 0}
				<div class="wrong-letters">
					<span class="wrong-label">Wrong:</span>
					{#each game.state.wrongLetters as letter}
						<span class="wrong-letter">{letter}</span>
					{/each}
				</div>
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
	.back-link {
		font-family: var(--font-body);
		font-size: 1rem;
		color: var(--text-dim);
		text-decoration: none;
		transition: color 0.2s;
	}

	.back-link:hover {
		color: var(--purple-glow);
	}

	/* ---- GAME LAYOUT ---- */
	.game-layout {
		display: grid;
		grid-template-columns: 220px 1fr 260px;
		height: 100vh;
		gap: 1rem;
		padding: 1rem;
		overflow: hidden;
	}

	.panel-left {
		min-height: 0;
		overflow: hidden;
		display: flex;
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
		align-items: center;
		gap: 0.1rem;
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

		.panel-left {
			display: none; /* collapse word list on mobile */
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
