//! The kernel API's store operations, driven through the host request the
//! runtime's `rlm.harness` client sends. The runtime's
//! `test/test_harness.py` is the end-to-end parity oracle; these pin the
//! request contract and the behaviours that oracle cannot reach.

use std::time::Duration;

use serde_json::json;

use super::*;

struct Store {
    _dir: tempfile::TempDir,
    file: PathBuf,
}

fn store() -> Store {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("harness_state.json");
    Store { _dir: dir, file }
}

impl Store {
    fn call(&self, request_type: &str, args: Value) -> Value {
        self.call_with(request_type, args, json!({}))
    }

    fn call_with(&self, request_type: &str, args: Value, extra: Value) -> Value {
        let mut request = json!({
            "type": request_type,
            "store": {"file": self.file.display().to_string(), "scope": "local"},
        });
        request["args"] = args;
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                request[key] = value;
            }
        }
        handle_request(&request)
    }

    fn ok(&self, request_type: &str, args: Value) -> Value {
        let reply = self.call(request_type, args);
        assert_eq!(reply["ok"], json!(true), "{reply}");
        reply["result"].clone()
    }

    fn on_disk(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.file).unwrap()).unwrap()
    }
}

fn memory(title: &str, content: &str, id: Option<&str>) -> Value {
    json!({
        "kind": "memory", "title": title, "content": content, "id": id,
        "path": "general", "reference": null, "arguments": null, "metadata": null,
        "source": "kernel",
    })
}

fn error_of(reply: &Value) -> (String, String) {
    assert_eq!(reply["ok"], json!(false), "{reply}");
    (
        reply["error"]["type"].as_str().unwrap().to_string(),
        reply["error"]["message"].as_str().unwrap().to_string(),
    )
}

#[test]
fn create_update_delete_round_trip_with_versions_and_kernel_provenance() {
    let store = store();
    let created = store.ok(
        "harness.create",
        memory("Prefer focused patches", "Small.", None),
    );
    assert_eq!(created["id"], json!("prefer_focused_patches"));
    assert_eq!(created["version"], json!(1));
    assert_eq!(created["source"], json!("kernel"));
    assert_eq!(created["scope"], json!("local"));

    let mut update = memory(
        "Prefer focused patches",
        "Smaller.",
        Some("prefer_focused_patches"),
    );
    update["path"] = Value::Null;
    let updated = store.ok("harness.update", update);
    assert_eq!(
        (
            updated["version"].clone(),
            updated["content"].clone(),
            updated["path"].clone()
        ),
        (json!(2), json!("Smaller."), json!("general"))
    );
    assert_eq!(
        store.on_disk()["entries"]["memory"]["prefer_focused_patches"],
        updated
    );

    let duplicate = store.call(
        "harness.create",
        memory(
            "Prefer focused patches",
            "Again.",
            Some("prefer_focused_patches"),
        ),
    );
    assert_eq!(
        error_of(&duplicate),
        (
            "ValueError".to_string(),
            "memory entry 'prefer_focused_patches' already exists".to_string()
        )
    );
    let missing = store.call("harness.update", memory("T", "c", Some("missing")));
    assert_eq!(
        error_of(&missing).1,
        "memory entry 'missing' does not exist"
    );

    let remove = json!({"kind": "memory", "id": "prefer_focused_patches"});
    assert_eq!(store.ok("harness.delete", remove.clone()), json!(true));
    assert_eq!(store.ok("harness.delete", remove), json!(false));
    assert_eq!(store.ok("harness.list", json!({"kind": null})), json!([]));
}

