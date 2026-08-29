use cadet_backend_local_db::LocalDbBackend;
use cadet_core::{
    Backend, BackendError, ChangeSet, Cursor, FieldValue, Priority, Task, TaskKey, TaskUid,
    conformance,
};
use std::collections::BTreeMap;
use std::path::Path;

const CONFIG: &str = r#"
[project]
id = "t"
name = "T"
prefix = "T"

[tasks]
match = "frontmatter"

[workflow]
states = ["todo", "doing", "done"]
initial = "todo"
terminal = ["done"]

[[fields]]
name = "estimate"
type = "int"
"#;

fn backend() -> (tempfile::TempDir, LocalDbBackend) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("t.toml");
    std::fs::write(&cfg, CONFIG).unwrap();
    let b = LocalDbBackend::open(&dir.path().join("t.db")).unwrap();
    (dir, b)
}

fn task(n: u32, title: &str) -> Task {
    let mut fields = BTreeMap::new();
    fields.insert("estimate".to_string(), FieldValue::Int(3));
    Task {
        uid: TaskUid::generate(),
        key: TaskKey::new("T", n),
        title: title.into(),
        state: "todo".into(),
        created: "2026-08-02T00:00:00Z".parse().unwrap(),
        updated: "2026-08-02T00:00:00Z".parse().unwrap(),
        due: Some("2026-09-01".into()),
        priority: Priority::High,
        tags: vec!["home".into(), "urgent".into()],
        renumbered_from: None,
        possible_duplicate_of: None,
        fields,
        body: "some body\n".into(),
    }
}

fn assert_malformed_project_config(err: BackendError, config_path: &Path) {
    let message = err.to_string();
    match &err {
        BackendError::MalformedProjectConfig { path, .. } => {
            let path = Path::new(path);
            assert!(path.is_absolute(), "config path must be absolute: {path:?}");
            assert_eq!(path, config_path);
        }
        other => panic!("expected malformed project config, got {other:?}"),
    }
    assert!(message.contains("project config"), "{message}");
    assert!(!message.contains("task file"), "{message}");
}

#[test]
fn local_db_satisfies_the_conformance_suite() {
    let (_d, b) = backend();
    conformance::assert_scan_is_a_complete_snapshot(&b);
    conformance::assert_round_trip(&b, task(1, "round trip"));
    conformance::assert_stale_revision_is_rejected(&b, task(2, "stale"));
    conformance::assert_delete_removes_the_task(&b, task(3, "delete me"));
    conformance::assert_scan_detects_a_change(&b, task(4, "watch me"));
}

#[test]
fn adopt_is_unsupported_because_there_are_no_loose_rows() {
    let (_d, b) = backend();
    let err = b
        .adopt(
            "anything".into(),
            TaskUid::generate(),
            TaskKey::new("T", 9),
            "2026-08-02T00:00:00Z".parse().unwrap(),
        )
        .unwrap_err();
    assert!(
        matches!(err, BackendError::Unsupported { .. }),
        "adopt must report Unsupported, not a fake success: {err:?}"
    );
}

#[test]
fn the_project_config_comes_from_the_sibling_toml() {
    let (_d, b) = backend();
    let cfg = b.load_project().unwrap();
    assert_eq!(cfg.prefix, "T");
    assert_eq!(cfg.fields.len(), 1, "the declared field must be read");
}

#[test]
fn malformed_project_config_reports_config_path_for_load_and_snapshot_scan() {
    let (dir, _uid) = {
        let (dir, b) = backend();
        let t = task(1, "stored before config broke");
        b.put(t.clone(), None).unwrap();
        (dir, t.uid)
    };
    let config_path = dir.path().join("t.toml");
    std::fs::write(&config_path, "not [ valid toml").unwrap();

    let b = LocalDbBackend::open(&dir.path().join("t.db")).unwrap();
    assert_malformed_project_config(b.load_project().unwrap_err(), &config_path);
    assert_malformed_project_config(b.scan(None).unwrap_err(), &config_path);
}

#[test]
fn scan_rejects_malformed_project_config_for_an_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("t.toml");
    std::fs::write(&config_path, "not [ valid toml").unwrap();
    let b = LocalDbBackend::open(&dir.path().join("t.db")).unwrap();

    assert_malformed_project_config(b.scan(None).unwrap_err(), &config_path);
}

