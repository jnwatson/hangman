const API_BASE = '/api';

// Set true to use a fake in-memory game (no backend needed)
const MOCK = false;

export interface GameState {
	gameId: string;
	wordLength: number;
	guessesAllowed: number;
	guessesLeft: number;
	pattern: string[];
	wrongLetters: string[];
	guessedLetters: Set<string>;
	gameOver: boolean;
	won: boolean;
	exampleWord: string | null;
	minimaxValue: number | null;
	/** Bitvector of words still valid after the referee's choices (from base64). */
	validWordsBitvec: Uint8Array | null;
	/** Worst solve status seen: "solved", "degraded", or "unresolved". */
	solveStatus: 'solved' | 'degraded' | 'unresolved';
}

// Simple mock: picks a random word, plays normal (non-adversarial) hangman
const MOCK_WORDS: Record<number, string[]> = {
	3: ['cat', 'dog', 'bat', 'fox', 'owl', 'rat', 'pig', 'hen', 'yak', 'emu'],
	4: ['frog', 'lynx', 'wasp', 'moth', 'toad', 'wren', 'hawk', 'deer', 'hare', 'newt'],
	5: ['snake', 'crane', 'squid', 'shark', 'bison', 'moose', 'goose', 'raven', 'viper', 'otter'],
	6: ['jaguar', 'parrot', 'falcon', 'donkey', 'lizard', 'turtle', 'ferret', 'weasel', 'badger', 'pigeon'],
	7: ['penguin', 'panther', 'dolphin', 'buffalo', 'cheetah', 'gorilla', 'giraffe', 'ostrich', 'pelican', 'sparrow'],
	8: ['elephant', 'anteater', 'hedgehog', 'chipmunk', 'flamingo', 'kangaroo', 'platypus', 'scorpion', 'tortoise', 'goldfish'],
	9: ['alligator', 'porcupine', 'crocodile', 'butterfly', 'chameleon', 'dragonfly', 'jellyfish', 'armadillo', 'centipede', 'wolverine'],
	10: ['chimpanzee', 'woodpecker', 'rhinoceros', 'salamander', 'orangutang', 'chinchilla', 'kingfisher', 'roadrunner', 'copperhead', 'cuttlefish'],
	11: ['hummingbird', 'mockingbird', 'rattlesnake', 'caterpillar', 'grasshopper', 'nightingale', 'stickleback', 'thunderbird', 'woodchecker', 'yellowtaill'],
	12: ['hippopotamus', 'butterscotch', 'housebreaker', 'kleptomaniac', 'backbreaking', 'checkerboard', 'frontrunning', 'hallmarkings', 'overwhelming', 'thanksgiving'],
	13: ['unforgettable', 'communication', 'extraordinary', 'approximately', 'constellation', 'understanding', 'investigation', 'opportunities', 'contributions', 'entertainment'],
	14: ['accomplishment', 'transformation', 'characteristic', 'interpretation', 'recommendation', 'representation', 'responsibility', 'infrastructure', 'discrimination', 'superintendent'],
};

function pickMockWord(k: number): string {
	const words = MOCK_WORDS[k];
	if (!words || words.length === 0) return 'x'.repeat(k);
	return words[Math.floor(Math.random() * words.length)];
}

// Mock minimax values (approximate, for display)
const MOCK_MINIMAX: Record<number, number> = {
	3: 17, 4: 16, 5: 14, 6: 12, 7: 11, 8: 8, 9: 6, 10: 5, 11: 6, 12: 4, 13: 4, 14: 3,
};

