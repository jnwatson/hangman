<script lang="ts">
	let {
		words = [] as string[],
		validWordsBitvec = null as Uint8Array | null,
		showCheat = false,
	}: {
		words?: string[];
		validWordsBitvec?: Uint8Array | null;
		showCheat?: boolean;
	} = $props();

	let search = $state('');
	let hintMode = $state(false);

	const displayWords = $derived(() => {
		let list = words;
		if (hintMode && validWordsBitvec) {
			const bv = validWordsBitvec;
			list = list.filter((_, i) => (bv[i >> 3] & (1 << (i & 7))) !== 0);
		}
		if (search) {
			const s = search.toLowerCase();
			list = list.filter(w => w.includes(s));
		}
		return list;
	});

	const displayList = $derived(displayWords());
</script>

<div class="wordlist-panel">
	<div class="panel-header">
		<h3 class="panel-title">Dictionary</h3>
		<span class="word-count">{displayList.length.toLocaleString()} words</span>
	</div>

	<input
		class="search-box"
		type="text"
		placeholder="Search..."
		bind:value={search}
	/>

	{#if showCheat}
		<label class="hint-toggle">
			<input type="checkbox" bind:checked={hintMode} />
			<span class="hint-label">Cheat: show only valid words</span>
		</label>
	{/if}

	<div class="word-scroll">
		{#each displayList as word}
			<div class="word-item">{word}</div>
		{/each}
	</div>
</div>

<style>
	.wordlist-panel {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		background: var(--bg-card, #13131f);
		border: 1px solid var(--text-ghost, #3d3647);
		border-radius: 6px;
		padding: 0.75rem;
		height: 100%;
		min-height: 0;
	}

	.panel-header {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
	}

	.panel-title {
		font-family: var(--font-display, 'Creepster', cursive);
		font-size: 1.2rem;
		font-weight: 400;
		color: var(--bone-dim, #8a7f6f);
		letter-spacing: 0.05em;
	}

	.word-count {
		font-size: 0.8rem;
		color: var(--text-dim, #6b6575);
	}

	.search-box {
		width: 100%;
		padding: 0.4rem 0.6rem;
		font-family: var(--font-mono, 'Courier New', monospace);
		font-size: 0.85rem;
		color: var(--bone, #e8dcc8);
		background: var(--bg-dark, #0a0a0f);
		border: 1px solid var(--text-ghost, #3d3647);
		border-radius: 3px;
		outline: none;
	}

	.search-box:focus {
		border-color: var(--purple-mid, #6b3fa0);
	}

	.search-box::placeholder {
		color: var(--text-ghost, #3d3647);
	}

	.hint-toggle {
		display: flex;
		align-items: center;
		gap: 0.4rem;
		cursor: pointer;
		padding: 0.25rem 0;
	}

	.hint-toggle input[type="checkbox"] {
		accent-color: var(--purple-mid, #6b3fa0);
		width: 14px;
		height: 14px;
		cursor: pointer;
	}

	.hint-label {
		font-size: 0.8rem;
		font-style: italic;
		color: var(--text-dim, #6b6575);
		transition: color 0.2s;
	}

	.hint-toggle:hover .hint-label {
		color: var(--purple-glow, #9b6dd7);
	}

	.word-scroll {
		flex: 1;
		overflow-y: auto;
		min-height: 0;
	}

	.word-item {
		font-family: var(--font-mono, 'Courier New', monospace);
		font-size: 0.8rem;
		color: var(--text-dim, #6b6575);
		padding: 0.1rem 0.3rem;
		border-bottom: 1px solid var(--bg-dark, #0a0a0f);
	}

	.word-item:hover {
		color: var(--bone, #e8dcc8);
		background: var(--bg-panel, #1a1a2e);
	}

	.word-scroll::-webkit-scrollbar {
		width: 6px;
	}

	.word-scroll::-webkit-scrollbar-track {
		background: var(--bg-dark, #0a0a0f);
	}

	.word-scroll::-webkit-scrollbar-thumb {
		background: var(--text-ghost, #3d3647);
		border-radius: 3px;
	}
</style>