#[test]
fn scan_rejects_malformed_project_config_for_an_empty_delta() {
    let (dir, cursor) = {
        let (dir, b) = backend();
        let ChangeSet::Snapshot {
            cursor: Some(cursor),
            ..
        } = b.scan(None).unwrap()
        else {
            panic!("scan(None) must return a snapshot cursor");
        };
        (dir, cursor)
    };
    let config_path = dir.path().join("t.toml");
    std::fs::write(&config_path, "not [ valid toml").unwrap();

    let b = LocalDbBackend::open(&dir.path().join("t.db")).unwrap();
    assert_malformed_project_config(b.scan(Some(cursor)).unwrap_err(), &config_path);
}

/// A from-scratch renderer would overwrite the sibling `.toml` wholesale —
/// comments, unmodelled keys, section ordering, all gone — the exact bug
/// `render_project_toml` in `crates/cli/src/project.rs` exists to prevent.
/// Nothing calls `save_project` today, so an honest "I don't do that" beats
/// a lossy implementation with no caller to catch the loss.
#[test]
fn save_project_is_unsupported_rather_than_lossy() {
    let (dir, b) = backend();
    let before = std::fs::read_to_string(dir.path().join("t.toml")).unwrap();

    let cfg = b.load_project().unwrap();
    let err = b.save_project(cfg).unwrap_err();
    assert!(
        matches!(err, BackendError::Unsupported { .. }),
        "save_project must report Unsupported, not silently overwrite: {err:?}"
    );

    let after = std::fs::read_to_string(dir.path().join("t.toml")).unwrap();
    assert_eq!(
        before, after,
        "an Unsupported save_project must not touch the file at all"
    );
}

/// A declared field's type can change — an ordinary edit to `project.toml`.
/// `MarkdownBackend` coerces every field it reads to its *currently*
/// declared type; local-db must do the same, or the stored `kind` from the
/// moment the field was written wins forever and later reads disagree with
/// what the project now says the field is.
#[test]
fn get_coerces_a_field_whose_declared_type_changed_since_it_was_written() {
    let (dir, uid) = {
        let (dir, b) = backend();
        let t = task(1, "typed");
        b.put(t.clone(), None).unwrap();
        (dir, t.uid)
    };

    std::fs::write(
        dir.path().join("t.toml"),
        CONFIG.replace("type = \"int\"", "type = \"str\""),
    )
    .unwrap();

    // A fresh backend instance, the way a fresh `cadet` invocation would
    // open one — the point is that a new process must see the new
    // declaration, not that any one instance is expected to notice a
    // concurrent edit.
    let b2 = LocalDbBackend::open(&dir.path().join("t.db")).unwrap();
    let got = b2.get(uid).unwrap().unwrap();
    assert_eq!(
        got.fields.get("estimate"),
        Some(&FieldValue::Str("3".into())),
        "a field must be coerced to its currently declared type, not the \
         type it happened to be written with"
    );
}

/// A declared field can be removed entirely — the shipped CLI template
/// invites exactly this with a commented-out `[[fields]]` block. Reporting
/// a field the project no longer declares is not just noise: `validate_task`
/// rejects unknown fields, so every future write to every task carrying the
/// stale field would fail, fixable only with `sqlite3`.
#[test]
fn get_drops_a_field_whose_declaration_was_removed() {
    let (dir, uid) = {
        let (dir, b) = backend();
        let t = task(1, "declared");
        b.put(t.clone(), None).unwrap();
        (dir, t.uid)
    };

    let no_fields = CONFIG.split("\n[[fields]]").next().unwrap();
    std::fs::write(dir.path().join("t.toml"), no_fields).unwrap();

    let b2 = LocalDbBackend::open(&dir.path().join("t.db")).unwrap();
    let got = b2.get(uid).unwrap().unwrap();
    assert!(
        got.fields.is_empty(),
        "a field whose declaration was removed must not still be reported: {:?}",
        got.fields
    );
}

