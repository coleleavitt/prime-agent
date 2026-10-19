//! Pre-flight refusal prediction: content filter + surrogate predictor + refusal guard + logger.
//!
//! This crate provides a Rust-native port of the TypeScript refusal engine
//! from `packages/core/src/` in the TS `anthropic-auth` fork.
//!
//! ## Architecture
//!
//! - **Surrogate** (`surrogate.rs`): hashed word-n-gram feature vector -> logistic model -> P(refuse).
//!   Pure arithmetic + md5, deterministic, <1ms.
//! - **Filter** (`filter.rs`): keyword classification, scoring, rewrite rules,
//!   size-based eviction, dynamic threshold scaling.
//! - **Guard** (`guard.rs`): closed-loop escalation -- runs the filter, asks the surrogate,
//!   if P(refuse) stays dangerous, escalates eviction floors and re-scores.
//! - **Logger** (`logger.rs`): writes JSONL event files to `~/.prime/agent/`.
//!
//! ## Integration
//!
//! Consumed via `pa-ai::request_hooks::ProviderRequestHooks::prepare` where the
//! filter rewrites the request body before sending. Controlled by a `content_filter`
//! setting that is off by default.

pub mod filter;
pub mod guard;
pub mod logger;
pub mod surrogate;

pub use filter::ContentFilter;
pub use guard::{GuardResult, RefusalGuard};
pub use surrogate::SurrogateModel;
