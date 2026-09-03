/** An exact non-negative rational used for probability and error-budget values. */
export class Rational {
	readonly numerator: bigint;
	readonly denominator: bigint;

	private constructor(numerator: bigint, denominator: bigint) {
		this.numerator = numerator;
		this.denominator = denominator;
		Object.freeze(this);
	}

	static of(numerator: bigint | number | string, denominator: bigint | number | string = 1): Rational {
		let n = BigInt(numerator);
		let d = BigInt(denominator);
		if (d === 0n) throw new RangeError("rational denominator must be non-zero");
		if (d < 0n) {
			n = -n;
			d = -d;
		}
		const divisor = gcd(n < 0n ? -n : n, d);
		return new Rational(n / divisor, d / divisor);
	}

	static parse(value: string): Rational {
		if (!/^-?\d+(?:\/\d+)?$/.test(value)) throw new TypeError(`invalid rational: ${value}`);
		const [numerator, denominator = "1"] = value.split("/");
		return Rational.of(numerator, denominator);
	}

	add(other: Rational): Rational {
		return Rational.of(
			this.numerator * other.denominator + other.numerator * this.denominator,
			this.denominator * other.denominator,
		);
	}

	compare(other: Rational): number {
		const difference = this.numerator * other.denominator - other.numerator * this.denominator;
		return difference < 0n ? -1 : difference > 0n ? 1 : 0;
	}

	lte(other: Rational): boolean {
		return this.compare(other) <= 0;
	}
	isZero(): boolean {
		return this.numerator === 0n;
	}
	toString(): string {
		return `${this.numerator}/${this.denominator}`;
	}
	toJSON(): string {
		return this.toString();
	}
}

function gcd(left: bigint, right: bigint): bigint {
	while (right !== 0n) [left, right] = [right, left % right];
	return left === 0n ? 1n : left;
}

export function probability(value: Rational, name = "probability"): Rational {
	if (value.compare(Rational.of(0)) < 0 || value.compare(Rational.of(1)) > 0) {
		throw new RangeError(`${name} must be in [0, 1]`);
	}
	return value;
}
