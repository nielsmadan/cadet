//! The contract every Backend implementation must satisfy. When a second
//! backend is added, it calls this same suite — that is what stops a backend
//! lying about its behaviour.
//!
//! Spec §9.1 names five categories: round-trip CRUD, a conditional write
//! rejecting a stale revision, `scan` detecting external changes,
//! `scan(None)` returning a complete snapshot, and deletion semantics. Each
//! has an `assert_*` here, and `crates/core/tests/conformance.rs` proves the
//! suite fails for a backend that gets them wrong.

use crate::{Backend, BackendError, ChangeSet, Observed, Task, TaskKey, TaskUid};

pub fn assert_scan_is_a_complete_snapshot(b: &dyn Backend) {
    match b.scan(None).unwrap() {
        ChangeSet::Snapshot { snapshot, .. } => assert!(
            snapshot.complete,
            "scan(None) must return a complete snapshot — deletion inference depends on it"
        ),
        ChangeSet::Delta { .. } => panic!("scan(None) must not return a Delta"),
    }
}

pub fn assert_stale_revision_is_rejected(b: &dyn Backend, mut task: Task) {
    let rev = b.put(task.clone(), None).unwrap();
    task.title = format!("{}!", task.title);
    b.put(task.clone(), Some(rev.clone())).unwrap();
    task.title = format!("{}?", task.title);
    assert!(
        matches!(b.put(task, Some(rev)), Err(BackendError::RevisionMismatch)),
        "a stale `expected` revision must be rejected"
    );
}

/// Round-trip CRUD. Compares every field a backend is responsible for
/// persisting: `uid`, `key`, `title`, `state`, `created`, `due`, `priority`,
/// `tags`, custom `fields`, `body`, `renumbered_from` and
/// `possible_duplicate_of` — not a sample of three. A backend that drops any
/// one of them round-trips "successfully" under a weaker check while losing
/// user data on every write.
///
/// `renumbered_from` and `possible_duplicate_of` are overwritten on `task`
/// before the round trip regardless of what the caller passed in: leaving
/// them at whatever a fixture happened to set (usually `None`) would let a
/// backend that silently drops them pass every time every fixture in the
/// suite left them unset.
pub fn assert_round_trip(b: &dyn Backend, mut task: Task) {
    task.renumbered_from = Some(TaskKey::new(
        task.key.prefix.clone(),
        task.key.number.wrapping_add(1),
    ));
    task.possible_duplicate_of = Some(TaskUid::generate());

    b.put(task.clone(), None).unwrap();
    let got = b
        .get(task.uid.clone())
        .unwrap()
        .expect("task must be readable after put");
    assert_eq!(got.uid, task.uid);
    assert_eq!(got.key, task.key, "key must survive a round trip");
    assert_eq!(got.title, task.title);
    assert_eq!(got.state, task.state);
    assert_eq!(
        got.created, task.created,
        "created must survive a round trip"
    );
    assert_eq!(got.due, task.due, "due must survive a round trip");
    assert_eq!(
        got.priority, task.priority,
        "priority must survive a round trip"
    );
    assert_eq!(got.tags, task.tags, "tags must survive a round trip");
    assert_eq!(
        got.fields, task.fields,
        "custom fields must survive a round trip"
    );
    assert_eq!(got.body, task.body, "body must survive a round trip");
    assert_eq!(
        got.renumbered_from, task.renumbered_from,
        "renumbered_from must survive a round trip — collision resolution \
         writes it back through `put`, and losing it makes `cadet doctor`'s \
         renumber bookkeeping permanently wrong"
    );
    assert_eq!(
        got.possible_duplicate_of, task.possible_duplicate_of,
        "possible_duplicate_of must survive a round trip"
    );
}

/// Deletion semantics: a deleted task is gone, and deleting something that is
/// not there is `NotFound` rather than a silent success — a caller cannot
/// tell "I removed it" from "there was nothing to remove" otherwise.
pub fn assert_delete_removes_the_task(b: &dyn Backend, task: Task) {
    b.put(task.clone(), None).unwrap();
    assert!(
        b.get(task.uid.clone()).unwrap().is_some(),
        "the task must exist before it is deleted"
    );
    b.delete(task.uid.clone(), None).unwrap();
    assert!(
        b.get(task.uid.clone()).unwrap().is_none(),
        "delete must remove the task"
    );
    assert!(
        matches!(b.delete(task.uid, None), Err(BackendError::NotFound)),
        "deleting a task that is not there must be NotFound, not a silent success"
    );
}

/// External-change detection: `scan` reports the store as it is now, not as
/// the backend last remembered handing it out. A task written to the store
/// appears in the next scan, and changing it changes the revision the scan
/// reports — that revision is the only signal reconcile has that a file
/// changed under it.
pub fn assert_scan_detects_a_change(b: &dyn Backend, mut task: Task) {
    b.put(task.clone(), None).unwrap();
    let first = observe(b, &task.uid).expect("scan must observe a task that is in the store");

    task.title = format!("{} (edited)", task.title);
    b.put(task.clone(), None).unwrap();
    let second = observe(b, &task.uid).expect("scan must still observe the task after a change");

    assert_ne!(
        first.revision, second.revision,
        "scan must report a new revision once the task changes"
    );
}

fn observe(b: &dyn Backend, uid: &TaskUid) -> Option<Observed> {
    match b.scan(None).unwrap() {
        ChangeSet::Snapshot { snapshot, .. } => snapshot
            .observed
            .into_iter()
            .find(|o| o.uid.as_ref() == Some(uid)),
        ChangeSet::Delta { .. } => panic!("scan(None) must not return a Delta"),
    }
}
