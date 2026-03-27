let allWords: string[] | null = null;
let loading: Promise<string[]> | null = null;

/** Load enable1.txt dictionary (cached after first load). */
export async function loadDictionary(): Promise<string[]> {
	if (allWords) return allWords;
	if (loading) return loading;

	loading = fetch('/enable1.txt')
		.then(res => {
			if (!res.ok) throw new Error('Failed to load dictionary');
			return res.text();
		})
		.then(text => {
			allWords = text
				.split('\n')
				.map(w => w.trim().toLowerCase())
				.filter(w => w.length > 0 && /^[a-z]+$/.test(w));
			return allWords;
		});

	return loading;
}

/** Get words of a specific length from the cached dictionary. */
export function wordsByLength(words: string[], length: number): string[] {
	return words.filter(w => w.length === length);
}
