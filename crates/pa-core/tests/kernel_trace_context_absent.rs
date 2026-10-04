//! Without a current-context source (the native product) the kernel gets no
//! `traceparent` frame field and inherits `TRACEPARENT` exactly as before:
//! whatever this process had, nothing more.

#![cfg(unix)]

mod trace_kernel;

#[tokio::test]
async fn no_source_sets_no_carrier() {
    let seen = trace_kernel::observe_carriers().await;
    assert_eq!(
        seen,
        trace_kernel::Carriers {
            frame: None,
            env: std::env::var(pa_types::trace_context::TRACEPARENT_ENV).ok(),
        }
    );
}