#[test]
fn rejections_name_the_entry_field_and_python_type() {
    let store = store();
    let cases = [
        (
            json!({"content": ["one string"]}),
            json!({"content": "list"}),
            "memory entry 't' rejected: content must be a non-empty string, got a list",
        ),
        (
            json!({"title": ""}),
            json!({}),
            "memory entry '<unnamed>' rejected: title must be a non-empty string, got an empty string",
        ),
        (
            json!({"id": 7}),
            json!({"id": "int"}),
            "memory entry 'T' rejected: id must be a non-empty string, got int",
        ),
        (
            json!({"metadata": ["m"], "id": "x"}),
            json!({"metadata": "list"}),
            "memory entry 'x' rejected: metadata must be a dict when provided, got a list",
        ),
        (
            json!({"metadata": {UNSERIALIZABLE_KEY: "set"}, "id": "x"}),
            json!({"metadata": "set"}),
            "memory entry 'x' rejected: metadata must be a dict when provided, got set",
        ),
    ];
    for (overrides, types, message) in cases {
        let mut args = memory("T", "c", None);
        for (key, value) in overrides.as_object().unwrap() {
            args[key] = value.clone();
        }
        let reply = store.call_with("harness.create", args, json!({ "types": types }));
        assert_eq!(
            error_of(&reply),
            ("ValueError".to_string(), message.to_string())
        );
    }
    assert!(!store.file.exists(), "no rejected write touched the disk");

    let unknown = store.call("harness.get", json!({"kind": "tool", "id": "x"}));
    assert_eq!(
        error_of(&unknown).1,
        "unknown harness kind 'tool'; expected one of ('prompt', 'memory', 'skill', 'subagent', 'factory')"
    );
    let unhashable = store.call_with(
        "harness.get",
        json!({"kind": "memory", "id": ["x"]}),
        json!({"types": {"id": "list"}}),
    );
    assert_eq!(
        error_of(&unhashable),
        (
            "TypeError".to_string(),
            "unhashable type: 'list'".to_string()
        )
    );
}

#[test]
fn a_nested_value_json_cannot_carry_refuses_the_save_like_json_dump() {
    let store = store();
    let mut args = memory("T", "c", Some("t"));
    args["metadata"] = json!({"when": {UNSERIALIZABLE_KEY: "datetime"}});
    assert_eq!(
        error_of(&store.call("harness.create", args)),
        (
            "TypeError".to_string(),
            "Object of type datetime is not JSON serializable".to_string()
        )
    );
    assert!(!store.file.exists());
}

#[test]
fn skill_and_factory_writes_check_their_contracts_first() {
    let store = store();
    let skill = |reference: Value| {
        json!({
            "kind": "skill", "title": "Skill", "content": "c", "id": "s", "path": "general",
            "reference": reference, "arguments": {}, "metadata": null, "source": "kernel",
        })
    };
    let mut args = skill(json!({"type": "shell"}));
    args["describeId"] = json!("global:s");
    assert_eq!(
        error_of(&store.call("harness.create_skill", args)).1,
        "skill entry 'global:s' rejected: skill reference.type must be 'python'"
    );
    let mut args = skill(json!({"type": "python", "import": "m", "callable": "run"}));
    args["describeId"] = json!("s");
    assert_eq!(store.ok("harness.create_skill", args)["id"], json!("s"));

    let factory = json!({
        "title": "F", "content": "c", "id": "f", "path": "general", "metadata": null,
        "source": "kernel", "dag": {"nodes": []}, "machine": null, "create": true,
    });
    assert_eq!(
        error_of(&store.call("harness.factory", factory.clone())).1,
        crate::refinement::FACTORY_DISABLED_MESSAGE
    );
    let agent_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"factory": {"enabled": true}}"#,
    )
    .unwrap();
    let enabled = json!({
        "agentDir": agent_dir.path().display().to_string(),
        "factorySpecErrors": ["factory dag needs at least one node"],
    });
    assert_eq!(
        error_of(&store.call_with("harness.factory", factory.clone(), enabled)).1,
        "factory dag needs at least one node"
    );
    let mut both = factory;
    both["machine"] = json!({"states": []});
    let enabled = json!({
        "agentDir": agent_dir.path().display().to_string(),
        "factorySpecErrors": [],
    });
    assert_eq!(
        error_of(&store.call_with("harness.factory", both, enabled)).1,
        "pass either dag or machine, not both"
    );
}

#[test]
fn set_enabled_flips_the_flag_without_a_new_version() {
    let store = store();
    store.ok("harness.create", memory("Fact", "c", Some("fact")));
    let disabled = store.ok(
        "harness.set_enabled",
        json!({"kind": "memory", "id": "fact", "enabled": false}),
    );
    assert_eq!(
        (disabled["enabled"].clone(), disabled["version"].clone()),
        (json!(false), json!(1))
    );
    let bad = store.call_with(
        "harness.set_enabled",
        json!({"kind": "memory", "id": "fact", "enabled": "no"}),
        json!({}),
    );
    assert_eq!(
        error_of(&bad),
        (
            "TypeError".to_string(),
            "enabled must be bool, got str".to_string()
        )
    );
    let missing = store.call(
        "harness.set_enabled",
        json!({"kind": "memory", "id": "missing", "enabled": true}),
    );
    assert_eq!(
        error_of(&missing).1,
        "memory entry 'missing' does not exist"
    );
    let overview = store.ok("harness.overview", json!({"max_entries_per_kind": 20}));
    assert!(
        overview
            .as_str()
            .unwrap()
            .contains("  - [local:fact] [disabled] Fact (general, v1): c"),
        "{overview}"
    );
}

