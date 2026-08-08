use cadet_backend_markdown::MarkdownBackend;
use cadet_core::conformance::*;
use cadet_core::*;
use std::collections::BTreeMap;

const CFG: &str = r#"
[project]
id = "p"
name = "P"
prefix = "P"
[workflow]
states = ["todo", "done"]
initial = "todo"
terminal = ["done"]
[[fields]]
name = "owner"
type = "str"
[[fields]]
name = "estimate"
type = "int"
[[fields]]
name = "labels"
type = "list<string>"
"#;

fn setup() -> (tempfile::TempDir, MarkdownBackend) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("project.toml"), CFG).unwrap();
    let b = MarkdownBackend::new(d.path().to_path_buf());
    (d, b)
}

fn task(title: &str) -> Task {
    Task {
        uid: TaskUid::generate(),
        key: TaskKey::new("P", 1),
        title: title.into(),
        state: "todo".into(),
        created: jiff::Timestamp::UNIX_EPOCH,
        updated: jiff::Timestamp::UNIX_EPOCH,
        due: None,
        priority: Priority::Normal,
        tags: vec![],
        renumbered_from: None,
        possible_duplicate_of: None,
        fields: BTreeMap::new(),
        body: "notes\n".into(),
    }
}

/// A task exercising every field the round-trip contract covers, not just
/// the three a bare `task()` sets.
fn rich(title: &str) -> Task {
    let mut t = task(title);
    t.due = Some("2026-09-01".into());
    t.priority = Priority::High;
    t.tags = vec!["home".into(), "errands".into()];
    t.fields
        .insert("owner".into(), FieldValue::Str("alice".into()));
    t.fields.insert("estimate".into(), FieldValue::Int(3));
    t.fields.insert(
        "labels".into(),
        FieldValue::List(vec!["a".into(), "b".into()]),
    );
    t
}

#[test]
fn loads_project_config() {
    let (_d, b) = setup();
    assert_eq!(b.load_project().unwrap().prefix, "P");
}

#[test]
fn put_then_get_round_trips() {
    let (_d, b) = setup();
    let t = task("Buy milk");
    b.put(t.clone(), None).unwrap();
    let got = b.get(t.uid.clone()).unwrap().unwrap();
    assert_eq!(got.title, "Buy milk");
    assert_eq!(got.state, "todo");
    assert_eq!(got.uid, t.uid);
}

