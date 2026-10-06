use std::{collections::vec_deque::VecDeque, io, time::Duration};

#[cfg(unix)]
use crate::event::source::unix::UnixInternalEventSource;
#[cfg(windows)]
use crate::event::source::windows::WindowsEventSource;
#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{filter::Filter, source::EventSource, timeout::PollTimeout, InternalEvent};

/// Prime Agent patch: the keyboard-enhancement reply watch.
///
/// The support check (`CSI ? u` + `CSI c`) arms the watch BEFORE it writes
/// the query, and from then on whichever poller parses the reply publishes
/// the verdict here: every caller reads the tty through the one shared
/// reader, and the app's input reader polls it throughout the check's
/// window. The check used to find the reply only by taking the reader lock
/// itself and reading it out of the shared queue; under CPU contention its
/// slices lost every round of the lock to the app reader (whose 10ms polls
/// re-take it within microseconds), so a reply that arrived and was parsed
/// in time sat in the queue while the check's window lapsed — and the
/// terminal was classified "no kitty support" for the rest of the process.
/// With the watch the check only reads the verdict, which needs no reader
/// lock.
///
/// A lapsed window is no verdict either: the query stays watched after the
/// check returns its no-answer error, and a reply that arrives later still
/// publishes its verdict, for the caller to take through
/// [`crate::event::take_late_keyboard_enhancement_reply`] (a parked
/// unbounded poll wakes when it lands). The kitty contract has no deadline;
/// a slow hop or a loaded host answers late, not never.
///
/// The verdict follows the kitty detection contract: the terminal answers
/// the flags query before the device-attributes query, so a flags reply
/// means "supported", and a DA1 reply that arrives first means
/// "unsupported" (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/#detection-of-support-for-this-protocol>).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyWatch {
    /// No query outstanding: replies take the normal filtered path.
    Off,
    /// The query is out: the first flags or DA1 reply is the verdict.
    Awaiting,
    /// A flags reply concluded the query: its trailing DA1 reply is consumed
    /// instead of parking in the shared queue forever.
    SwallowDa1,
}

#[cfg(unix)]
struct CapabilityReplies {
    watch: ReplyWatch,
    verdict: Option<bool>,
}

#[cfg(unix)]
static CAPABILITY_REPLIES: std::sync::Mutex<CapabilityReplies> =
    std::sync::Mutex::new(CapabilityReplies {
        watch: ReplyWatch::Off,
        verdict: None,
    });

#[cfg(unix)]
fn capability_replies() -> std::sync::MutexGuard<'static, CapabilityReplies> {
    CAPABILITY_REPLIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Arm the watch for a query about to be written (clears a stale verdict).
#[cfg(unix)]
pub(crate) fn arm_capability_watch() {
    let mut replies = capability_replies();
    replies.watch = ReplyWatch::Awaiting;
    replies.verdict = None;
}

/// Take the published verdict: `Some(true)` for a flags reply, `Some(false)`
/// for a DA1 reply that arrived first. Each verdict is returned once.
#[cfg(unix)]
pub(crate) fn take_capability_verdict() -> Option<bool> {
    capability_replies().verdict.take()
}

/// The check's window lapsed: take a verdict that landed in the meantime.
/// With none, the watch stays armed — the late reply's verdict is published
/// for [`crate::event::take_late_keyboard_enhancement_reply`].
#[cfg(unix)]
pub(crate) fn lapse_capability_watch() -> Option<bool> {
    capability_replies().verdict.take()
}

/// Prime Agent patch: the kitty graphics query watch.
///
/// The keyboard support check can carry the kitty graphics query
/// (`ESC _ G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC \`, the protocol's
/// documented detection) ahead of its own `CSI ? u` + `CSI c`, for a caller
/// whose terminal hides its name (an ssh session keeps only `TERM`). The
/// terminal answers in order, so a graphics reply lands before the DA1
/// reply that answers every terminal: a reply means the protocol works
/// (`OK`) or not (an error), and a DA1 reply first means no graphics
/// protocol. While the query is outstanding the parser reads `ESC _ G … ESC
/// \` as one reply (upstream reads `ESC _` as Alt+`_` and leaks the rest as
/// keys); the reply is always consumed, never queued.
#[cfg(unix)]
static GRAPHICS_REPLY_EXPECTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
static GRAPHICS_VERDICT: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

