//! Proves the reusable contract in `cadet_core::conformance` actually rejects
//! a non-conforming backend — a suite that cannot fail is not a suite.

use cadet_core::conformance::{assert_round_trip, assert_stale_revision_is_rejected};
use cadet_core::*;

#[test]
fn the_suite_detects_a_backend_that_ignores_expected_revisions() {
    // The suite's value is that it FAILS for a non-conforming backend. Prove it
    // with a deliberately broken stub rather than asserting nothing.
    struct LastWriteWins {
        tasks: std::sync::Mutex<Vec<Task>>,
    }
    impl Backend for LastWriteWins {
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
            // The defect: `expected` is ignored.
            let mut g = self.tasks.lock().unwrap();
            g.retain(|t| t.uid != task.uid);
            let rev = revision(&task);
            g.push(task);
            Ok(rev)
        }
        fn delete(&self, _: TaskUid, _: Option<Revision>) -> Result<(), BackendError> {
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
            Ok(ChangeSet::Snapshot {
                snapshot: Snapshot {
                    complete: true,
                    observed: vec![],
                },
                tasks: Default::default(),
            })
        }
    }

    let b = LastWriteWins {
        tasks: std::sync::Mutex::new(vec![]),
    };
    let t = Task {
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
    };

    // Round-trip is satisfied even by the broken backend.
    assert_round_trip(&b, t.clone());
    // The conditional-write contract is not.
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_stale_revision_is_rejected(&b, t)
    }));
    assert!(
        caught.is_err(),
        "the suite must reject a backend that ignores `expected`"
    );
}
