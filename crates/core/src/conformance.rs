//! The contract every Backend implementation must satisfy. When a second
//! backend is added, it calls this same suite — that is what stops a backend
//! lying about its behaviour.

use crate::{Backend, BackendError, ChangeSet, Task};

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

pub fn assert_round_trip(b: &dyn Backend, task: Task) {
    b.put(task.clone(), None).unwrap();
    let got = b
        .get(task.uid.clone())
        .unwrap()
        .expect("task must be readable after put");
    assert_eq!(got.uid, task.uid);
    assert_eq!(got.title, task.title);
    assert_eq!(got.state, task.state);
}
