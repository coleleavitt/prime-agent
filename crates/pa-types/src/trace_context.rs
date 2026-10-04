//! W3C trace context (`traceparent`) shared by every process boundary.
//!
//! One `traceparent` (`00-<traceId>-<spanId>-<flags>`) names the span that is
//! active where a request, frame, or child process starts, so a user turn can
//! be followed across the CLI, daemon workers, and the Python kernel with one
//! trace id. This module owns only the vocabulary: the value type, its strict
//! parse/format, and the conventions native code uses to hand context across
//! a boundary. Native crates never mint ids or record spans themselves; they
//! emit plain `tracing` spans, and whatever subscriber the binary installs
//! assigns the ids and answers [`current`]. With no source installed every
//! query answers `None`, so the carriers stay absent and the wire is
//! unchanged.

use std::fmt;
use std::sync::OnceLock;

/// The environment carrier: set on child processes, read once at startup.
pub const TRACEPARENT_ENV: &str = "TRACEPARENT";

/// The JSON field carrying the context on frames (kernel requests, daemon
/// envelopes, host requests).
pub const TRACEPARENT_FIELD: &str = "traceparent";

/// `tracing` target of a record another process produced in the trace log
/// shape (a finished span or a diagnostic line with its own `traceId` /
/// `spanId`), forwarded verbatim by the host that received it. The record is
/// the JSON object text in the [`FORWARDED_RECORD_FIELD`] field.
pub const FORWARDED_RECORD_TARGET: &str = "trace_context::forwarded";

/// The event field holding a forwarded record's JSON object text.
pub const FORWARDED_RECORD_FIELD: &str = "record";

/// A span field naming a remote parent: a span that carries
/// `traceparent = <W3C value>` becomes a child of that context instead of the
/// span that is current where it opens (a daemon command handled for a client
/// that sent its own context, say).
pub const REMOTE_PARENT_FIELD: &str = "traceparent";

/// One propagated context: the ids of the span that is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceContext {
    /// 128-bit trace id; never zero.
    pub trace_id: u128,
    /// 64-bit span id; never zero.
    pub span_id: u64,
    /// Trace flags; `0x01` is sampled.
    pub flags: u8,
}

impl TraceContext {
    /// The flags value every locally minted context carries (sampled).
    pub const SAMPLED: u8 = 0x01;

    /// Strict W3C parse: version `00`, lowercase hex of the exact lengths,
    /// non-zero ids. Surrounding whitespace is ignored; anything else answers
    /// `None` so untrusted fields can be passed straight in.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.trim().split('-');
        let (Some(version), Some(trace_id), Some(span_id), Some(flags), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return None;
        };
        if version != "00" {
            return None;
        }
        let trace_id = parse_lower_hex(trace_id, 32).and_then(|id| (id != 0).then_some(id))?;
        let span_id = parse_lower_hex(span_id, 16)
            .and_then(|id| u64::try_from(id).ok())
            .filter(|id| *id != 0)?;
        let flags = parse_lower_hex(flags, 2).and_then(|f| u8::try_from(f).ok())?;
        Some(TraceContext {
            trace_id,
            span_id,
            flags,
        })
    }

    /// The trace id as 32 lowercase hex characters.
    #[must_use]
    pub fn trace_id_hex(&self) -> String {
        format_trace_id(self.trace_id)
    }

    /// The span id as 16 lowercase hex characters.
    #[must_use]
    pub fn span_id_hex(&self) -> String {
        format_span_id(self.span_id)
    }
}

/// `00-<traceId>-<spanId>-<flags>`.
impl fmt::Display for TraceContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "00-{:032x}-{:016x}-{:02x}",
            self.trace_id, self.span_id, self.flags
        )
    }
}

/// A trace id as 32 lowercase hex characters.
#[must_use]
pub fn format_trace_id(trace_id: u128) -> String {
    format!("{trace_id:032x}")
}

/// A span id as 16 lowercase hex characters.
#[must_use]
pub fn format_span_id(span_id: u64) -> String {
    format!("{span_id:016x}")
}

/// Parse exactly `len` lowercase hex digits (uppercase is rejected, as W3C
/// requires).
fn parse_lower_hex(text: &str, len: usize) -> Option<u128> {
    if text.len() != len
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    u128::from_str_radix(text, 16).ok()
}

/// Answers the context active on the calling thread/task.
pub type CurrentContextSource = fn() -> Option<TraceContext>;

static CURRENT_SOURCE: OnceLock<CurrentContextSource> = OnceLock::new();

/// Install the process-wide answer to [`current`]. The subscriber that
/// assigns span ids installs it once at startup; later calls are ignored and
/// answer `false`.
pub fn set_current_context_source(source: CurrentContextSource) -> bool {
    CURRENT_SOURCE.set(source).is_ok()
}

/// The context of the innermost active span, when a source is installed.
#[must_use]
pub fn current() -> Option<TraceContext> {
    CURRENT_SOURCE.get().and_then(|source| source())
}

/// [`current`] formatted for a frame field or the [`TRACEPARENT_ENV`]
/// variable.
#[must_use]
pub fn current_traceparent() -> Option<String> {
    current().map(|context| context.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    #[test]
    fn parses_and_formats_a_valid_traceparent() {
        let context = TraceContext::parse(VALID).expect("valid");
        assert_eq!(
            context,
            TraceContext {
                trace_id: 0x0af7_6519_16cd_43dd_8448_eb21_1c80_319c,
                span_id: 0xb7ad_6b71_6920_3331,
                flags: 1,
            }
        );
        assert_eq!(context.to_string(), VALID);
        assert_eq!(context.trace_id_hex(), "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(context.span_id_hex(), "b7ad6b7169203331");
        assert_eq!(TraceContext::parse(&format!("  {VALID}\n")), Some(context));
    }

    #[test]
    fn rejects_malformed_values() {
        for value in [
            "",
            "00-garbage",
            "01-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            "00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01",
            "00-00000000000000000000000000000000-b7ad6b7169203331-01",
            "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-1",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01-extra",
            "00-0af7651916cd43dd8448eb211c80319-b7ad6b7169203331-01",
            "00-+af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        ] {
            assert_eq!(TraceContext::parse(value), None, "{value:?}");
        }
    }

    /// The only test in this binary that installs the process-wide source.
    #[test]
    fn current_answers_from_the_installed_source_only() {
        fn stub() -> Option<TraceContext> {
            TraceContext::parse(VALID)
        }
        assert_eq!(current(), None);
        assert_eq!(current_traceparent(), None);
        assert!(set_current_context_source(stub));
        assert!(!set_current_context_source(|| None));
        assert_eq!(current_traceparent().as_deref(), Some(VALID));
    }

    #[test]
    fn formats_ids_zero_padded() {
        let context = TraceContext {
            trace_id: 1,
            span_id: 2,
            flags: 0,
        };
        assert_eq!(
            context.to_string(),
            "00-00000000000000000000000000000001-0000000000000002-00"
        );
        assert_eq!(TraceContext::parse(&context.to_string()), Some(context));
    }
}