/// A write keeps the top-level and per-entry keys other producers own, and
/// the host's refinement `reason`.
#[test]
fn writes_keep_unmodelled_state_and_the_refinement_reason() {
    let store = store();
    let seed = json!({
        "schema": 1,
        "entries": {"skill": {"probe": {
            "id": "probe", "kind": "skill", "title": "probe", "content": "body",
            "path": "skills/probe", "scope": "global", "reference": {}, "arguments": {},
            "metadata": {}, "source": "refine", "created_at": "t0", "updated_at": "t0",
            "version": 1, "trust": {"score": 20}
        }}},
        "refinements": [
            {"id": "refine_a", "trigger": "t", "changes": ["c"], "evidence": "", "outcome": "",
             "created_at": "t0", "reason": "turn_interval"},
            {"id": "refine_b", "trigger": "t", "changes": [], "reason": 7}
        ],
        "ravo": {"champions": []},
    });
    std::fs::write(&store.file, seed.to_string()).unwrap();
    store.ok("harness.upsert", memory("t", "c", None));
    let event = store.ok(
        "harness.record_refinement",
        json!({"trigger": "kernel", "changes": "one change", "evidence": "", "outcome": "", "id": null}),
    );
    assert_eq!(
        (event["id"].clone(), event["changes"].clone()),
        (json!("refine_0003"), json!(["one change"]))
    );
    let after = store.on_disk();
    assert_eq!(after["ravo"], seed["ravo"]);
    assert_eq!(
        after["entries"]["skill"]["probe"],
        seed["entries"]["skill"]["probe"]
    );
    assert_eq!(after["refinements"][0]["reason"], json!("turn_interval"));
    assert_eq!(after["refinements"][1].get("reason"), None);
}

/// The kernel's in-memory store: the client holds the document, the host
/// never touches a file.
#[test]
fn an_in_memory_store_round_trips_its_document() {
    let created = handle_request(&json!({
        "type": "harness.create",
        "store": {"file": null, "scope": "local", "document": null},
        "args": memory("Volatile", "in memory only", Some("volatile")),
    }));
    assert_eq!(created["ok"], json!(true));
    let read = handle_request(&json!({
        "type": "harness.get",
        "store": {"file": null, "scope": "local", "document": created["state"]},
        "args": {"kind": "memory", "id": "volatile"},
    }));
    assert_eq!(read["result"]["content"], json!("in memory only"));
    let refused = handle_request(&json!({
        "type": "harness.delete",
        "store": {"file": null, "scope": "local", "document": null, "writeError": "no store"},
        "args": {"kind": "memory", "id": "volatile"},
    }));
    assert_eq!(
        error_of(&refused),
        ("RuntimeError".to_string(), "no store".to_string())
    );
}

/// A write waits for a live holder of the store's lock, reclaims a stale
/// one, and releases its own.
#[test]
fn writes_serialize_on_the_store_lock() {
    let store = store();
    let lock = crate::platform::LockDir::path_for(&store.file);
    std::fs::create_dir(&lock).unwrap();
    let holder = {
        let lock = lock.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            std::fs::remove_dir(&lock).unwrap();
        })
    };
    store.ok("harness.create", memory("Waited", "c", Some("waited")));
    holder.join().unwrap();
    assert!(!lock.exists());
    let ids: Vec<Value> = store
        .ok("harness.list", json!({"kind": "memory"}))
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!("waited")]);
}

/// The store's lock with the shortest stale window `LockDir` allows, so a
/// hold past it fits in a test.
fn quick_lock(wait: Duration) -> LockPolicy {
    LockPolicy {
        wait,
        retry: Duration::from_millis(5),
        stale: Duration::from_secs(2),
    }
}

fn file_target(file: &Path, lock: LockPolicy) -> StoreTarget {
    StoreTarget {
        location: StoreLocation::File(file.to_path_buf()),
        scope: HarnessScope::Local,
        write_error: None,
        lock,
    }
}