#[test]
fn yaml_sensitive_strings_are_escaped_and_round_trip() {
    let (d, b) = setup();
    let mut t = task(r#"bug: press "global" at C:\hotkeys"#);
    t.fields.insert(
        "owner".into(),
        FieldValue::Str(r#"team: "core" at C:\desk"#.into()),
    );
    b.put(t.clone(), None).unwrap();

    let path = d
        .path()
        .join(b.location_of(t.uid.clone()).unwrap().unwrap());
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(
        raw.contains(r#"title: "bug: press \"global\" at C:\\hotkeys""#),
        "{raw}"
    );
    assert!(
        raw.contains(r#"owner: "team: \"core\" at C:\\desk""#),
        "{raw}"
    );
    let got = b.get(t.uid.clone()).unwrap().unwrap();
    assert_eq!(got.title, t.title);
    assert_eq!(got.fields.get("owner"), t.fields.get("owner"));
}

#[test]
fn an_existing_unquoted_colon_title_is_normalized_on_write() {
    let (d, b) = setup();
    let t = task("bug: broken frontmatter");
    b.put(t.clone(), None).unwrap();
    let path = d
        .path()
        .join(b.location_of(t.uid.clone()).unwrap().unwrap());
    let raw = std::fs::read_to_string(&path).unwrap().replace(
        "title: \"bug: broken frontmatter\"",
        "title: bug: broken frontmatter",
    );
    std::fs::write(&path, raw).unwrap();

    let loaded = b.get(t.uid.clone()).unwrap().unwrap();
    assert_eq!(loaded.title, t.title);
    b.put(loaded, None).unwrap();
    let normalized = std::fs::read_to_string(path).unwrap();
    assert!(
        normalized.contains("title: \"bug: broken frontmatter\""),
        "{normalized}"
    );
}

#[test]
fn put_writes_a_readable_slug_filename() {
    let (d, b) = setup();
    b.put(task("Buy Oat Milk"), None).unwrap();
    assert!(d.path().join("buy-oat-milk.md").exists());
}

#[test]
fn stale_expected_revision_is_rejected() {
    let (_d, b) = setup();
    let t = task("x");
    let rev = b.put(t.clone(), None).unwrap();
    let mut t2 = t.clone();
    t2.title = "y".into();
    b.put(t2, Some(rev.clone())).unwrap();
    let mut t3 = t.clone();
    t3.title = "z".into();
    assert!(matches!(
        b.put(t3, Some(rev)),
        Err(BackendError::RevisionMismatch)
    ));
}

#[test]
fn scan_returns_a_complete_snapshot_with_parsed_tasks() {
    let (_d, b) = setup();
    b.put(task("one"), None).unwrap();
    b.put(task("two"), None).unwrap();
    match b.scan(None).unwrap() {
        ChangeSet::Snapshot {
            snapshot, tasks, ..
        } => {
            assert!(snapshot.complete);
            assert_eq!(snapshot.observed.len(), 2);
            // Parsed content rides along so callers never re-read the files.
            assert_eq!(tasks.len(), 2);
            let mut titles: Vec<_> = tasks.values().map(|t| t.title.clone()).collect();
            titles.sort();
            assert_eq!(titles, vec!["one".to_string(), "two".to_string()]);
        }
        _ => panic!("fs backend must return a Snapshot, never a Delta"),
    }
}

// Deletion guard 4 — "a cloud placeholder is never absence" — cannot be tested in
// `core`, which has no I/O. It is enforced here: a dataless file makes the snapshot
// incomplete, and guard 1 then forbids inferring deletion from it. Without this test
// the guard is asserted by no task at all.
#[cfg(target_os = "macos")]
#[test]
fn a_placeholder_file_makes_the_snapshot_incomplete() {
    let (d, b) = setup();
    b.put(task("real"), None).unwrap();
    // An evicted iCloud file is represented by a `.name.icloud` sidecar.
    std::fs::write(d.path().join("evicted.md"), "---\nstate: todo\n---\n").unwrap();
    std::fs::write(d.path().join(".evicted.md.icloud"), "").unwrap();

    match b.scan(None).unwrap() {
        ChangeSet::Snapshot { snapshot, .. } => {
            assert!(
                !snapshot.complete,
                "a placeholder must make the snapshot incomplete, or guard 1 cannot fire"
            );
        }
        _ => panic!("fs backend must return a Snapshot"),
    }
}

#[test]
fn adopt_stamps_uid_and_key_into_an_existing_file() {
    let (d, b) = setup();
    let path = d.path().join("hand-written.md");
    std::fs::write(&path, "---\nstate: todo\ntitle: Hand made\n---\nbody\n").unwrap();

    let uid = TaskUid::generate();
    let key = TaskKey::new("P", 7);
    let t = b
        .adopt(
            "hand-written.md".into(),
            uid.clone(),
            key.clone(),
            jiff::Timestamp::UNIX_EPOCH,
        )
        .unwrap();

    assert_eq!(t.uid, uid);
    assert_eq!(t.key, key);
    assert_eq!(t.title, "Hand made");
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("uid: "));
    assert!(raw.contains("key: P-7"));
    assert!(
        raw.contains("title: Hand made"),
        "the original title must survive"
    );
    assert!(raw.contains("body"), "the body must survive");
}

// Covers all three shapes an undeclared key can take in one document, so
// this can no longer pass while two of the three are silently deleted: a
// plain scalar happened to survive by accident (it was swept into
// `task.fields` and written back unchanged), while a block list or nested
// map — which `Frontmatter::get` cannot even represent as a scalar — was
// deleted outright by the removal loop.
#[test]
fn unknown_keys_survive_a_write() {
    let (d, b) = setup();
    let t = task("x");
    b.put(t.clone(), None).unwrap();
    let path = d.path().join("x.md");
    let raw = std::fs::read_to_string(&path).unwrap();
    let with_extras = raw.replace(
        "state: todo",
        "state: todo\nmystery: keep-me\nextra_list:\n  - one\n  - two\nmeta:\n  owner: alice\n  weight: 3",
    );
    std::fs::write(&path, with_extras).unwrap();

    let mut t2 = b.get(t.uid.clone()).unwrap().unwrap();
    t2.state = "done".into();
    b.put(t2, None).unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("mystery: keep-me"),
        "an undeclared scalar must survive a write"
    );
    assert!(
        after.contains("extra_list:\n  - one\n  - two"),
        "an undeclared block list must survive a write"
    );
    assert!(
        after.contains("meta:\n  owner: alice\n  weight: 3"),
        "an undeclared nested map must survive a write"
    );
}

#[test]
fn an_unmanaged_block_list_survives_a_write() {
    let (d, b) = setup();
    let t = task("has block list");
    b.put(t.clone(), None).unwrap();
    let path = d.path().join("has-block-list.md");
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        raw.replace("state: todo", "state: todo\nextra_list:\n  - one\n  - two"),
    )
    .unwrap();

    let mut t2 = b.get(t.uid.clone()).unwrap().unwrap();
    t2.state = "done".into();
    b.put(t2, None).unwrap();

    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("extra_list:\n  - one\n  - two"),
        "an undeclared block list must survive a write byte-identical"
    );
}

