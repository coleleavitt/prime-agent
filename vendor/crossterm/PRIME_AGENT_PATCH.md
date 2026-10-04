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