/// A write whose hold outlives the stale window (its fsync stalled under
/// I/O pressure) still owns the lock: a concurrent writer that judged it a
/// crashed holder's leftover would run its read-modify-write in parallel
/// and one of the two writes would be lost.
#[test]
fn a_live_holder_past_the_stale_window_keeps_its_lock() {
    let store = store();
    let policy = quick_lock(Duration::from_secs(30));
    let held = lock_store(&store.file, policy).unwrap();
    let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let target = file_target(&store.file, policy);
        let released = std::sync::Arc::clone(&released);
        std::thread::spawn(move || {
            with_store(&target, true, |_| {
                Ok(released.load(std::sync::atomic::Ordering::SeqCst))
            })
            .map(|(holder_had_released, _)| holder_had_released)
        })
    };
    // The modelled slow write: the holder keeps the lock past the window.
    std::thread::sleep(policy.stale + Duration::from_secs(1));
    released.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(held);
    assert_eq!(writer.join().unwrap(), Ok(true));
}

/// The wait bounds how long ONE holder may keep the lock, not how long a
/// writer queues: under a convoy of writers that each hold briefly (slow
/// fsyncs, every handoff won by another waiter) a write keeps waiting
/// while the lock changes hands, instead of failing with a `TimeoutError`
/// that claims the lock was "held longer than" the wait.
#[test]
fn a_write_keeps_waiting_while_the_lock_changes_hands() {
    let store = store();
    let policy = quick_lock(Duration::from_millis(600));
    let lock = crate::platform::LockDir::path_for(&store.file);
    let first = crate::platform::LockDir::acquire(&store.file, policy.stale).unwrap();
    let convoy = {
        let file = store.file.clone();
        std::thread::spawn(move || {
            // Each holder well inside the wait, the convoy as a whole far past it.
            let mut held = first;
            for _ in 0..5 {
                std::thread::sleep(Duration::from_millis(300));
                drop(held);
                match crate::platform::LockDir::acquire(&file, policy.stale) {
                    Ok(next) => held = next,
                    // The waiter won this handoff: the convoy is over.
                    Err(_) => return,
                }
            }
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        })
    };
    let written = with_store(&file_target(&store.file, policy), true, |session| {
        session.dirty = true;
        Ok(())
    })
    .map(|((), _)| ());
    convoy.join().unwrap();
    assert_eq!(written, Ok(()));
    assert!(!lock.exists());
}

/// One holder that keeps the lock past the wait (a hung process whose lock
/// stays fresh) still fails the write, naming the lock.
#[test]
fn a_write_times_out_behind_one_holder_held_past_the_wait() {
    let store = store();
    let policy = quick_lock(Duration::from_secs(1));
    let _held = lock_store(&store.file, policy).unwrap();
    let refused = with_store(&file_target(&store.file, policy), true, |_| Ok(())).map(|((), _)| ());
    assert_eq!(
        refused,
        Err(StoreError::new(
            StoreErrorKind::Timeout,
            format!(
                "harness state is locked by another process: {} (held longer than 1s)",
                crate::platform::LockDir::path_for(&store.file).display()
            ),
        ))
    );
}

