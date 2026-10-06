# Crossterm 0.28.1 keyboard-probe timeout patch

Pinned source of crates.io `crossterm` 0.28.1 (MIT). The only upstream source change is `src/terminal/sys/unix.rs`: the kitty/DA1 support query's answer window. See benchmark diagnostic record `1c5af0f` (`kitty-ab-diagnostic-20260925`): unanswered kitty queries starved app key reads for two seconds, and the same fix class continues here.

The window bound is 250ms (instead of upstream's 2000ms), held in 10ms poll slices instead of one blocking hold: each slice parks the process-global event-reader lock for at most a slice, so the app reader interleaves and delivers input typed during the probe at its own cadence, while the probe learns its reply's verdict from the reply watch (see "The reply watch" below), whichever poller parsed the reply. A slice timeout yields to the app reader and re-polls until the 250ms deadline — the settle time, the answer contract, the response filtering, and the queued user keys are unchanged; only the lock-hold shape changes. A poll error retries inside the same window and settles at the deadline (upstream retried without a bound, parking the lock 250ms at a time on a broken tty). Answering terminals still resolve immediately (a DA1 reply alone settles no-kitty early — the flags filter matches the primary-device-attributes reply too); a silent PTY settles at 250ms. Responses after the deadline still conclude the query through the reply watch (see "The reply watch"). Prefer upstreaming a configurable bounded-query API (ideally with lock-free slices) to crossterm before removing this vendor patch.

# The reply watch (2026-10-04, pty-flake root cause)

`src/event/read.rs` (`ReplyWatch`, `observe_capability_reply`), `src/event/filter.rs`
(`Filter::wakes_on_capability_verdict`), and the support check in
`src/terminal/sys/unix.rs`. The check arms the watch BEFORE it writes `CSI ? u` +
`CSI c`; from then on whichever poller parses the reply — the check's own slice, or
the app's input reader polling the same shared reader — publishes the verdict (a flags
reply: supported; a DA1 reply first: unsupported — the kitty detection contract,
<https://sw.kovidgoyal.net/kitty/keyboard-protocol/#detection-of-support-for-this-protocol>),
and the check reads the verdict between its slices without needing the reader lock.
A flags reply's trailing DA1 is consumed by the watch (upstream flushed it with a
blocking read).

A lapsed window is no verdict (the kitty contract has no deadline): the watch stays
armed after the check's no-answer error, a reply that arrives later still publishes its
verdict, and `event::take_late_keyboard_enhancement_reply()` (additive export) hands it
to the caller once; a parked `poll_opt(None)` returns `Ok(false)` when it lands (bounded
pollers keep their timeouts). The product's input reader takes it and pushes the flags
at the late answer — the old "upgrade cliff" at the deadline is gone; the DA1-first
early conclusion is unchanged.

The bug it fixes (measured): the check used to find its reply only by winning the
process-global event-reader lock and reading the reply out of the shared queue. The app
reader keeps 10ms bounded polls through the window and re-takes the lock within
microseconds of each release; under CPU contention the check's slice waits lost every
round. Lock-side fixes did not hold under the same load: a fair (handoff) release alone
still failed 44/48 runs, and a fair release plus a window-long lock wait 22/48 — the
check must not depend on winning the lock at all. Traced on a loaded pty: the kitty reply was parsed by the APP reader
at +35..76ms, parked in the queue, and the check's single lock attempt waited out the
whole 250ms window — the terminal was then classified "no kitty" for the process. Under
16 concurrent runs on one CPU with 4 busy loops, the kitty-control e2e failed 45/48 runs
before and 0/48 after.

# Additive exports for the edge-driven app reader (2026-09-28, wave-6 tui-wake-thread)

The `event-stream` feature's waker machinery (upstream, unchanged: `WAKE_TOKEN`,
the `Waker` type, `InternalEventReader::waker`) compiles in for the product
(`crates/pa-tui` enables the feature), and three additive exports make it
usable by the app's single input-reader thread:

* `event::poll_opt(timeout: Option<Duration>)` — the existing `poll` with the
  `None` park that `InternalEventReader::poll` already supported internally:
  park until real tty/signal input, or a waker wake, which the reader maps to
  `Ok(false)` exactly like a timeout.
* `event::waker() -> Option<Waker>` — the process-global source's wake handle,
  `None` when the source failed to initialize (no controlling tty: parking is
  unsafe then — the vendored contract makes the caller keep a bounded poll).
* `pub struct Waker` / `pub fn Waker::wake` / the `pub use` chain
  (`event::sys`, `event::sys::unix::waker::mio`, `event::sys::windows::waker`)
  — visibility bumps only; the type was already `pub(crate)` under the same
  feature gate, and `new` stays crate-private.
* `InternalEventReader::try_waker` — the non-panicking `waker()` variant
  (`Option` instead of `.expect("reader source not set")`).

The app reader (`crates/pa-tui/src/input.rs`) parks on `poll_opt(None)` when its
sequence guard holds nothing, keeps the 10ms bounded poll while the kitty-probe
window is open (`pa_tui::enhanced_keys::query_in_flight`) — the probe's slices
and the reader share the process-global event-reader lock, and a park would
starve them — and its stop flag is now observed through `Waker::wake()` at
teardown instead of a poll tick. Behavior on the wire (events, parse order,
chunk boundaries, handoff) is unchanged; only the idle wait's wakeup rate is.

# The verdict time per terminal class (2026-09-29, kitty-verdict-time lane)

