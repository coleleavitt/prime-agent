/**
 * Deterministic, forkable seeded RNG for the Dream-RSI subsystem.
 *
 * This is the ONLY source of randomness in the dream core: the tree, replay,
 * objective and task code never call `Math.random` or `Date.now`. Determinism
 * flows through this one injected generator, so an online rollout and its
 * frozen replay are byte-reproducible from a seed.
 *
 * The generator is splitmix64 over a masked 64-bit BigInt state. `fork(label)`
 * derives a child generator by hashing the ORIGINAL seed with the label, so a
 * fork is independent of how many draws its parent has taken — the label, not
 * the call order, decides the child stream.
 */

const MASK64 = (1n << 64n) - 1n;
const GAMMA = 0x9e3779b97f4a7c15n;
const MIX1 = 0xbf58476d1ce4e5b9n;
const MIX2 = 0x94d049bb133111ebn;
const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;
const TWO_POW_53 = 2 ** 53;
const MIN_POSITIVE_UNIT = 2 ** -53;

export interface SeededRng {
	/** Uniform double in [0, 1). */
	next(): number;
	/** Uniform integer in [0, maxExclusive). */
	nextInt(maxExclusive: number): number;
	/** Standard-normal draw (finite). */
	nextGaussian(): number;
	/** A child generator whose stream depends on `label` and the seed, not on this generator's draws. */
	fork(label: string): SeededRng;
}

function hashStringToU64(input: string): bigint {
	let hash = FNV_OFFSET;
	for (let index = 0; index < input.length; index++) {
		hash ^= BigInt(input.charCodeAt(index));
		hash = (hash * FNV_PRIME) & MASK64;
	}
	return hash;
}

function seedToU64(seed: number | string): bigint {
	if (typeof seed === "number") {
		if (!Number.isFinite(seed)) throw new TypeError("SeededRng seed must be a finite number");
		return BigInt(Math.trunc(seed)) & MASK64;
	}
	return hashStringToU64(seed);
}

class Splitmix64Rng implements SeededRng {
	private state: bigint;

	constructor(private readonly seed64: bigint) {
		this.state = seed64 & MASK64;
	}

	private nextU64(): bigint {
		this.state = (this.state + GAMMA) & MASK64;
		let z = this.state;
		z = ((z ^ (z >> 30n)) * MIX1) & MASK64;
		z = ((z ^ (z >> 27n)) * MIX2) & MASK64;
		return (z ^ (z >> 31n)) & MASK64;
	}

	next(): number {
		return Number(this.nextU64() >> 11n) / TWO_POW_53;
	}

	nextInt(maxExclusive: number): number {
		if (!Number.isInteger(maxExclusive) || maxExclusive <= 0) {
			throw new RangeError("nextInt requires a positive integer bound");
		}
		return Math.floor(this.next() * maxExclusive);
	}

	nextGaussian(): number {
		let u1 = this.next();
		if (u1 < MIN_POSITIVE_UNIT) u1 = MIN_POSITIVE_UNIT;
		const u2 = this.next();
		return Math.sqrt(-2 * Math.log(u1)) * Math.cos(2 * Math.PI * u2);
	}

	fork(label: string): SeededRng {
		const forkSeed = hashStringToU64(`${this.seed64.toString(16)}:${label}`);
		return new Splitmix64Rng(forkSeed);
	}
}

export function createSeededRng(seed: number | string): SeededRng {
	return new Splitmix64Rng(seedToU64(seed));
}
