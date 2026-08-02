//! Proves the reusable contract in `cadet_core::conformance` actually rejects
//! a non-conforming backend — a suite that cannot fail is not a suite.

use cadet_core::conformance::*;
use cadet_core::*;

/// Deliberately broken in three separate ways, one per contract the suite is
/// supposed to enforce: it ignores `expected`, its `delete` is a no-op that
/// reports success, and its `scan` reports an empty store no matter what it
/// holds. Every one of those used to pass the suite.
struct BrokenBackend {
    tasks: std::sync::Mutex<Vec<Task>>,
}

impl Backend for BrokenBackend {
    fn load_project(&self) -> Result<ProjectConfig, BackendError> {
        Err(BackendError::NotFound)
    }
    fn save_project(&self, _: ProjectConfig) -> Result<(), BackendError> {
        Ok(())
    }
    fn get(&self, uid: TaskUid) -> Result<Option<Task>, BackendError> {
        Ok(self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.uid == uid)
            .cloned())
    }
    fn put(&self, task: Task, _expected: Option<Revision>) -> Result<Revision, BackendError> {
        // Defect 1: `expected` is ignored.
        let mut g = self.tasks.lock().unwrap();
        g.retain(|t| t.uid != task.uid);
        let rev = revision(&task);
        g.push(task);
        Ok(rev)
    }
    fn delete(&self, _: TaskUid, _: Option<Revision>) -> Result<(), BackendError> {
        // Defect 2: nothing is deleted, and it reports success anyway.
        Ok(())
    }
    fn adopt(
        &self,
        _: String,
        _: TaskUid,
        _: TaskKey,
        _: jiff::Timestamp,
    ) -> Result<Task, BackendError> {
        Err(BackendError::NotFound)
    }
    fn scan(&self, _: Option<Cursor>) -> Result<ChangeSet, BackendError> {
        // Defect 3: the scan never reports anything the store holds.
        Ok(ChangeSet::Snapshot {
            snapshot: Snapshot {
                complete: true,
                observed: vec![],
            },
            tasks: Default::default(),
            cursor: None,
        })
    }
}

fn broken() -> BrokenBackend {
    BrokenBackend {
        tasks: std::sync::Mutex::new(vec![]),
    }
}

fn sample() -> Task {
    Task {
        uid: TaskUid::generate(),
        key: TaskKey::new("P", 1),
        title: "x".into(),
        state: "todo".into(),
        created: jiff::Timestamp::UNIX_EPOCH,
        updated: jiff::Timestamp::UNIX_EPOCH,
        due: None,
        priority: Priority::Normal,
        tags: vec![],
        renumbered_from: None,
        possible_duplicate_of: None,
        fields: Default::default(),
        body: String::new(),
    }
}

fn rejects(name: &str, f: impl FnOnce() + std::panic::UnwindSafe) {
    let caught = std::panic::catch_unwind(f);
    assert!(
        caught.is_err(),
        "the suite must reject a backend that {name}"
    );
}

#[test]
fn the_suite_detects_a_backend_that_ignores_expected_revisions() {
    let b = broken();
    rejects("ignores `expected`", || {
        assert_stale_revision_is_rejected(&b, sample())
    });
}

#[test]
fn the_suite_detects_a_backend_whose_delete_does_nothing() {
    let b = broken();
    rejects("reports a delete it did not perform", || {
        assert_delete_removes_the_task(&b, sample())
    });
}

#[test]
fn the_suite_detects_a_backend_whose_scan_sees_nothing() {
    let b = broken();
    rejects("never reports what its store holds", || {
        assert_scan_detects_a_change(&b, sample())
    });
}

/// Round-trip is the one contract this backend happens to satisfy. Asserted
/// explicitly so it is on the record that passing `assert_round_trip` alone
/// says almost nothing about a backend.
#[test]
fn round_trip_alone_does_not_certify_a_backend() {
    let b = broken();
    assert_round_trip(&b, sample());
}
