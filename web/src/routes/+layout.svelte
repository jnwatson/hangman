<script lang="ts">
	import '../app.css';
	let { children } = $props();
</script>

<svelte:head>
	<title>Dead Letters</title>
	<meta name="description" content="The hardest game of hangman you'll ever play." />
</svelte:head>

{@render children()}
