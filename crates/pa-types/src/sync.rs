//! Poison-tolerant acquisition for std locks.
//!
//! A std [`Mutex`]/[`RwLock`] is poisoned when a holder panics with the guard held, and every
//! later `.lock().unwrap()` then panics too. In the long-lived processes (the daemon supervisor,
//! session workers, the kernel manager) one bad turn would cascade into an outage. The guarded
//! values here are maps, queues and bookkeeping that every critical section leaves valid between
//! statements, so the right default is to keep serving: these methods recover the guard through
//! [`PoisonError::into_inner`] instead of panicking.
//!
//! The rule `pa-lock-unwrap-outside-tests` in `codegraph-rules/locks.yaml` flags new
//! `.lock()/.read()/.write()` + `unwrap`/`expect` sites in product code.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Poison-tolerant [`Mutex::lock`]. Implementations block like `lock` and return the guard even
/// when a previous holder panicked; they never panic on poisoning.
pub trait MutexExt<T: ?Sized> {
    /// Acquire the mutex, recovering the guard from a poisoned lock.
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> MutexExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Poison-tolerant [`RwLock::read`]/[`RwLock::write`]. Implementations block like the std
/// methods and return the guard even when a previous writer panicked; they never panic on
/// poisoning.
pub trait RwLockExt<T: ?Sized> {
    /// Acquire shared read access, recovering the guard from a poisoned lock.
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T>;
    /// Acquire exclusive write access, recovering the guard from a poisoned lock.
    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `hold` on a thread; it panics with a guard held, which poisons the lock.
    fn panic_while_holding(hold: impl FnOnce() + Send) {
        std::thread::scope(|scope| {
            assert!(scope.spawn(hold).join().is_err(), "the holder panicked");
        });
    }

    #[test]
    fn a_poisoned_mutex_serves_the_value_its_holder_left() {
        let counter = Mutex::new(vec![1]);
        panic_while_holding(|| {
            let mut guard = counter.lock().unwrap();
            guard.push(2);
            panic!("a holder panics with the guard held");
        });
        assert!(counter.is_poisoned());
        counter.lock_or_recover().push(3);
        assert_eq!(*counter.lock_or_recover(), vec![1, 2, 3]);
    }

    #[test]
    fn a_poisoned_rwlock_serves_reads_and_writes() {
        let state = RwLock::new(String::from("a"));
        panic_while_holding(|| {
            let mut guard = state.write().unwrap();
            guard.push('b');
            panic!("a writer panics with the guard held");
        });
        assert!(state.is_poisoned());
        state.write_or_recover().push('c');
        assert_eq!(*state.read_or_recover(), "abc");
    }
}
