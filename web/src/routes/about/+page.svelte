<div class="about">
	<header class="about-header">
		<h1 class="about-title">About <span class="title-dead">Dead</span> <span class="title-letters">Letters</span></h1>
		<a href="/" class="back-link">&larr; Back</a>
	</header>

	<section class="about-section">
		<h2>What is this?</h2>
		<p>
			Dead Letters is <strong>adversarial hangman</strong>, also known as evil hangman
			or Schr&ouml;dinger's hangman. Unlike normal hangman, the referee doesn't pick
			a word in advance. Instead, after each guess, the referee chooses whichever
			response &mdash; hit or miss &mdash; makes things worst for the guesser, as long as at
			least one dictionary word remains consistent with all previous responses.
		</p>
		<p>
			The referee in Dead Letters plays <strong>optimally</strong>. Every response is the
			mathematically worst-case move, computed by a minimax solver that has analyzed
			the full game tree.
		</p>
	</section>

	<section class="about-section">
		<h2>Can I actually win?</h2>
		<p>
			No. The game allows you one fewer miss than the theoretical minimum, so
			even with perfect play, the referee will always hang you. The table below
			shows the minimum number of wrong guesses required for each word length
			&mdash; you get one fewer than that.
		</p>

		<div class="results-table-wrap">
			<table class="results-table">
				<thead>
					<tr><th>Length</th><th>Words</th><th>Min. Misses</th></tr>
				</thead>
				<tbody>
					<tr><td>3</td><td>972</td><td>17</td></tr>
					<tr><td>4</td><td>3,903</td><td>16</td></tr>
					<tr><td>5</td><td>8,636</td><td>15</td></tr>
					<tr><td>6</td><td>15,232</td><td>12</td></tr>
					<tr><td>7</td><td>23,109</td><td>11</td></tr>
					<tr><td>8</td><td>28,420</td><td>8</td></tr>
					<tr><td>9</td><td>24,873</td><td>6</td></tr>
					<tr><td>10</td><td>20,300</td><td>5</td></tr>
					<tr><td>11</td><td>15,504</td><td>6</td></tr>
					<tr><td>12</td><td>11,357</td><td>4</td></tr>
					<tr><td>13</td><td>7,827</td><td>4</td></tr>
					<tr><td>14</td><td>5,127</td><td>3</td></tr>
				</tbody>
			</table>
		</div>
		<p class="table-note">
			Longer words are easier. At 20+ letters, only 1 miss is needed.
		</p>
	</section>

	<section class="about-section">
		<h2>How does the solver work?</h2>
		<p>
			The game is modeled as a two-player perfect-information game and solved with
			<strong>alpha-beta minimax search</strong>. The guesser picks a letter; the
			referee partitions the remaining words by where that letter appears (or doesn't)
			and picks the worst partition. The solver explores this tree to find optimal
			strategies for both sides.
		</p>
		<p>Key optimizations that make this tractable:</p>
		<ul>
			<li><strong>MTD(f) with Lazy SMP</strong> &mdash; iterative deepening with null-window probes, parallelized across cores</li>
			<li><strong>Transposition table</strong> &mdash; canonical hashing identifies equivalent positions across different guess orderings</li>
			<li><strong>History heuristic</strong> &mdash; letters that have been empirically good are tried first</li>
			<li><strong>Miss-chain lower bounds</strong> &mdash; quickly proves positions require more misses than the current bound</li>
			<li><strong>Disk cache</strong> &mdash; LMDB-backed persistent cache means positions are solved once and reused</li>
		</ul>
	</section>

	<section class="about-section">
		<h2>Dictionary</h2>
		<p>
			Dead Letters uses the <a href="https://norvig.com/ngrams/enable1.txt" class="inline-link">enable1</a> word list (172,820 words), a
			standard competitive Scrabble dictionary. The full word list is available in
			the hints panel during play.
		</p>
	</section>

	<section class="about-section">
		<h2>Source code</h2>
		<p>
			The solver, server, and frontend are open source under the MIT license.
		</p>
		<p>
			<a href="https://github.com/jnwatson/hangman" class="github-link">github.com/jnwatson/hangman</a>
		</p>
	</section>

	<div class="about-footer">
		<a href="/play" class="play-link">Play the game &rarr;</a>
	</div>
</div>

<style>
	.about {
		max-width: 640px;
		margin: 0 auto;
		padding: 3rem 2rem;
	}

	.about-header {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		margin-bottom: 2.5rem;
		border-bottom: 1px solid var(--text-ghost, #3d3647);
		padding-bottom: 1.5rem;
	}

	.about-title {
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 1.6rem;
		font-weight: 400;
		letter-spacing: 0.05em;
		color: var(--bone-dim, #8a7f6f);
	}

	.about-title .title-dead {
		color: var(--blood, #8b2233);
	}

	.about-title .title-letters {
		color: var(--bone, #e8dcc8);
	}

	.back-link {
		font-size: 0.9rem;
		color: var(--text-dim, #6b6575);
		text-decoration: none;
		transition: color 0.2s;
		white-space: nowrap;
	}

	.back-link:hover {
		color: var(--purple-glow, #9b6dd7);
	}

	.about-section {
		margin-bottom: 2rem;
	}

	.about-section h2 {
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 1.3rem;
		font-weight: 400;
		color: var(--bone, #e8dcc8);
		letter-spacing: 0.04em;
		margin-bottom: 0.75rem;
	}

	.about-section p {
		font-family: var(--font-body, Georgia, serif);
		font-size: 0.95rem;
		color: var(--text-dim, #6b6575);
		line-height: 1.7;
		margin-bottom: 0.75rem;
	}

	.about-section strong {
		color: var(--bone, #e8dcc8);
	}

	.about-section ul {
		list-style: none;
		padding: 0;
		margin: 0.5rem 0;
	}

	.about-section li {
		font-family: var(--font-body, Georgia, serif);
		font-size: 0.9rem;
		color: var(--text-dim, #6b6575);
		line-height: 1.6;
		padding: 0.3rem 0;
		padding-left: 1.2rem;
		position: relative;
	}

	.about-section li::before {
		content: '';
		position: absolute;
		left: 0;
		top: 0.75rem;
		width: 5px;
		height: 5px;
		border-radius: 50%;
		background: var(--purple-mid, #6b3fa0);
	}

	.about-section li strong {
		color: var(--purple-glow, #9b6dd7);
	}

	.results-table-wrap {
		overflow-x: auto;
		margin: 1rem 0;
	}

	.results-table {
		width: 100%;
		border-collapse: collapse;
		font-family: var(--font-mono, 'Courier New', monospace);
		font-size: 0.85rem;
	}

	.results-table th {
		text-align: left;
		color: var(--bone-dim, #8a7f6f);
		font-weight: 400;
		padding: 0.4rem 0.8rem;
		border-bottom: 1px solid var(--text-ghost, #3d3647);
		font-size: 0.8rem;
		text-transform: uppercase;
		letter-spacing: 0.08em;
	}

	.results-table td {
		padding: 0.35rem 0.8rem;
		color: var(--text-dim, #6b6575);
		border-bottom: 1px solid rgba(61, 54, 71, 0.3);
	}

	.results-table td:last-child {
		color: var(--purple-glow, #9b6dd7);
		font-weight: bold;
	}

	.table-note {
		font-size: 0.85rem !important;
		font-style: italic;
	}

	.inline-link {
		color: var(--purple-glow, #9b6dd7);
		text-decoration: none;
		font-weight: bold;
		transition: color 0.2s;
	}

	.inline-link:hover {
		color: var(--bone, #e8dcc8);
	}

	.github-link {
		font-family: var(--font-mono, 'Courier New', monospace);
		color: var(--purple-glow, #9b6dd7);
		text-decoration: none;
		font-size: 0.95rem;
		transition: color 0.2s;
	}

	.github-link:hover {
		color: var(--bone, #e8dcc8);
	}

	.about-footer {
		margin-top: 2.5rem;
		padding-top: 1.5rem;
		border-top: 1px solid var(--text-ghost, #3d3647);
		text-align: center;
	}

	.play-link {
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 1.2rem;
		color: var(--accent-blue, #4a7fb5);
		text-decoration: none;
		letter-spacing: 0.05em;
		transition: color 0.2s;
	}

	.play-link:hover {
		color: var(--accent-blue-glow, #5b9fd4);
	}

	@media (max-width: 600px) {
		.about {
			padding: 2rem 1.25rem;
		}

		.about-header {
			flex-direction: column;
			gap: 0.5rem;
		}
	}
</style>
