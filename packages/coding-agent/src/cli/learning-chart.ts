/**
 * A dependency-free ASCII plot for `prime-agent learning`. Two series over the
 * same day axis, drawn on a fixed grid so the output diffs cleanly and works
 * over a pipe. When there are more days than columns the days are bucketed and
 * averaged, so the axis never wraps.
 */

export interface ChartSeries {
	label: string;
	/** Single character drawn for this series; two series on one cell draw `*`. */
	mark: string;
	points: ReadonlyArray<number | undefined>;
}

export interface ChartOptions {
	height?: number;
	width?: number;
	/** One label per point; the first, the marked column, and the last are printed. */
	xLabels?: readonly string[];
	/** Index of a point to mark on the axis (the pivot). */
	markerIndex?: number;
	valueLabel?: string;
}

const DEFAULT_HEIGHT = 9;
const DEFAULT_WIDTH = 56;
const GUTTER = 7;
const BOTH_MARK = "*";
const EMPTY_CHART = "(no data to chart)";

function bucket(points: ReadonlyArray<number | undefined>, columns: number): Array<number | undefined> {
	if (points.length <= columns) return [...points];
	const out: Array<number | undefined> = [];
	for (let column = 0; column < columns; column++) {
		const start = Math.floor((column * points.length) / columns);
		const end = Math.max(start + 1, Math.floor(((column + 1) * points.length) / columns));
		const values = points.slice(start, end).filter((value): value is number => typeof value === "number");
		out.push(values.length === 0 ? undefined : values.reduce((sum, value) => sum + value, 0) / values.length);
	}
	return out;
}

function bucketIndex(index: number, length: number, columns: number): number {
	if (length <= columns) return index;
	return Math.min(columns - 1, Math.floor((index * columns) / length));
}

function formatAxisValue(value: number): string {
	const text = Math.abs(value) >= 100 ? value.toFixed(0) : value.toFixed(2);
	return text.padStart(GUTTER - 2);
}

export function renderAsciiChart(series: readonly ChartSeries[], options: ChartOptions = {}): string[] {
	const height = Math.max(3, Math.trunc(options.height ?? DEFAULT_HEIGHT));
	const length = series.reduce((longest, entry) => Math.max(longest, entry.points.length), 0);
	if (length === 0 || series.length === 0) return [EMPTY_CHART];
	const columns = Math.max(8, Math.min(Math.trunc(options.width ?? DEFAULT_WIDTH), length));
	const bucketed = series.map((entry) => ({ ...entry, points: bucket(entry.points, columns) }));
	const values = bucketed.flatMap((entry) =>
		entry.points.filter((value): value is number => typeof value === "number"),
	);
	if (values.length === 0) return [EMPTY_CHART];

	const max = Math.max(...values);
	const min = Math.min(0, Math.min(...values));
	const span = max - min || 1;
	const grid: string[][] = Array.from({ length: height }, () => Array.from({ length: columns }, () => " "));
	for (const entry of bucketed) {
		for (let column = 0; column < entry.points.length; column++) {
			const value = entry.points[column];
			if (typeof value !== "number") continue;
			const row = Math.min(height - 1, Math.max(0, Math.round(((max - value) / span) * (height - 1))));
			const cell = grid[row]![column]!;
			grid[row]![column] = cell === " " ? entry.mark : cell === entry.mark ? cell : BOTH_MARK;
		}
	}

	const lines: string[] = [];
	if (options.valueLabel) lines.push(`${" ".repeat(GUTTER)}${options.valueLabel}`);
	for (let row = 0; row < height; row++) {
		const label =
			row === 0 ? formatAxisValue(max) : row === height - 1 ? formatAxisValue(min) : " ".repeat(GUTTER - 2);
		lines.push(`${label} |${grid[row]!.join("")}`);
	}
	lines.push(`${" ".repeat(GUTTER - 2)} +${"-".repeat(columns)}`);

	const axis = Array.from({ length: columns }, () => " ");
	if (options.markerIndex !== undefined && options.markerIndex >= 0 && options.markerIndex < length) {
		axis[bucketIndex(options.markerIndex, length, columns)] = "^";
	}
	if (axis.some((cell) => cell !== " ")) lines.push(`${" ".repeat(GUTTER)}${axis.join("")}`);

	const labels = options.xLabels ?? [];
	const first = labels[0];
	const last = labels[labels.length - 1];
	if (first && last) {
		const gap = Math.max(1, columns - first.length - last.length);
		lines.push(`${" ".repeat(GUTTER)}${first}${" ".repeat(gap)}${last}`);
	}
	lines.push(
		`${" ".repeat(GUTTER)}${bucketed.map((entry) => `${entry.mark} ${entry.label}`).join("   ")}   ${BOTH_MARK} both`,
	);
	return lines.map((line) => line.trimEnd());
}
