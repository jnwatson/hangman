<script lang="ts">
	import Gallows from '$lib/components/Gallows.svelte';

	let difficulty = $state<'hard' | 'hardest'>('hardest');

	$effect(() => {
		const stored = localStorage.getItem('difficulty');
		if (stored === 'hard' || stored === 'hardest') {
			difficulty = stored;
		}
	});

	function setDifficulty(d: 'hard' | 'hardest') {
		difficulty = d;
		localStorage.setItem('difficulty', d);
	}
</script>

<div class="landing">
	<div class="fog fog-1"></div>
	<div class="fog fog-2"></div>

	<div class="gallows-bg">
		<Gallows stage={0} size={200} dim={true} />
	</div>

	<main class="content">
		<h1 class="title">
			<span class="title-dead">Dead</span>
			<span class="title-letters">Letters</span>
		</h1>

		<p class="tagline">The hardest game of hangman you'll ever play.</p>

		<div class="difficulty-picker" role="radiogroup" aria-label="Difficulty">
			<button
				type="button"
				class="diff-btn"
				class:active={difficulty === 'hard'}
				role="radio"
				aria-checked={difficulty === 'hard'}
				onclick={() => setDifficulty('hard')}
			>Hard</button>
			<button
				type="button"
				class="diff-btn"
				class:active={difficulty === 'hardest'}
				role="radio"
				aria-checked={difficulty === 'hardest'}
				onclick={() => setDifficulty('hardest')}
			>Hardest</button>
		</div>
		<p class="diff-caption">
			{#if difficulty === 'hardest'}
				Unwinnable. Optimal play hangs you by one miss.
			{:else}
				Survivable. Optimal play just barely escapes.
			{/if}
		</p>

		<a href="/play" class="play-btn">
			<span class="play-text">PLAY</span>
			<span class="play-glow"></span>
		</a>
	</main>

	<footer class="landing-footer">
		<a href="/about" class="about-link">What is this?</a>
	</footer>
</div>

<style>
	.landing {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		min-height: 100vh;
		position: relative;
		overflow: hidden;
	}

	.gallows-bg {
		position: absolute;
		top: 50%;
		left: 50%;
		transform: translate(-50%, -50%);
		opacity: 0.06;
		pointer-events: none;
	}

	.content {
		position: relative;
		z-index: 1;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 2rem;
		text-align: center;
		padding: 2rem;
	}

	.title {
		font-family: var(--font-display);
		font-weight: 400;
		line-height: 1.1;
		letter-spacing: 0.05em;
		display: flex;
		flex-direction: column;
		gap: 0;
	}

	.title-dead {
		font-size: 5rem;
		color: var(--blood);
		text-shadow:
			0 0 40px rgba(139, 34, 51, 0.4),
			0 0 80px rgba(139, 34, 51, 0.2);
	}

	.title-letters {
		font-size: 6rem;
		color: var(--bone);
		text-shadow:
			0 0 30px rgba(232, 220, 200, 0.15),
			2px 2px 0 rgba(0, 0, 0, 0.5);
		margin-top: -1rem;
	}

	.tagline {
		font-family: var(--font-body);
		font-size: 1.3rem;
		font-style: italic;
		color: var(--text-dim);
		max-width: 400px;
	}

	.difficulty-picker {
		display: inline-flex;
		border: 1px solid var(--text-ghost, #3d3647);
		border-radius: 4px;
		overflow: hidden;
		margin-top: -0.5rem;
	}

	.diff-btn {
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 1rem;
		letter-spacing: 0.08em;
		padding: 0.45rem 1.4rem;
		background: transparent;
		color: var(--text-dim, #6b6575);
		border: none;
		cursor: pointer;
		transition: background 0.15s, color 0.15s;
	}

	.diff-btn:not(:last-child) {
		border-right: 1px solid var(--text-ghost, #3d3647);
	}

	.diff-btn:hover {
		color: var(--bone, #e8dcc8);
	}

	.diff-btn.active {
		background: var(--blood, #8b2233);
		color: var(--bone, #e8dcc8);
	}

	.diff-caption {
		font-family: var(--font-body);
		font-size: 0.85rem;
		font-style: italic;
		color: var(--text-ghost, #3d3647);
		margin-top: -0.75rem;
		max-width: 400px;
		min-height: 1.2em;
	}

	.play-btn {
		position: relative;
		display: inline-block;
		margin-top: 1rem;
		padding: 1rem 4rem;
		font-family: var(--font-display);
		font-size: 2rem;
		letter-spacing: 0.15em;
		color: var(--bone);
		background: var(--accent-blue);
		border-radius: 4px;
		text-decoration: none;
		transition: all 0.3s ease;
		overflow: hidden;
	}

	.play-btn:hover {
		background: var(--accent-blue-glow);
		color: white;
		transform: scale(1.05);
		box-shadow:
			0 0 30px rgba(74, 127, 181, 0.4),
			0 0 60px rgba(74, 127, 181, 0.2);
	}

	.play-btn:active {
		transform: scale(0.98);
	}

	.play-glow {
		position: absolute;
		inset: 0;
		background: linear-gradient(
			135deg,
			transparent 30%,
			rgba(255, 255, 255, 0.1) 50%,
			transparent 70%
		);
		animation: shimmer 3s ease-in-out infinite;
	}

	.landing-footer {
		position: absolute;
		bottom: 2rem;
		z-index: 1;
	}

	.about-link {
		display: block;
		font-family: var(--font-body);
		font-size: 0.85rem;
		color: var(--text-ghost);
		text-decoration: none;
		transition: color 0.2s;
	}

	.about-link:hover {
		color: var(--purple-glow);
	}

	/* Fog layers */
	.fog {
		position: absolute;
		width: 200%;
		height: 100%;
		top: 0;
		left: -50%;
		background: radial-gradient(
			ellipse at center,
			rgba(45, 27, 78, 0.08) 0%,
			transparent 70%
		);
		pointer-events: none;
	}

	.fog-1 {
		animation: drift 20s ease-in-out infinite;
	}

	.fog-2 {
		animation: drift 15s ease-in-out infinite reverse;
		opacity: 0.5;
	}

	@keyframes shimmer {
		0%, 100% { transform: translateX(-100%); }
		50% { transform: translateX(100%); }
	}

	@keyframes drift {
		0%, 100% { transform: translateX(-10%); }
		50% { transform: translateX(10%); }
	}

	@media (max-width: 600px) {
		.title-dead { font-size: 3.5rem; }
		.title-letters { font-size: 4rem; margin-top: -0.5rem; }
		.tagline { font-size: 1.1rem; }
		.play-btn { font-size: 1.5rem; padding: 0.8rem 3rem; }
	}
</style>