/// A declared field can be removed and later restored — an ordinary pair of
/// edits to `project.toml`. `MarkdownBackend::put` only ever emits a
/// *removal* edit for a field that is currently declared, so an undeclared
/// key already sitting in the file is never touched by `splice` — the value
/// survives the declaration's absence, it is only hidden while the
/// declaration is gone. local-db must match that contract: an ordinary
/// read-modify-write of some other attribute, made while the declaration is
/// gone, must not delete the field's stored row.
#[test]
fn an_ordinary_edit_while_a_field_is_undeclared_does_not_delete_it() {
    let (dir, uid) = {
        let (dir, b) = backend();
        let t = task(1, "keeps its fields");
        b.put(t.clone(), None).unwrap();
        (dir, t.uid)
    };

    // The declaration goes away...
    let no_fields = CONFIG.split("\n[[fields]]").next().unwrap();
    std::fs::write(dir.path().join("t.toml"), no_fields).unwrap();

    // ...and an ordinary edit touches something unrelated — the exact
    // get/mutate/put shape `App::update` uses.
    {
        let b2 = LocalDbBackend::open(&dir.path().join("t.db")).unwrap();
        let mut t = b2.get(uid.clone()).unwrap().unwrap();
        assert!(
            t.fields.is_empty(),
            "the field must be hidden while undeclared, not gone yet: {:?}",
            t.fields
        );
        t.title = "renamed while undeclared".into();
        b2.put(t, None).unwrap();
    }

    // ...then the declaration comes back.
    std::fs::write(dir.path().join("t.toml"), CONFIG).unwrap();
    let b3 = LocalDbBackend::open(&dir.path().join("t.db")).unwrap();
    let got = b3.get(uid).unwrap().unwrap();
    assert_eq!(
        got.fields.get("estimate"),
        Some(&FieldValue::Int(3)),
        "an ordinary edit made while a field was undeclared must not delete \
         it — markdown only ever hides an undeclared field, never deletes it"
    );
}

#[test]
fn local_db_deltas_reconstruct_the_snapshot() {
    let (_d, b) = backend();
    conformance::assert_deltas_reconstruct_the_snapshot(&b, task(1, "seed"), true);
}

#[test]
fn a_cursor_below_the_prune_floor_falls_back_to_a_full_snapshot() {
    let (_d, b) = backend();
    let t = task(1, "first");
    b.put(t.clone(), None).unwrap();
    b.delete(t.uid.clone(), None).unwrap();

    // Presenting a current cursor prunes tombstones at or below it.
    let ChangeSet::Delta { cursor, .. } = b.scan(Some(Cursor(b"0".to_vec()))).unwrap() else {
        panic!("expected a delta");
    };
    let _ = b.scan(Some(cursor)).unwrap();

    // A cursor from before the prune can no longer be served incrementally.
    match b.scan(Some(Cursor(b"0".to_vec()))).unwrap() {
        ChangeSet::Snapshot { snapshot, .. } => assert!(snapshot.complete),
        ChangeSet::Delta { .. } => {
            panic!("a cursor below the prune floor must fall back to a full snapshot")
        }
    }
}

#[test]
fn a_cursor_above_head_falls_back_without_poisoning_the_floor() {
    let (_d, b) = backend();
    let t = task(1, "first");
    b.put(t.clone(), None).unwrap();

    // Establish a real, legitimately-issued cursor.
    let legitimate = match b.scan(Some(Cursor(b"0".to_vec()))).unwrap() {
        ChangeSet::Delta { cursor, .. } => cursor,
        ChangeSet::Snapshot { .. } => panic!("expected a delta"),
    };

    // A cursor greater than the current head — malformed input, a cursor
    // from a different project's DB, a corrupted `cursors` row — must fall
    // back to a full snapshot rather than being accepted as a valid,
    // if odd, request.
    match b.scan(Some(Cursor(b"999999".to_vec()))).unwrap() {
        ChangeSet::Snapshot { snapshot, .. } => assert!(snapshot.complete),
        ChangeSet::Delta { .. } => {
            panic!("a cursor above the current head must fall back to a full snapshot")
        }
    }

    // Serving that bogus cursor must not have raised the prune floor: the
    // legitimate cursor issued earlier must still be servable as a delta.
    match b.scan(Some(legitimate)).unwrap() {
        ChangeSet::Delta { .. } => {}
        ChangeSet::Snapshot { .. } => panic!(
            "a cursor above head must not poison the prune floor — a \
             legitimately-issued cursor from before it must still work"
        ),
    }
}

#[test]
fn scan_reports_the_uid_as_the_locator() {
    let (_d, b) = backend();
    let t = task(1, "locatable");
    b.put(t.clone(), None).unwrap();
    match b.scan(None).unwrap() {
        cadet_core::ChangeSet::Snapshot {
            snapshot, tasks, ..
        } => {
            let o = snapshot
                .observed
                .iter()
                .find(|o| o.uid.as_ref() == Some(&t.uid));
            let o = o.expect("the task must be observed");
            assert_eq!(
                o.path,
                t.uid.as_str(),
                "a DB has no paths; the uid is the locator"
            );
            assert!(tasks.contains_key(t.uid.as_str()));
        }
        cadet_core::ChangeSet::Delta { .. } => panic!("scan(None) must return a Snapshot"),
    }
}