export function createGameState() {
	let state = $state<GameState | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let _mockWord = $state('');
	let _started = $state(false);

	async function newGame(wordLength: number) {
		cancelHint();
		loading = true;
		error = null;
		hint = null;
		_started = true;

		if (MOCK) {
			_mockWord = pickMockWord(wordLength);
			const guesses = (MOCK_MINIMAX[wordLength] ?? 8) - 1;
			state = {
				gameId: 'mock-' + Math.random().toString(36).slice(2),
				wordLength,
				guessesAllowed: guesses,
				guessesLeft: guesses,
				pattern: Array(wordLength).fill('_'),
				wrongLetters: [],
				guessedLetters: new Set(),
				gameOver: false,
				won: false,
				exampleWord: null,
				minimaxValue: MOCK_MINIMAX[wordLength] ?? null,
				validWordsBitvec: null,
			solveStatus: 'solved',
			};
			loading = false;
			return;
		}

		try {
			const res = await fetch(`${API_BASE}/new?length=${wordLength}`);
			if (!res.ok) throw new Error(await res.text());
			const data = await res.json();
			state = {
				gameId: data.game_id,
				wordLength: data.word_length,
				guessesAllowed: data.guesses_allowed,
				guessesLeft: data.guesses_allowed,
				pattern: Array(data.word_length).fill('_'),
				wrongLetters: [],
				guessedLetters: new Set(),
				gameOver: false,
				won: false,
				exampleWord: null,
				minimaxValue: data.minimax_value ?? null,
				validWordsBitvec: null,
			solveStatus: 'solved',
			};
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to start game';
		} finally {
			loading = false;
		}
	}

	async function guess(letter: string) {
		if (!state || state.gameOver || state.guessedLetters.has(letter)) return;

		cancelHint();
		loading = true;
		error = null;
		hint = null;

		if (MOCK) {
			state.guessedLetters = new Set([...state.guessedLetters, letter]);
			const positions: number[] = [];
			for (let i = 0; i < _mockWord.length; i++) {
				if (_mockWord[i] === letter) positions.push(i);
			}

			if (positions.length > 0) {
				for (const pos of positions) {
					state.pattern[pos] = letter.toUpperCase();
				}
			} else {
				state.wrongLetters = [...state.wrongLetters, letter.toUpperCase()];
				state.guessesLeft--;
			}

			// Check win
			if (!state.pattern.includes('_')) {
				state.gameOver = true;
				state.won = true;
				state.exampleWord = _mockWord;
			}
			// Check loss
			if (state.guessesLeft <= 0) {
				state.gameOver = true;
				state.won = false;
				state.exampleWord = _mockWord;
			}

			loading = false;
			return;
		}

		try {
			const res = await fetch(`${API_BASE}/guess`, {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ game_id: state.gameId, letter }),
			});
			if (!res.ok) throw new Error(await res.text());
			const data = await res.json();

			state.guessedLetters = new Set([...state.guessedLetters, letter]);

			if (data.positions && data.positions.length > 0) {
				for (const pos of data.positions) {
					state.pattern[pos] = letter.toUpperCase();
				}
			} else {
				state.wrongLetters = [...state.wrongLetters, letter.toUpperCase()];
				state.guessesLeft--;
			}

			state.gameOver = data.game_over;
			state.won = data.won ?? false;
			if (data.example_word) {
				state.exampleWord = data.example_word;
			}
			if (data.valid_words_bitvec) {
				const bin = atob(data.valid_words_bitvec);
				const bv = new Uint8Array(bin.length);
				for (let i = 0; i < bin.length; i++) bv[i] = bin.charCodeAt(i);
				state.validWordsBitvec = bv;
			} else {
				state.validWordsBitvec = null;
			}
			const status = data.solve_status as 'solved' | 'degraded' | 'unresolved';
			const rank = { solved: 0, degraded: 1, unresolved: 2 };
			if (rank[status] > rank[state.solveStatus]) {
				state.solveStatus = status;
			}
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to submit guess';
		} finally {
			loading = false;
		}
	}

	let hint = $state<{ letter: string; value: number | null } | null>(null);
	let hintLoading = $state(false);
	let hintAbort: AbortController | null = null;

	function cancelHint() {
		if (hintAbort) {
			hintAbort.abort();
			hintAbort = null;
			hintLoading = false;
		}
	}

	async function fetchHint() {
		if (!state || state.gameOver) return;
		cancelHint();
		hintLoading = true;
		hint = null;
		hintAbort = new AbortController();
		try {
			const res = await fetch(`${API_BASE}/hint?game_id=${state.gameId}`, {
				signal: hintAbort.signal,
			});
			if (!res.ok) throw new Error(await res.text());
			const data = await res.json();
			hint = { letter: data.letter, value: data.value ?? null };
		} catch (e) {
			if (e instanceof DOMException && e.name === 'AbortError') return;
			hint = null;
		} finally {
			hintAbort = null;
			hintLoading = false;
		}
	}

	return {
		get state() { return state; },
		get loading() { return loading; },
		get error() { return error; },
		get started() { return _started; },
		get hint() { return hint; },
		get hintLoading() { return hintLoading; },
		newGame,
		guess,
		fetchHint,
	};
}
