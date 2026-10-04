//! The live-kernel registry: one registry serves every kernel client so process-wide
//! cleanup can dispose them exactly once. The TS process hooks live in the process
//! layer here: the binary crate calls [`shutdown_all_live_kernels`] with the same
//! flush-then-teardown ordering.

use std::sync::{Arc, Mutex, Weak};

use crate::kernel::manager::Inner;

type Registry = Mutex<Vec<Weak<Inner>>>;

fn registry() -> &'static Registry {
    static REGISTRY: Registry = Mutex::new(Vec::new());
    &REGISTRY
}

/// Track a kernel from the moment startup begins so cleanup can dispose a kernel. Entries are weak:
/// dropping the manager deregisters implicitly.
pub(crate) fn add(inner: &std::sync::Arc<Inner>) {
    let mut entries = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.retain(|weak| weak.strong_count() > 0);
    if !contains(&entries, inner) {
        entries.push(Arc::downgrade(inner));
    }
}

/// Stop tracking a kernel: teardown or defunct transition.
pub(crate) fn remove(inner: &Inner) {
    let mut entries = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.retain(|weak| {
        weak.strong_count() == 0
            || !std::ptr::eq(
                Weak::as_ptr(weak).cast::<()>(),
                std::ptr::from_ref(inner).cast::<()>(),
            )
    });
}

/// Alias matching the manager's internal call sites.
pub(crate) fn remove_inner(inner: &Inner) {
    remove(inner);
}

fn contains(entries: &[Weak<Inner>], inner: &std::sync::Arc<Inner>) -> bool {
    entries
        .iter()
        .any(|weak| std::ptr::eq(Weak::as_ptr(weak), std::sync::Arc::as_ptr(inner)))
}

/// Flush and tear down every live kernel, discarding individual failures: the shutdown of one
/// wedged kernel must not block the others. Mirrors the TS async shutdown handler (`snapshot:
/// true`).
pub async fn shutdown_all_live_kernels() {
    let snapshots: Vec<std::sync::Arc<Inner>> = {
        let mut entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|weak| weak.strong_count() > 0);
        entries.iter().filter_map(Weak::upgrade).collect()
    };
    let mut joins = Vec::new();
    for inner in snapshots {
        joins.push(tokio::spawn(async move {
            let _ = inner
                .shutdown_for_cleanup(crate::kernel::shared::KernelShutdownOptions {
                    snapshot: true,
                    drain_host_requests: false,
                })
                .await;
        }));
    }
    for join in joins {
        let _ = join.await;
    }
}

/// The sessions the registry currently serves.
pub fn live_kernel_count() -> usize {
    let mut entries = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.retain(|weak| weak.strong_count() > 0);
    entries.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::shared::KernelManagerOptions;

    /// Whether the registry holds a live entry for this allocation. Tests
    /// ask about their own kernel, not the global count: the registry is
    /// process-wide and other tests' kernels come and go concurrently.
    fn tracks(target: &Weak<Inner>) -> bool {
        let mut entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|weak| weak.strong_count() > 0);
        entries.iter().any(|weak| Weak::ptr_eq(weak, target))
    }

    #[tokio::test]
    async fn registry_tracks_and_releases_kernels() {
        let manager = crate::kernel::ReplKernelManager::new(KernelManagerOptions::default());
        let weak = Arc::downgrade(&manager.inner);
        assert!(!tracks(&weak));
        add(&manager.inner);
        assert!(tracks(&weak));
        // Removing the same pointer deregisters it.
        remove(&manager.inner);
        assert!(!tracks(&weak));
        add(&manager.inner);
        drop(manager);
        assert_eq!(
            weak.strong_count(),
            0,
            "the manager held the only strong ref"
        );
        assert!(!tracks(&weak), "weak entry must vanish with the manager");
    }

    #[tokio::test]
    async fn shutdown_all_is_safe_with_no_kernels() {
        shutdown_all_live_kernels().await;
    }
}
