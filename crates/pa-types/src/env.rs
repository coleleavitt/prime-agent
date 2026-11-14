//! Safe wrappers over the process-environment mutators that became `unsafe`
//! in the 2024 edition.
//!
//! `std::env::set_var` / `remove_var` are `unsafe` in Rust 2024: a write races
//! any concurrent read of the environment on another thread. Every caller in
//! this workspace mutates the environment at process startup (before the
//! worker/kernel threads exist) or inside single-threaded test setup, which
//! satisfies that contract, so the `unsafe` is discharged here once behind a
//! documented invariant instead of at every call site.

use std::ffi::OsStr;

/// Set the process environment variable `key` to `value`.
///
/// # Caller contract
/// Call only before threads that read the environment are spawned (process
/// startup) or from single-threaded test setup; a concurrent `std::env::var*`
/// on another thread would be a data race.
#[allow(
    unsafe_code,
    reason = "env mutation is unsafe in edition 2024; discharged by the documented caller contract"
)]
pub fn set_var<K: AsRef<OsStr>, V: AsRef<OsStr>>(key: K, value: V) {
    // SAFETY: the caller contract above (startup or single-threaded test setup)
    // rules out a concurrent environment read.
    unsafe { std::env::set_var(key, value) };
}

/// Remove the process environment variable `key`. Same caller contract as
/// [`set_var`].
#[allow(
    unsafe_code,
    reason = "env mutation is unsafe in edition 2024; discharged by the documented caller contract"
)]
pub fn remove_var<K: AsRef<OsStr>>(key: K) {
    // SAFETY: the caller contract above (startup or single-threaded test setup)
    // rules out a concurrent environment read.
    unsafe { std::env::remove_var(key) };
}