#[test]
fn an_unmanaged_nested_map_survives_a_write() {
    let (d, b) = setup();
    let t = task("has nested map");
    b.put(t.clone(), None).unwrap();
    let path = d.path().join("has-nested-map.md");
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        raw.replace(
            "state: todo",
            "state: todo\nmeta:\n  owner: alice\n  weight: 3",
        ),
    )
    .unwrap();

    let mut t2 = b.get(t.uid.clone()).unwrap().unwrap();
    t2.state = "done".into();
    b.put(t2, None).unwrap();

    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("meta:\n  owner: alice\n  weight: 3"),
        "an undeclared nested map must survive a write byte-identical"
    );
}

#[test]
fn an_unmanaged_scalar_key_does_not_break_validation() {
    let (d, b) = setup();
    let t = task("has scalar");
    b.put(t.clone(), None).unwrap();
    let path = d.path().join("has-scalar.md");
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        raw.replace("state: todo", "state: todo\nnote_to_self: remember this"),
    )
    .unwrap();

    let got = b.get(t.uid.clone()).unwrap().unwrap();
    let cfg = b.load_project().unwrap();
    assert!(validate_task(&got, &cfg).is_ok());
}

#[test]
fn a_declared_field_removed_from_the_task_is_removed_from_the_file() {
    let (d, b) = setup();
    let mut t = task("has an amount");
    t.fields.insert("estimate".into(), FieldValue::Int(8));
    b.put(t.clone(), None).unwrap();
    let path = d.path().join("has-an-amount.md");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("estimate: 8")
    );

    let mut t2 = t.clone();
    t2.fields.clear();
    b.put(t2, None).unwrap();

    assert!(
        !std::fs::read_to_string(&path).unwrap().contains("estimate"),
        "a declared field removed from the task must be removed from the file"
    );
}

/// A *declared* field whose on-disk value is not the shape its declared type
/// demands cannot be read into `task.fields` — and must therefore not be a
/// candidate for the removal loop either. Otherwise a plausible hand-edit
/// (writing a scalar field as a block list, or as a nested map) is silently
/// deleted by the next ordinary `cadet done`.
#[test]
fn a_declared_field_with_a_mismatched_shape_is_preserved_not_deleted() {
    let (d, b) = setup();
    let t = task("hand edited");
    b.put(t.clone(), None).unwrap();
    let path = d.path().join("hand-edited.md");
    let raw = std::fs::read_to_string(&path).unwrap();
    // `estimate` is declared `int` and `owner` declared `str`; neither can
    // be read as a scalar in these shapes.
    std::fs::write(
        &path,
        raw.replace(
            "state: todo",
            "state: todo\nestimate:\n  - 3\n  - 5\nowner:\n  name: alice\n  team: core",
        ),
    )
    .unwrap();

    let mut t2 = b.get(t.uid.clone()).unwrap().unwrap();
    t2.state = "done".into();
    b.put(t2, None).unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("estimate:\n  - 3\n  - 5"),
        "a declared field written as a block list must survive:\n{after}"
    );
    assert!(
        after.contains("owner:\n  name: alice\n  team: core"),
        "a declared field written as a nested map must survive:\n{after}"
    );
}

