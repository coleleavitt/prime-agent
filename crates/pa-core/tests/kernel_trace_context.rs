//! The kernel's trace-context carriers: with a current-context source
//! installed (a stub here; the trace subscriber in the product), the kernel
//! child inherits `TRACEPARENT` and every request frame carries
//! `traceparent`. Without one (the companion test binary
//! `kernel_trace_context_absent`) neither carrier is set.

#![cfg(unix)]

mod trace_kernel;

use pa_types::trace_context::{set_current_context_source, TraceContext};

const STUB: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

fn stub() -> Option<TraceContext> {
    TraceContext::parse(STUB)
}

#[tokio::test]
async fn the_kernel_child_and_its_requests_carry_the_current_context() {
    assert!(set_current_context_source(stub));
    let seen = trace_kernel::observe_carriers().await;
    assert_eq!(
        seen,
        trace_kernel::Carriers {
            frame: Some(STUB.to_string()),
            env: Some(STUB.to_string()),
        }
    );
}