/// A write whose lock was taken over while it ran (another process judged
/// it dead and reclaimed the directory) refuses its save instead of
/// overwriting the new holder's state (upstream #3380's lost-lock check):
/// the store's lock records its owner, and the save re-checks it.
#[cfg(unix)]
#[test]
fn a_write_whose_lock_was_taken_over_refuses_its_save() {
    let store = store();
    store.ok("harness.create", memory("Seed", "c", Some("seed")));
    let before = std::fs::read_to_string(&store.file).unwrap();
    let lock = crate::platform::LockDir::path_for(&store.file);
    let refused = with_store(&file_target(&store.file, STORE_LOCK), true, |session| {
        // The takeover: a new holder's directory, a different owner.
        std::fs::remove_dir_all(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        std::fs::write(lock.join("owner"), "1 another-holder\n").unwrap();
        session.dirty = true;
        Ok(())
    })
    .map(|((), _)| ());
    assert_eq!(
        refused,
        Err(StoreError::new(
            StoreErrorKind::Runtime,
            format!("harness state lock lost: {}", lock.display())
        ))
    );
    assert_eq!(std::fs::read_to_string(&store.file).unwrap(), before);
    // The new holder's lock is left to its owner.
    assert!(lock.join("owner").exists());
    std::fs::remove_dir_all(&lock).unwrap();
}

#[test]
fn search_ranks_by_rarity_then_recency() {
    let store = store();
    store.ok(
        "harness.create",
        memory("Tea notes", "All about oolong brewing.", Some("tea")),
    );
    store.ok(
        "harness.create",
        memory(
            "Worktree policy",
            "Use git worktrees for parallel branches.",
            Some("worktree"),
        ),
    );
    let ranked = store.ok(
        "harness.search",
        json!({"query": "worktree branches", "kind": null, "limit": 10}),
    );
    assert_eq!(ranked.as_array().unwrap().len(), 1);
    assert_eq!(ranked[0]["id"], json!("worktree"));
    let bad = store.call_with(
        "harness.search",
        json!({"query": "worktree", "kind": null, "limit": 0}),
        json!({}),
    );
    assert_eq!(
        error_of(&bad),
        (
            "TypeError".to_string(),
            "limit must be a positive int".to_string()
        )
    );
}

#[test]
fn factory_writes_validate_the_spec_they_store() {
    // The client ships the spec's Python value (a node table) and the
    // store runs the factory validator itself, in its validation order.
    let store = store();
    let agent_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"factory": {"enabled": true}}"#,
    )
    .unwrap();
    let table = |value: Value| {
        crate::factory::pyvalue::encode_node_table(&crate::factory::pyvalue::PyValue::from_json(
            &value,
        ))
    };
    let enabled = |arguments: Value| {
        json!({
            "agentDir": agent_dir.path().display().to_string(),
            "factoryArguments": table(arguments),
        })
    };
    let empty_dag = json!({"nodes": []});
    let factory = json!({
        "title": "F", "content": "c", "id": "f", "path": "general", "metadata": null,
        "source": "kernel", "dag": empty_dag, "machine": null, "create": true,
    });
    assert_eq!(
        error_of(&store.call_with(
            "harness.factory",
            factory,
            enabled(json!({"machine": null, "dag": empty_dag}))
        )),
        (
            "ValueError".to_string(),
            "factory dag must declare between 1 and 1024 nodes, got 0".to_string()
        )
    );
    let generic = json!({
        "kind": "factory", "title": "F", "content": "c", "id": "g", "path": "general",
        "reference": null, "arguments": {"dag": empty_dag}, "metadata": null, "source": "kernel",
    });
    assert_eq!(
        error_of(&store.call_with(
            "harness.create",
            generic.clone(),
            enabled(json!({"dag": empty_dag}))
        ))
        .1,
        "factory entry 'g' rejected: factory dag must declare between 1 and 1024 nodes, got 0"
    );
    let valid = json!({"nodes": [{"id": "a", "subagent": "worker"}]});
    let mut stored = generic;
    stored["arguments"] = json!({"dag": valid});
    let reply = store.call_with("harness.create", stored, enabled(json!({"dag": valid})));
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert_eq!(reply["result"]["arguments"], json!({"dag": valid}));
}

/// A store's `harness.resolve_factory` reply value, decoded: `{"spec",
/// "subagents"}` as the client ships it to `factory.run`.
fn run_value(result: &Value) -> Value {
    crate::factory::pyvalue::decode_node_table(&result["value"])
        .expect("value table")
        .to_json()
}