#[test]
fn list_items_containing_commas_round_trip_intact() {
    let (_d, b) = setup();
    let mut t = task("commas");
    t.fields.insert(
        "labels".into(),
        FieldValue::List(vec![
            "has,comma".into(),
            "plain".into(),
            r#"C:\tmp,"quoted""#.into(),
        ]),
    );
    b.put(t.clone(), None).unwrap();

    let got = b.get(t.uid.clone()).unwrap().unwrap();
    assert_eq!(
        got.fields.get("labels"),
        Some(&FieldValue::List(vec![
            "has,comma".into(),
            "plain".into(),
            r#"C:\tmp,"quoted""#.into(),
        ]))
    );
}

#[test]
fn tags_containing_commas_round_trip_intact() {
    let (_d, b) = setup();
    let mut t = task("tag commas");
    t.tags = vec!["has,comma".into(), "plain".into()];
    b.put(t.clone(), None).unwrap();

    let got = b.get(t.uid.clone()).unwrap().unwrap();
    assert_eq!(got.tags, vec!["has,comma".to_string(), "plain".to_string()]);
}

#[test]
fn yaml_sensitive_tags_are_quoted_and_round_trip() {
    let (d, b) = setup();
    let mut t = task("yaml tags");
    t.tags = vec![
        "plain".into(),
        "bug: urgent".into(),
        "[nested]".into(),
        "#hash".into(),
        "true".into(),
    ];
    b.put(t.clone(), None).unwrap();

    let path = d
        .path()
        .join(b.location_of(t.uid.clone()).unwrap().unwrap());
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(
        raw.contains(r##"tags: ["plain", "bug: urgent", "[nested]", "#hash", "true"]"##),
        "{raw}"
    );
    assert_eq!(b.get(t.uid.clone()).unwrap().unwrap().tags, t.tags);
}

#[test]
fn delete_removes_the_file() {
    let (d, b) = setup();
    let t = task("gone");
    b.put(t.clone(), None).unwrap();
    b.delete(t.uid, None).unwrap();
    assert!(!d.path().join("gone.md").exists());
}

#[test]
fn custom_fields_round_trip_through_put_and_get() {
    let (_d, b) = setup();
    let mut t = task("with fields");
    t.fields
        .insert("owner".into(), FieldValue::Str("alice".into()));
    t.fields.insert("estimate".into(), FieldValue::Int(5));
    t.fields.insert(
        "labels".into(),
        FieldValue::List(vec!["a".into(), "b".into()]),
    );
    b.put(t.clone(), None).unwrap();

    let got = b.get(t.uid.clone()).unwrap().unwrap();
    assert_eq!(
        got.fields.get("owner"),
        Some(&FieldValue::Str("alice".into()))
    );
    assert_eq!(got.fields.get("estimate"), Some(&FieldValue::Int(5)));
    assert_eq!(
        got.fields.get("labels"),
        Some(&FieldValue::List(vec!["a".into(), "b".into()]))
    );
}

#[test]
fn removing_a_custom_field_removes_it_from_the_file() {
    let (d, b) = setup();
    let mut t = task("has field");
    t.fields
        .insert("owner".into(), FieldValue::Str("alice".into()));
    b.put(t.clone(), None).unwrap();
    let path = d.path().join("has-field.md");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("owner: \"alice\"")
    );

    let mut t2 = t.clone();
    t2.fields.clear();
    b.put(t2, None).unwrap();

    assert!(
        !std::fs::read_to_string(&path).unwrap().contains("owner"),
        "a field removed from the task must be removed from the file"
    );
}

