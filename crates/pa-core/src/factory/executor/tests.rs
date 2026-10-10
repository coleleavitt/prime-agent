//! Rust ports of the Python executor battery (`prime-agent-runtime/test/
//! test_factory.py`: `FactoryExecutorTest`, `FactoryGraphWatchTest`,
//! `FactoryFrameCapTest`, the opt-in gate's lane test), plus the durable
//! store's persistence and restart-recovery tests. Every test runs the
//! executor against the scripted host in [`fake`] on a current-thread
//! runtime, so interleavings are as deterministic as the original
//! single-threaded asyncio battery.

pub(crate) mod fake;
mod graph;
mod machine;
mod persist;
mod policies;
mod recovery;
mod run;