#[test]
fn a_factory_run_resolves_its_entry_or_library_machine_and_subagents_in_one_request() {
    let store = store();
    let agent_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"factory": {"enabled": true}}"#,
    )
    .unwrap();
    let subagent = |title: &str, id: &str, metadata: Value| {
        json!({
            "kind": "subagent", "title": title, "content": format!("{title} prompt."), "id": id,
            "path": "general", "reference": null, "arguments": null, "metadata": metadata,
            "source": "kernel",
        })
    };
    store.ok("harness.create", subagent("Worker", "worker", json!(null)));
    store.ok(
        "harness.create",
        subagent(
            "The Reviewer",
            "reviewer-md",
            json!({"model": "m", "thinking": "low"}),
        ),
    );
    let dag = json!({"nodes": [
        {"id": "x", "subagent": "worker"},
        {"id": "y", "subagent": "The Reviewer"},
        {"id": "z", "subagent": "ghost"},
        {"id": "w", "subagent": {"prompt": "Inline."}},
        {"id": "v", "subagent": "worker"},
    ]});
    let reply = store.call_with(
        "harness.create",
        json!({
            "kind": "factory", "title": "F", "content": "c", "id": "sw", "path": "general",
            "reference": null, "arguments": {"dag": dag}, "metadata": null, "source": "kernel",
        }),
        json!({"agentDir": agent_dir.path().display().to_string(), "factorySpecErrors": []}),
    );
    assert_eq!(reply["ok"], json!(true), "{reply}");

    // A stored entry: its spec and every reference resolved (by id, else
    // by title; an unknown one is null for the host to report).
    let resolved = store.ok("harness.resolve_factory", json!({"id": "sw"}));
    assert_eq!(resolved["spec_id"], json!("sw"));
    assert_eq!(resolved.get("machine"), None);
    assert_eq!(
        run_value(&resolved),
        json!({
            "spec": dag,
            "subagents": {
                "worker": {"content": "Worker prompt.", "model": null, "thinking": null},
                "The Reviewer": {"content": "The Reviewer prompt.", "model": "m", "thinking": "low"},
                "ghost": null,
            },
        })
    );
    // No entry and no library: the unknown-spec refusal.
    assert_eq!(
        error_of(&store.call("harness.resolve_factory", json!({"id": "nope"}))),
        (
            "ValueError".to_string(),
            "unknown factory spec 'nope'".to_string()
        )
    );

    // The library: a template runs by name, with the run_factory frames
    // for a broken machine, a missing one, and an id that is no name.
    let library = tempfile::tempdir().unwrap();
    let repo = library.path().join("repo");
    for (name, spec) in [
        ("sweep", r#"{"nodes": [{"id": "a", "subagent": "worker"}]}"#),
        ("broken", r#"{"states": []}"#),
    ] {
        std::fs::create_dir_all(repo.join(name)).unwrap();
        std::fs::write(
            repo.join(name).join("MACHINE.md"),
            format!("---\nname: {name}\ndescription: D.\n---\n\n```machine-spec\n{spec}\n```\n"),
        )
        .unwrap();
    }
    let dirs = json!({"library": [
        ["repo", repo.display().to_string()],
        ["user", library.path().join("user").display().to_string()],
    ]});
    let template = store.call_with(
        "harness.resolve_factory",
        json!({"id": "sweep"}),
        dirs.clone(),
    );
    assert_eq!(template["ok"], json!(true), "{template}");
    let template = &template["result"];
    let path = repo.join("sweep").join("MACHINE.md").display().to_string();
    assert_eq!(
        (
            template["spec_id"].clone(),
            template["machine"].clone(),
            template["machine_path"].clone()
        ),
        (json!("sweep"), json!("sweep"), json!(path))
    );
    assert_eq!(
        run_value(template)["subagents"],
        json!({"worker": {"content": "Worker prompt.", "model": null, "thinking": null}})
    );
    let broken = repo.join("broken").join("MACHINE.md").display().to_string();
    for (id, message) in [
        (
            "broken",
            format!(
                "the library machine 'broken' exists but is broken ({broken}: factory machine \
                 must declare between 1 and 1024 states, got 0)"
            ),
        ),
        (
            "missing",
            "unknown factory spec 'missing': no stored factory entry and no library machine \
             with that name (unknown machine 'missing': no MACHINE.md for it in the machine \
             library (machines: sweep))"
                .to_string(),
        ),
        (
            "My Spec",
            "unknown factory spec 'My Spec': no stored factory entry, and the id is not a valid \
             machine name either (machine name contains invalid characters (must be lowercase \
             a-z, 0-9, hyphens only))"
                .to_string(),
        ),
    ] {
        assert_eq!(
            error_of(&store.call_with("harness.resolve_factory", json!({"id": id}), dirs.clone())),
            ("ValueError".to_string(), message),
            "{id}"
        );
    }

    // A machine the caller holds: its references resolve the same way.
    let table =
        crate::factory::pyvalue::encode_node_table(&crate::factory::pyvalue::PyValue::from_json(
            &json!({"states": [{"id": "a", "entry": true, "subagent": "reviewer-md"}]}),
        ));
    let held = store.call_with(
        "harness.resolve_factory",
        json!({"id": "held"}),
        json!({"factorySpec": table}),
    );
    assert_eq!(held["ok"], json!(true), "{held}");
    assert_eq!(held["result"]["spec_id"], json!("held"));
    assert_eq!(
        run_value(&held["result"])["subagents"],
        json!({"reviewer-md": {"content": "The Reviewer prompt.", "model": "m", "thinking": "low"}})
    );
}
