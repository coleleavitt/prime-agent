//! Sinks shipped with the crate.

pub mod analytics;
pub mod file;
pub mod mock;
pub mod noop;

pub use analytics::{ANALYTICS_ENDPOINT, AnalyticsSink};
pub use file::FileSink;
pub use mock::{MockSink, RecordedBatch};
pub use noop::NoopSink;
