<script lang="ts">
	/**
	 * stage: 0 = empty gallows, 1 = head, 2 = body, 3 = left arm,
	 *        4 = right arm, 5 = left leg, 6 = right leg, 7+ = dead (X eyes)
	 * size: pixel width/height of the SVG
	 * dim: if true, render faintly (for background use)
	 */
	let { stage = 0, size = 300, dim = false }: {
		stage?: number;
		size?: number;
		dim?: boolean;
	} = $props();

	const dead = $derived(stage >= 7);
</script>

<svg
	viewBox="0 0 200 240"
	width={size}
	height={size * 1.2}
	class="gallows"
	class:dim
	xmlns="http://www.w3.org/2000/svg"
>
	<!-- Base platform -->
	<line x1="20" y1="220" x2="180" y2="220" class="wood" />

	<!-- Upright post -->
	<line x1="50" y1="220" x2="50" y2="20" class="wood" />

	<!-- Top beam -->
	<line x1="50" y1="20" x2="130" y2="20" class="wood" />

	<!-- Support brace -->
	<line x1="50" y1="50" x2="80" y2="20" class="wood" />

	<!-- Rope -->
	<line x1="130" y1="20" x2="130" y2="50" class="rope" />

	{#if stage >= 1}
		<!-- Head -->
		<circle cx="130" cy="65" r="15" class="body-part" class:dead />
		{#if dead}
			<!-- X eyes -->
			<line x1="123" y1="60" x2="129" y2="66" class="eyes" />
			<line x1="129" y1="60" x2="123" y2="66" class="eyes" />
			<line x1="131" y1="60" x2="137" y2="66" class="eyes" />
			<line x1="137" y1="60" x2="131" y2="66" class="eyes" />
		{:else}
			<!-- Dot eyes -->
			<circle cx="125" cy="63" r="1.5" class="eyes-dot" />
			<circle cx="135" cy="63" r="1.5" class="eyes-dot" />
		{/if}
	{/if}

	{#if stage >= 2}
		<!-- Body -->
		<line x1="130" y1="80" x2="130" y2="140" class="body-part" />
	{/if}

	{#if stage >= 3}
		<!-- Left arm -->
		<line x1="130" y1="95" x2="105" y2="120" class="body-part" />
	{/if}

	{#if stage >= 4}
		<!-- Right arm -->
		<line x1="130" y1="95" x2="155" y2="120" class="body-part" />
	{/if}

	{#if stage >= 5}
		<!-- Left leg -->
		<line x1="130" y1="140" x2="110" y2="175" class="body-part" />
	{/if}

	{#if stage >= 6}
		<!-- Right leg -->
		<line x1="130" y1="140" x2="150" y2="175" class="body-part" />
	{/if}
</svg>

<style>
	.gallows {
		display: block;
	}

	.gallows.dim {
		opacity: 0.3;
	}

	.wood {
		stroke: var(--gallows-wood, #5c3d2e);
		stroke-width: 4;
		stroke-linecap: round;
	}

	.rope {
		stroke: var(--rope, #c4a777);
		stroke-width: 2;
		stroke-linecap: round;
	}

	.body-part {
		stroke: var(--bone, #e8dcc8);
		stroke-width: 3;
		stroke-linecap: round;
		fill: none;
		transition: all 0.3s ease;
	}

	.body-part.dead {
		stroke: var(--blood, #8b2233);
	}

	.eyes-dot {
		fill: var(--bone, #e8dcc8);
	}

	.eyes {
		stroke: var(--blood, #8b2233);
		stroke-width: 2;
		stroke-linecap: round;
	}
</style>
