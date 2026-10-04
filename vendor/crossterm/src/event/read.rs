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

/// The check's window lapsed: atomically take a verdict that landed in the
/// meantime, or stop watching (an unanswered query yields no verdict).
#[cfg(unix)]
pub(crate) fn lapse_capability_watch() -> Option<bool> {
    let mut replies = capability_replies();
    if replies.watch == ReplyWatch::Awaiting {
        replies.watch = ReplyWatch::Off;
    }
    replies.verdict.take()
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
                            if filter.wakes_on_capability_verdict() {
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
