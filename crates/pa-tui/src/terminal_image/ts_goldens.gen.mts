// The TS encoder goldens for terminal_image/ts_parity_tests.rs. Regenerate with:
//   git show v0.9.8:packages/tui/src/terminal-image.ts > terminal-image.ts
//   node ts_goldens.gen.mts > ts_goldens.json   (node >= 23: native type stripping)
import * as ti from "./terminal-image.ts";
const out: any = {};
const b64 = (n: number) => Buffer.from(Array.from({ length: n }, (_, i) => (i * 7 + 3) & 0xff)).toString("base64");
const small = b64(30);
const big = b64(7000); // 9336 base64 chars -> 3 chunks
out.kitty_small_plain = ti.encodeKitty(small);
out.kitty_small_opts = ti.encodeKitty(small, { columns: 60, rows: 12, imageId: 4242, moveCursor: false });
out.kitty_big_opts = ti.encodeKitty(big, { columns: 40, rows: 9, imageId: 7, moveCursor: false });
out.kitty_exact_4096 = ti.encodeKitty("A".repeat(4096), { imageId: 1 });
out.kitty_8192 = ti.encodeKitty("B".repeat(8192), { imageId: 2 });
out.iterm_plain = ti.encodeITerm2(small);
out.iterm_render = ti.encodeITerm2(small, { width: 60, height: "auto", preserveAspectRatio: true });
out.iterm_named = ti.encodeITerm2(small, { width: 10, height: 5, name: "shot é.png", preserveAspectRatio: false, inline: false });
out.delete_one = ti.deleteKittyImage(4242);
out.delete_all = ti.deleteAllKittyImages();
out.rows = [
  [ti.calculateImageRows({ widthPx: 1600, heightPx: 900 }, 60), 1600, 900, 60, 9, 18],
  [ti.calculateImageRows({ widthPx: 100, heightPx: 1 }, 60), 100, 1, 60, 9, 18],
  [ti.calculateImageRows({ widthPx: 800, heightPx: 1600 }, 56, { widthPx: 10, heightPx: 21 }), 800, 1600, 56, 10, 21],
  [ti.calculateImageRows({ widthPx: 1200, heightPx: 1200 }, 33, { widthPx: 17, heightPx: 37 }), 1200, 1200, 33, 17, 37],
];
const envKeys = ["TERM_PROGRAM", "TERM", "COLORTERM", "TMUX", "KITTY_WINDOW_ID", "GHOSTTY_RESOURCES_DIR", "WEZTERM_PANE", "ITERM_SESSION_ID"];
const cases: Record<string, string>[] = [
  {}, { KITTY_WINDOW_ID: "1" }, { TERM_PROGRAM: "kitty" }, { TERM_PROGRAM: "KITTY" }, { TERM_PROGRAM: "ghostty" },
  { TERM: "xterm-ghostty" }, { GHOSTTY_RESOURCES_DIR: "/x" }, { WEZTERM_PANE: "0" }, { TERM_PROGRAM: "WezTerm" },
  { ITERM_SESSION_ID: "w0" }, { TERM_PROGRAM: "iTerm.app" }, { TERM_PROGRAM: "vscode" }, { TERM_PROGRAM: "alacritty" },
  { KITTY_WINDOW_ID: "1", TMUX: "/tmp/tmux,1,0" }, { KITTY_WINDOW_ID: "1", TERM: "tmux-256color" },
  { ITERM_SESSION_ID: "w0", TERM: "screen-256color" }, { TERM_PROGRAM: "kitty", ITERM_SESSION_ID: "w0" },
  { KITTY_WINDOW_ID: "", TERM_PROGRAM: "" }, { TERM: "xterm-kitty" },
];
out.detect = cases.map((env) => {
  for (const k of envKeys) delete process.env[k];
  Object.assign(process.env, env);
  return [env, ti.detectCapabilities().images];
});
console.log(JSON.stringify(out, null, 1));
