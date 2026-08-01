use cadet_app::*;
use cadet_backend_fs::FsBackend;
use cadet_store_sqlite::SqliteIndex;

const CFG: &str = r#"
[project]
id = "p"
name = "P"
prefix = "P"
[workflow]
states = ["todo", "doing", "done"]
initial = "todo"
terminal = ["done"]
"#;

struct Fixture {
    _vault: tempfile::TempDir,
    _repo: tempfile::TempDir,
    app: App,
    vault_path: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let vault = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(vault.path().join("project.toml"), CFG).unwrap();
    let backend = FsBackend::new(vault.path().to_path_buf());
    let index = SqliteIndex::open_in_memory().unwrap();
    let git = GitNet::new(repo.path().join("r.git"), vault.path().to_path_buf());
    git.ensure_init().unwrap();
    let vault_path = vault.path().to_path_buf();
    Fixture {
        _vault: vault,
        _repo: repo,
        app: App::new(Box::new(backend), index, git, "p".into()),
        vault_path,
    }
}

#[test]
fn add_allocates_sequential_keys() {
    let f = fixture();
    let a = f.app.add("first").unwrap();
    let b = f.app.add("second").unwrap();
    assert_eq!(a.key.to_string(), "P-1");
    assert_eq!(b.key.to_string(), "P-2");
}

#[test]
fn keys_are_not_reused_after_deletion() {
    let f = fixture();
    let a = f.app.add("first").unwrap();
    f.app.delete(&a.key).unwrap();
    let b = f.app.add("second").unwrap();
    assert_eq!(
        b.key.to_string(),
        "P-2",
        "keys must never be reused (spec §5)"
    );
}

#[test]
fn list_returns_non_terminal_tasks() {
    let f = fixture();
    let a = f.app.add("open one").unwrap();
    f.app.add("open two").unwrap();
    f.app.set_state(&a.key, "done").unwrap();
    let open = f.app.list(false).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].title, "open two");
    assert_eq!(f.app.list(true).unwrap().len(), 2);
}

#[test]
fn a_hand_written_note_is_pending_before_the_grace_period() {
    let f = fixture();
    std::fs::write(
        f.vault_path.join("note.md"),
        "---\nstate: todo\ntitle: Hand made\n---\nx\n",
    )
    .unwrap();
    let r = f.app.reconcile(0).unwrap();
    assert_eq!(r.pending_adoption, 1);
    assert_eq!(r.adopted, 0);
    assert!(f.app.list(false).unwrap().is_empty());
}

#[test]
fn a_hand_written_note_is_adopted_after_the_grace_period() {
    let f = fixture();
    std::fs::write(
        f.vault_path.join("note.md"),
        "---\nstate: todo\ntitle: Hand made\n---\nx\n",
    )
    .unwrap();
    f.app.reconcile(0).unwrap();
    let r = f.app.reconcile(60_001).unwrap();
    assert_eq!(r.adopted, 1);
    let raw = std::fs::read_to_string(f.vault_path.join("note.md")).unwrap();
    assert!(raw.contains("uid: "), "adoption must write uid back");
    assert!(raw.contains("key: P-"), "adoption must write key back");
    assert!(raw.contains("Hand made"), "the original title must survive");
}

#[test]
fn external_deletion_is_pending_then_applied() {
    let f = fixture();
    let a = f.app.add("doomed").unwrap();
    f.app.reconcile(0).unwrap();
    std::fs::remove_file(f.vault_path.join("doomed.md")).unwrap();

    let r1 = f.app.reconcile(1_000).unwrap();
    assert_eq!(r1.pending_deletion, 1);
    assert_eq!(f.app.list(false).unwrap().len(), 1, "not deleted yet");

    let r2 = f.app.reconcile(70_000).unwrap();
    assert_eq!(r2.deleted, 1);
    assert!(f.app.list(false).unwrap().is_empty());
    let _ = a;
}

#[test]
fn a_task_that_returns_then_vanishes_again_gets_a_fresh_grace_period() {
    let f = fixture();
    let a = f.app.add("flaky").unwrap();
    f.app.reconcile(0).unwrap();
    let path = f.vault_path.join("flaky.md");
    let saved = std::fs::read_to_string(&path).unwrap();

    // Vanishes — pending deletion recorded.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(f.app.reconcile(1_000).unwrap().pending_deletion, 1);

    // Returns — the pending-deletion record must be cleared.
    std::fs::write(&path, &saved).unwrap();
    f.app.reconcile(2_000).unwrap();

    // Vanishes again, long after the original absence. It must be PENDING again,
    // not deleted outright on the strength of the stale first timestamp.
    std::fs::remove_file(&path).unwrap();
    let r = f.app.reconcile(500_000).unwrap();
    assert_eq!(
        r.deleted, 0,
        "a stale pending-deletion record must not skip the grace period"
    );
    assert_eq!(r.pending_deletion, 1);
    let _ = a;
}