#[test]
fn a_typed_custom_field_passes_validation_after_a_round_trip() {
    let (_d, b) = setup();
    let mut t = task("typed");
    t.fields.insert("estimate".into(), FieldValue::Int(3));
    b.put(t.clone(), None).unwrap();

    let got = b.get(t.uid.clone()).unwrap().unwrap();
    let cfg = b.load_project().unwrap();
    assert!(validate_task(&got, &cfg).is_ok());
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_does_not_abort_the_scan() {
    use std::os::unix::fs::PermissionsExt;

    let (d, b) = setup();
    b.put(task("one"), None).unwrap();
    b.put(task("two"), None).unwrap();
    b.put(task("three"), None).unwrap();

    let locked = d.path().join("locked.md");
    std::fs::write(&locked, "---\nstate: todo\ntitle: locked\n---\nbody\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let result = b.scan(None);

    // Restore permissions so the tempdir can clean itself up afterwards.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();

    match result.unwrap() {
        ChangeSet::Snapshot {
            snapshot, tasks, ..
        } => {
            assert!(
                !snapshot.complete,
                "an unreadable file must make the snapshot incomplete"
            );
            assert_eq!(tasks.len(), 3, "the readable tasks must still come back");
        }
        _ => panic!("fs backend must return a Snapshot"),
    }
}

#[test]
fn put_with_an_expected_revision_on_a_missing_task_is_rejected() {
    let (_d, b) = setup();
    let t = task("never written");
    let bogus_rev = revision(&t);
    assert!(matches!(
        b.put(t, Some(bogus_rev)),
        Err(BackendError::RevisionMismatch)
    ));
}

#[test]
fn changing_a_title_does_not_rename_the_file() {
    let (d, b) = setup();
    let t = task("Original Title");
    b.put(t.clone(), None).unwrap();
    let path = d.path().join("original-title.md");
    assert!(path.exists());

    let mut t2 = t.clone();
    t2.title = "Renamed Title".into();
    b.put(t2, None).unwrap();

    assert!(path.exists(), "the file must not be renamed");
    assert!(!d.path().join("renamed-title.md").exists());
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("title: \"Renamed Title\""),
        "the new title must be spliced in place"
    );
}

/// The other half of `LocalDbBackend`'s
/// `save_project_is_unsupported_rather_than_lossy`. Both backends decline for
/// the same reason — a from-scratch renderer would destroy the comments,
/// unmodelled keys and section ordering `render_project_toml` exists to
/// preserve — so both must decline the same WAY. The CLI already matches on
/// `Unsupported` to phrase capability errors for `undo` and `adopt`; an
/// `Io("… not implemented in milestone 1")` here reads to that matcher as a
/// real I/O failure and would be reported as one.
#[test]
fn save_project_is_unsupported_rather_than_a_bare_io_error() {
    let (d, b) = setup();
    let before = std::fs::read_to_string(d.path().join("project.toml")).unwrap();

    let cfg = b.load_project().unwrap();
    let err = b.save_project(cfg).unwrap_err();
    assert!(
        matches!(err, BackendError::Unsupported { .. }),
        "save_project must report Unsupported, as the other backend does: {err:?}"
    );

    let after = std::fs::read_to_string(d.path().join("project.toml")).unwrap();
    assert_eq!(
        before, after,
        "an Unsupported save_project must not touch the file at all"
    );
}

/// Proves the reusable contract in `cadet_core::conformance` is not just
/// self-consistent but actually satisfied by the only real implementation of
/// `Backend` — the whole point of extracting the suite.
#[test]
fn fs_backend_satisfies_the_conformance_suite() {
    let (_d, b) = setup();
    assert_scan_is_a_complete_snapshot(&b);
    assert_round_trip(&b, rich("Round trip"));
    assert_stale_revision_is_rejected(&b, task("Stale revision"));
    assert_delete_removes_the_task(&b, task("Deletable"));
    assert_scan_detects_a_change(&b, task("Changeable"));
}