#[cfg(unix)]
fn graphics_verdict() -> std::sync::MutexGuard<'static, Option<bool>> {
    GRAPHICS_VERDICT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Arm the graphics watch for a query about to be written.
#[cfg(unix)]
pub(crate) fn arm_graphics_watch() {
    *graphics_verdict() = None;
    GRAPHICS_REPLY_EXPECTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Whether a graphics reply is outstanding (the parser's `ESC _ G` gate).
#[cfg(unix)]
pub(crate) fn graphics_reply_expected() -> bool {
    GRAPHICS_REPLY_EXPECTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Serializes the tests that drive the process-global graphics watch.
#[cfg(all(unix, test))]
pub(crate) static GRAPHICS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(unix, test))]
pub(crate) fn disarm_graphics_watch_for_tests() {
    GRAPHICS_REPLY_EXPECTED.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// Take the graphics verdict (each is returned once).
#[cfg(unix)]
pub(crate) fn take_graphics_verdict() -> Option<bool> {
    graphics_verdict().take()
}

/// The query's id (the protocol's documented example).
#[cfg(unix)]
pub(crate) const GRAPHICS_QUERY_ID: &str = "31";

/// Whether a reply body (`i=31;OK`) says the query's image loaded.
#[cfg(unix)]
fn graphics_reply_ok(body: &str) -> bool {
    let (keys, message) = body.split_once(';').unwrap_or((body, ""));
    keys.split(',')
        .any(|key| key.strip_prefix("i=") == Some(GRAPHICS_QUERY_ID))
        && message == "OK"
}

/// Route a graphics reply, or the DA1 that concludes an unanswered query.
/// `true` when the event was a graphics reply (consumed).
#[cfg(unix)]
fn observe_graphics_reply(event: &InternalEvent) -> bool {
    use std::sync::atomic::Ordering;
    match event {
        InternalEvent::KittyGraphicsReply(body) => {
            if GRAPHICS_REPLY_EXPECTED.swap(false, Ordering::SeqCst) {
                *graphics_verdict() = Some(graphics_reply_ok(body));
            }
            true
        }
        InternalEvent::PrimaryDeviceAttributes => {
            if GRAPHICS_REPLY_EXPECTED.swap(false, Ordering::SeqCst) {
                *graphics_verdict() = Some(false);
            }
            false
        }
        _ => false,
    }
}

/// What [`observe_capability_reply`] did with one parsed event.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchedReply {
    /// Not a watched reply: the event takes the normal filtered path.
    Unwatched,
    /// The event concluded the outstanding query (the verdict is published).
    Verdict,
    /// The trailing DA1 of a query a flags reply concluded: consumed.
    Swallowed,
}

/// Route one parsed event through the reply watch.
#[cfg(unix)]
fn observe_capability_reply(event: &InternalEvent) -> WatchedReply {
    if observe_graphics_reply(event) {
        return WatchedReply::Swallowed;
    }
    let flags = match event {
        InternalEvent::KeyboardEnhancementFlags(_) => true,
        InternalEvent::PrimaryDeviceAttributes => false,
        _ => return WatchedReply::Unwatched,
    };
    let mut replies = capability_replies();
    match (replies.watch, flags) {
        (ReplyWatch::Awaiting, true) => {
            replies.watch = ReplyWatch::SwallowDa1;
            replies.verdict = Some(true);
            WatchedReply::Verdict
        }
        (ReplyWatch::Awaiting, false) => {
            replies.watch = ReplyWatch::Off;
            replies.verdict = Some(false);
            WatchedReply::Verdict
        }
        (ReplyWatch::SwallowDa1, false) => {
            replies.watch = ReplyWatch::Off;
            WatchedReply::Swallowed
        }
        _ => WatchedReply::Unwatched,
    }
}

/// Can be used to read `InternalEvent`s.
pub(crate) struct InternalEventReader {
    events: VecDeque<InternalEvent>,
    source: Option<Box<dyn EventSource>>,
    skipped_events: Vec<InternalEvent>,
}

impl Default for InternalEventReader {
    fn default() -> Self {
        #[cfg(windows)]
        let source = WindowsEventSource::new();
        #[cfg(unix)]
        let source = UnixInternalEventSource::new();

        let source = source.ok().map(|x| Box::new(x) as Box<dyn EventSource>);

        InternalEventReader {
            source,
            events: VecDeque::with_capacity(32),
            skipped_events: Vec::with_capacity(32),
        }
    }
}

impl InternalEventReader {
    /// Returns a `Waker` allowing to wake/force the `poll` method to return `Ok(false)`.
    #[cfg(feature = "event-stream")]
    pub(crate) fn waker(&self) -> Waker {
        self.source.as_ref().expect("reader source not set").waker()
    }

    /// The source's wake handle when the source initialized: `None` on a
    /// source that failed to open (a process without a controlling tty) —
    /// a parking caller must keep a bounded poll in that case.
    #[cfg(feature = "event-stream")]
    pub(crate) fn try_waker(&self) -> Option<Waker> {
        self.source.as_ref().map(|source| source.waker())
    }

    pub(crate) fn poll<F>(&mut self, timeout: Option<Duration>, filter: &F) -> io::Result<bool>
    where
        F: Filter,
    {
        for event in &self.events {
            if filter.eval(event) {
                return Ok(true);
            }
        }

        let event_source = match self.source.as_mut() {
            Some(source) => source,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to initialize input reader",
                ))
            }
        };

        let poll_timeout = PollTimeout::new(timeout);

        loop {
            let maybe_event = match event_source.try_read(poll_timeout.leftover()) {
                Ok(None) => None,
                Ok(Some(event)) => {
                    #[cfg(unix)]
                    match observe_capability_reply(&event) {
                        WatchedReply::Unwatched => {}
                        WatchedReply::Swallowed => continue,
                        WatchedReply::Verdict => {
                            // A parked (unbounded) poller wakes too, like a
                            // waker wake, so an edge-driven reader can take a
                            // late verdict at once; bounded pollers keep their
                            // timeouts (a drain must not end on a reply).
                            if timeout.is_none() || filter.wakes_on_capability_verdict() {
                                self.events.extend(self.skipped_events.drain(..));
                                return Ok(false);
                            }
                            continue;
                        }
                    }
                    if filter.eval(&event) {
                        Some(event)
                    } else {
                        self.skipped_events.push(event);
                        None
                    }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        return Ok(false);
                    }

                    return Err(e);
                }
            };

            if poll_timeout.elapsed() || maybe_event.is_some() {
                self.events.extend(self.skipped_events.drain(..));

                if let Some(event) = maybe_event {
                    self.events.push_front(event);
                    return Ok(true);
                }

                return Ok(false);
            }
        }
    }

    pub(crate) fn read<F>(&mut self, filter: &F) -> io::Result<InternalEvent>
    where
        F: Filter,
    {
        let mut skipped_events = VecDeque::new();

        loop {
            while let Some(event) = self.events.pop_front() {
                if filter.eval(&event) {
                    while let Some(event) = skipped_events.pop_front() {
                        self.events.push_back(event);
                    }

                    return Ok(event);
                } else {
                    // We can not directly write events back to `self.events`.
                    // If we did, we would put our self's into an endless loop
                    // that would enqueue -> dequeue -> enqueue etc.
                    // This happens because `poll` in this function will always return true if there are events in it's.
                    // And because we just put the non-fulfilling event there this is going to be the case.
                    // Instead we can store them into the temporary buffer,
                    // and then when the filter is fulfilled write all events back in order.
                    skipped_events.push_back(event);
                }
            }

            let _ = self.poll(None, filter)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::{collections::VecDeque, time::Duration};

    #[cfg(unix)]
    use super::super::filter::CursorPositionFilter;
    use super::{
        super::{filter::InternalEventFilter, Event},
        EventSource, InternalEvent, InternalEventReader,
    };

    /// Prime Agent patch: the graphics watch's verdicts. A reply is
    /// consumed whatever it says; `OK` for the query's id is the only yes;
    /// a DA1 that arrives first is no, and still reaches the keyboard watch.
    #[cfg(unix)]
    #[test]
    fn graphics_replies_conclude_the_watch_and_never_queue() {
        use super::{
            arm_capability_watch, arm_graphics_watch, graphics_reply_expected,
            observe_capability_reply, take_capability_verdict, take_graphics_verdict,
            WatchedReply, GRAPHICS_TEST_LOCK,
        };
        let _guard = GRAPHICS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reply = |body: &str| InternalEvent::KittyGraphicsReply(body.to_string());
        arm_graphics_watch();
        assert_eq!(observe_capability_reply(&reply("i=31;OK")), WatchedReply::Swallowed);
        assert!(!graphics_reply_expected());
        assert_eq!(take_graphics_verdict(), Some(true));
        assert_eq!(take_graphics_verdict(), None);
        arm_graphics_watch();
        assert_eq!(
            observe_capability_reply(&reply("i=31;ENOTSUPPORTED:no")),
            WatchedReply::Swallowed
        );
        assert_eq!(take_graphics_verdict(), Some(false));
        // A stray reply with nothing outstanding is consumed, no verdict.
        assert_eq!(observe_capability_reply(&reply("i=7;OK")), WatchedReply::Swallowed);
        assert_eq!(take_graphics_verdict(), None);
        // DA1 first: no graphics; the keyboard watch still takes the DA1.
        arm_graphics_watch();
        arm_capability_watch();
        assert_eq!(
            observe_capability_reply(&InternalEvent::PrimaryDeviceAttributes),
            WatchedReply::Verdict
        );
        assert_eq!(take_graphics_verdict(), Some(false));
        assert_eq!(take_capability_verdict(), Some(false));
    }

    #[test]
    fn test_poll_fails_without_event_source() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(10)), &InternalEventFilter)
            .is_err());
    }

    #[test]
    fn test_poll_returns_true_for_matching_event_in_queue_at_front() {
        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10))].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn test_poll_returns_true_for_matching_event_in_queue_at_back() {
        let mut reader = InternalEventReader {
            events: vec![
                InternalEvent::Event(Event::Resize(10, 10)),
                InternalEvent::CursorPosition(10, 20),
            ]
            .into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &CursorPositionFilter).unwrap());
    }

    #[test]
    fn test_read_returns_matching_event_in_queue_at_front() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let mut reader = InternalEventReader {
            events: vec![EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_read_returns_matching_event_in_queue_at_back() {
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10)), CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_read_does_not_consume_skipped_event() {
        const SKIPPED_EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![SKIPPED_EVENT, CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), SKIPPED_EVENT);
    }

    #[test]
    fn test_poll_timeouts_if_source_has_no_events() {
        let source = FakeSource::default();

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert!(!reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_poll_returns_true_if_source_has_at_least_one_event() {
        let source = FakeSource::with_events(&[InternalEvent::Event(Event::Resize(10, 10))]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_reads_returns_event_if_source_has_at_least_one_event() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_read_returns_events_if_source_has_events() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_poll_returns_false_after_all_source_events_are_consumed() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(!reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_poll_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .err()
                .map(|e| format!("{:?}", &e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_read_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .read(&InternalEventFilter)
                .err()
                .map(|e| format!("{:?}", &e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_poll_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_read_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[derive(Default)]
    struct FakeSource {
        events: VecDeque<InternalEvent>,
        error: Option<io::Error>,
    }

    impl FakeSource {
        fn new(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: Some(io::Error::new(io::ErrorKind::Other, "")),
            }
        }

        fn with_events(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: None,
            }
        }
    }

    impl EventSource for FakeSource {
        fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
            // Return error if set in case there's just one remaining event
            if self.events.len() == 1 {
                if let Some(error) = self.error.take() {
                    return Err(error);
                }
            }

            // Return all events from the queue
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }

            // Return error if there're no more events
            if let Some(error) = self.error.take() {
                return Err(error);
            }

            // Timeout
            Ok(None)
        }

        #[cfg(feature = "event-stream")]
        fn waker(&self) -> super::super::sys::Waker {
            unimplemented!();
        }
    }
}