#[test]
fn a_mass_disappearance_is_rejected_rather_than_deleted() {
    let f = fixture();
    for i in 0..20 {
        f.app.add(&format!("task {i}")).unwrap();
    }
    f.app.reconcile(0).unwrap();
    for entry in std::fs::read_dir(&f.vault_path).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "md") {
            std::fs::remove_file(p).unwrap();
        }
    }
    let r = f.app.reconcile(1_000).unwrap();
    assert!(r.scan_rejected, "a 100% drop must reject the scan");
    assert_eq!(
        f.app.list(false).unwrap().len(),
        20,
        "nothing may be deleted"
    );
}

#[test]
fn index_rebuild_reproduces_the_same_state() {
    let f = fixture();
    f.app.add("one").unwrap();
    f.app.add("two").unwrap();
    let before = f.app.list(true).unwrap();

    f.app.clear_index().unwrap();
    f.app.reconcile(0).unwrap();
    let after = f.app.list(true).unwrap();

    assert_eq!(before.len(), after.len());
    for (a, b) in before.iter().zip(after.iter()) {
        assert_eq!(a.uid, b.uid);
        assert_eq!(a.key, b.key);
        assert_eq!(a.title, b.title);
    }
}

#[test]
fn an_external_rename_preserves_identity() {
    let f = fixture();
    let a = f.app.add("movable").unwrap();
    f.app.reconcile(0).unwrap();

    std::fs::rename(
        f.vault_path.join("movable.md"),
        f.vault_path.join("relocated.md"),
    )
    .unwrap();

    let r = f.app.reconcile(1_000).unwrap();
    assert_eq!(r.renamed, 1);

    let after = f.app.list(true).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].uid, a.uid.as_str());
    assert_eq!(after[0].key, a.key);
    assert_eq!(after[0].title, a.title);
}

#[test]
fn an_external_copy_gets_a_fresh_identity() {
    let f = fixture();
    let a = f.app.add("duplicatable").unwrap();
    f.app.reconcile(0).unwrap();

    std::fs::copy(
        f.vault_path.join("duplicatable.md"),
        f.vault_path.join("duplicatable-copy.md"),
    )
    .unwrap();

    let r = f.app.reconcile(1_000).unwrap();
    assert_eq!(r.copies, 1);

    let tasks = f.app.list(true).unwrap();
    assert_eq!(
        tasks.len(),
        2,
        "the original and its copy must both be visible"
    );
    let copy = tasks
        .iter()
        .find(|t| t.uid != a.uid.as_str())
        .expect("the copy must have gotten a distinct uid");
    assert_ne!(copy.key, a.key, "the copy must have gotten a distinct key");

    let b = f.app.add("brand new").unwrap();
    assert_ne!(
        b.key, copy.key,
        "add() must not mint a key that collides with one the copy was given"
    );
    assert_ne!(b.key, a.key);
}

#[test]
fn a_failed_commit_does_not_fail_the_write() {
    let vault = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(vault.path().join("project.toml"), CFG).unwrap();
    let backend = FsBackend::new(vault.path().to_path_buf());
    let index = SqliteIndex::open_in_memory().unwrap();
    // Deliberately never initialised: every `git.commit` call fails with no
    // repository to write to, independent of the backend and the index.
    let git = GitNet::new(
        repo.path().join("never-initialised.git"),
        vault.path().to_path_buf(),
    );
    let app = App::new(Box::new(backend), index, git, "p".into());

    let task = app.add("resilient").unwrap();

    let tasks = app.list(true).unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "the write must have succeeded regardless of git"
    );
    assert_eq!(tasks[0].title, "resilient");

    let warnings = app.drain_warnings();
    assert_eq!(warnings.len(), 1, "the failed commit must be surfaced");
    assert!(
        warnings[0].contains("safety net"),
        "unexpected warning text: {}",
        warnings[0]
    );
    assert!(
        app.drain_warnings().is_empty(),
        "drain must clear the queue"
    );
    let _ = task;
}
