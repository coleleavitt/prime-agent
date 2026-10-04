//! Lock poisoning on the roster path: a panic in one holder of a roster
//! lock must not turn every later roster read and push into a panic of
//! its own (the long-lived supervisor would cascade one bug into an outage).
use super::*;
use crate::supervisor_roster_seed::tests::{
    live_child_summary, register_root_worker, roster_fixture,
};

/// Poison `mutex` the way production would: a thread panics while it
/// holds the guard.
fn poison<T: Send>(mutex: &std::sync::Mutex<T>) {
    std::thread::scope(|scope| {
        let holder = scope.spawn(|| {
            let _guard = mutex.lock();
            panic!("a roster holder panics with the guard held");
        });
        assert!(holder.join().is_err(), "the holder panicked");
    });
    assert!(mutex.is_poisoned());
}

/// After every roster lock is poisoned, `roster_subscribe` still answers
/// the same snapshot and a worker's roster pull still publishes its
/// `roster_update`.
#[tokio::test]
async fn poisoned_roster_locks_still_serve_subscribe_and_pushes() {
    let (dir, supervisor, root_file, child_file) = roster_fixture().await;
    register_root_worker(&supervisor, "w-root", &root_file).await;
    let resident = supervisor
        .registry
        .get("w-root")
        .await
        .expect("registered root worker");
    let mut summary = live_child_summary(&root_file, &child_file);
    summary["runtimeKind"] = json!("top-level");
    summary["sessionId"] = json!("root-persisted");
    summary["id"] = json!("root-persisted");
    summary["sessionFile"] = json!(root_file.to_string_lossy());
    let fields = summary.as_object_mut().expect("summary object");
    fields.remove("rlmChildId");
    fields.remove("parentSessionPath");
    supervisor
        .write_roster_summary_for_resident(&resident, &summary)
        .await
        .expect("the healthy pull writes");
    let healthy = supervisor
        .handle_roster_subscribe("s0", "roster_subscribe")
        .await;
    assert!(healthy.success);

    poison(&supervisor.roster);
    poison(&supervisor.last_published_roster);
    poison(&supervisor.pending_registration_seeds);

    let after_poison = supervisor
        .handle_roster_subscribe("s1", "roster_subscribe")
        .await;
    assert_eq!(
        (after_poison.success, after_poison.data),
        (true, healthy.data)
    );

    let mut events = supervisor.events.subscribe();
    summary["activity"] = json!("idle");
    summary["isStreaming"] = json!(false);
    supervisor
        .write_roster_summary_for_resident(&resident, &summary)
        .await
        .expect("the pull after the poison writes");
    let refreshed = supervisor
        .handle_roster_subscribe("s2", "roster_subscribe")
        .await;
    let refreshed_roster = refreshed.data.expect("roster snapshot")["roster"].clone();
    let pushed: Vec<Value> = drain_roster_pushes(&mut events)
        .into_iter()
        .map(|push| push["changed"].clone())
        .collect();
    assert_eq!(pushed, vec![refreshed_roster]);
    let _ = std::fs::remove_dir_all(&dir);
}