The probe's conclusion time was characterized on a real pty per class
(`kitty_verdict_time_e2e`'s sweep mode, VM feq0mhg7yk19ycrk2ytuhuww at the
tip; rows in the lane's record):

* A kitty terminal concludes at its flags reply (push at answer+~6ms).
* A DA1-answering non-kitty terminal — the COMMON non-kitty class; real
  tmux 3.2a answers DA1 in 15-24us and never answers the flags query
  (1001ms silence, 10/10); real screen 4.09 answers in 15-33us —
  concludes at the DA1 arrival (the window measured CLOSED from +20ms
  with the answer at +15ms; a flags reply at +20..+80ms never upgrades),
  so a raced mode transition lands with its dispatch (offset+2ms) instead
  of the deadline.
* A fully-silent pty (no DA1 ever — CI harnesses) is the only class that
  pays the deadline: the raced teardown pins at 249-251ms at every
  in-window offset, and the upgrade cliff sat at 240-250ms (since the
  reply watch, 2026-10-04: no cliff — a later flags reply upgrades at its
  arrival).

The 250ms bound was therefore TWO contracts at once: the silent class's
verdict bound, and the late-kitty catch window (since the reply watch the
bound is only the first contract; a reply past it still upgrades) — a kitty terminal over a
slow hop answers its flags at RTT (this fleet's own single public hop
measures 24-29ms; the intercontinental SSH classes ride 80-250ms),
inside today's window and outside any 50ms cut. A flat deadline cut is
rejected on that misclassification distribution: it would silently drop
the enhancement for the RTT>50ms class (the product's primary remote-SSH
deployment shape) while buying only the silent class's raced-transition
stall, which no user rides. The timed contract is locked by
`crates/pa-cli/tests/kitty_verdict_time_e2e.rs`.

# xterm modifyOtherKeys sequences (2026-10-05, #1305 word-delete chords)

`src/event/sys/unix/parse.rs` (`parse_csi_special_key_code`): `CSI 27 ; <modifiers> ;
<codepoint> ~` (xterm modifyOtherKeys, also tmux `extended-keys` in its xterm format) parses
as the CSI-u key `CSI <codepoint> ; <modifiers> u` — Ctrl+Backspace `CSI 27;5;127~` is
Backspace+CONTROL, Ctrl+Enter `CSI 27;5;13~` is Enter+CONTROL. Upstream has no case for
the form, and its parse error clears the whole pending input buffer, so every key typed with
it vanished. The product still resets mode 2 at each surface start; this covers a terminal
that sends the form anyway. Pinned by `test_parse_csi_modify_other_keys` (the vendored crate is
outside the workspace: run it from a scratch copy with an empty `[workspace]` table).

# Kitty-printable twin dedup (2026-10-05, upstream #3341 / issue #3250)

`src/event/sys/unix/parse.rs` (`KittyPrintableTwin`) and the `Parser::advance` loops in
`src/event/source/unix/mio.rs` and `tty.rs`. A duplicate-reporting terminal sends both an
unmodified `CSI <cp> u` and the raw character for one printable keypress (TS #3780). Parsed,
both are the same unmodified `Char` press, so the product's event-layer equality guess also ate
real raw pairs (dictation "will" typed "wil", IME commits, batched key repeat). The dedup now
runs on each completed sequence's bytes, as TS `StdinBuffer` does
(`pendingKittyPrintableCodepoint`): an unmodified CSI-u report for a codepoint >= 32 (the TS
regex shape `CSI \d+ (:\d*)? (:\d+)? u` — alternate-key sections, no modifier field) arms the
pending; a raw sequence that is exactly that character is dropped; every other sequence and
every parse failure clears it. The state is parser state, so a twin split across reads still
drops. Windows reads structured records and needs nothing. Pinned by
`test_kitty_printable_twin_drops_only_after_a_csi_u_report` and
`test_kitty_modified_or_control_reports_never_arm_the_twin` (scratch-copy run, see above).

# The kitty graphics query (2026-10-06, inline images over ssh)

`src/terminal/sys/unix.rs` (`request_kitty_graphics_query`, `GRAPHICS_QUERY`),
`src/event/read.rs` (`arm_graphics_watch`, `observe_graphics_reply`,
`take_graphics_verdict`), `src/event/sys/unix/parse.rs` (`parse_kitty_graphics_reply`),
`src/event.rs` (`InternalEvent::KittyGraphicsReply`, `take_kitty_graphics_reply`), and the
`terminal.rs` re-export. Additive: nothing changes unless a caller requests the query.

An ssh session keeps only `TERM` (openssh forwards no environment by default), so the
product cannot tell a kitty-protocol terminal from its variables. A caller that wants to know
calls `terminal::request_kitty_graphics_query()` before the keyboard support check; the
check then writes the graphics protocol's documented detection query
(`ESC _ G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC \`) ahead of its `CSI ? u` + `CSI c`,
arming a second watch first. The terminal answers in order, so a graphics reply lands before
the DA1 that every terminal sends: `i=31;OK` publishes `Some(true)`, an error reply or a DA1
that arrives first publishes `Some(false)` (the DA1 still reaches the keyboard watch
unchanged). The caller reads it once with `event::take_kitty_graphics_reply()`.

While the query is outstanding (armed until its reply or the concluding DA1) the parser reads
`ESC _ G … ESC \` as one `KittyGraphicsReply` (at most 1 KiB) instead of upstream's Alt+`_`
followed by the reply's bytes as typed keys; the watch consumes every such reply (it never
queues). A lone `ESC _` at the end of a read stays Alt+`_`; `ESC _` followed by anything but
`G` in the same read is a parse error while armed (the window is one terminal round trip).
Outside the window `ESC _` is upstream's Alt+`_`. Pinned by
`test_parse_kitty_graphics_reply_only_while_expected` and
`graphics_replies_conclude_the_watch_and_never_queue` (scratch-copy run, see above; the copy
also needs the `[[example]]` tables removed), and end to end by pa-cli's
`image_graphics_query_e2e`.
