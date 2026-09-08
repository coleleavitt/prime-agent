import { appendRotatingLog } from "../../src/config.js";

const [logPath, writer, countValue] = process.argv.slice(2);
const count = Number(countValue);
if (!logPath || !writer || !Number.isInteger(count)) throw new Error("invalid writer fixture arguments");
for (let index = 0; index < count; index++) {
	appendRotatingLog(logPath, JSON.stringify({ writer, index }), 1, 100);
}
