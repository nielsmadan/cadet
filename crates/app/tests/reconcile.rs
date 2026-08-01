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
    repo_dir: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let vault = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(vault.path().join("project.toml"), CFG).unwrap();
    let backend = FsBackend::new(vault.path().to_path_buf());
    let index = SqliteIndex::open_in_memory().unwrap();
    let repo_dir = repo.path().join("r.git");
    let git = GitNet::new(repo_dir.clone(), vault.path().to_path_buf());
    git.ensure_init().unwrap();
    let vault_path = vault.path().to_path_buf();
    Fixture {
        _vault: vault,
        _repo: repo,
        app: App::new(Box::new(backend), index, git, "p".into()),
        vault_path,
        repo_dir,
    }
}

/// Commits directly against the fixture's repo, bypassing `App` entirely —
/// used to simulate a change that lands in its own git commit, independent
/// of any `App` write path, so a later `undo` (which only reverts the most
/// recent commit) cannot accidentally touch it.
fn commit_raw(repo_dir: &std::path::Path, work_tree: &std::path::Path, message: &str) {
    let add = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo_dir)
        .arg("--work-tree")
        .arg(work_tree)
        .args(["add", "--all"])
        .output()
        .unwrap();
    assert!(add.status.success());
    let commit = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo_dir)
        .arg("--work-tree")
        .arg(work_tree)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@localhost",
            "commit",
            "--no-gpg-sign",
            "-m",
            message,
        ])
        .output()
        .unwrap();
    assert!(commit.status.success());
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
fn a_pending_note_adopts_after_the_grace_period_even_when_polled() {
    let f = fixture();
    std::fs::write(
        f.vault_path.join("note.md"),
        "---\nstate: todo\ntitle: Hand made\n---\nx\n",
    )
    .unwrap();
    // Poll repeatedly inside the grace window, as running `cadet ls` more
    // than once a minute would. Each poll must not restart the clock.
    for t in [0, 10_000, 20_000, 30_000, 40_000, 50_000, 59_000] {
        let r = f.app.reconcile(t).unwrap();
        assert_eq!(
            r.adopted, 0,
            "must not adopt before the grace period, t={t}"
        );
    }
    let r = f.app.reconcile(60_001).unwrap();
    assert_eq!(
        r.adopted, 1,
        "polling during the grace period must not have reset since_ms"
    );
}

#[test]
fn adopt_pending_bypasses_the_grace_period_for_a_hand_written_note() {
    let f = fixture();
    std::fs::write(
        f.vault_path.join("note.md"),
        "---\nstate: todo\ntitle: Hand made\n---\nx\n",
    )
    .unwrap();
    // Never polled before — `adopt_pending` still adopts immediately: the
    // explicit request is itself the confirmation the grace period waits for.
    let r = f.app.adopt_pending(0).unwrap();
    assert_eq!(r.adopted, 1);
    assert_eq!(f.app.list(true).unwrap().len(), 1);
}

#[test]
fn a_write_does_not_drop_another_task_that_is_mid_grace_period() {
    let f = fixture();
    let a = f.app.add("vanishing").unwrap();
    f.app.add("steady").unwrap();
    f.app.reconcile(0).unwrap();

    std::fs::remove_file(f.vault_path.join("vanishing.md")).unwrap();
    let r = f.app.reconcile(1_000).unwrap();
    assert_eq!(
        r.pending_deletion, 1,
        "the vanished task must start a grace period"
    );

    // An ordinary write, unrelated to the vanished task, made while it is
    // still mid grace period. `add`'s internal `refresh_cache` must not
    // silently drop it from the cache just because this scan doesn't see it.
    f.app.add("third task").unwrap();

    let tasks = f.app.list(true).unwrap();
    assert!(
        tasks.iter().any(|t| t.uid == a.uid.as_str()),
        "a task mid pending-deletion grace period must survive an unrelated write, got: {tasks:?}"
    );
    assert!(tasks.iter().any(|t| t.title == "steady"));
    assert!(tasks.iter().any(|t| t.title == "third task"));
}

#[test]
fn an_explicitly_removed_task_does_not_reappear() {
    let f = fixture();
    let a = f.app.add("delete me").unwrap();
    // Mirrors the real CLI: a reconcile always runs before a command
    // dispatches, so `entries` already knows this uid by the time `delete`
    // runs — exactly the condition that let the stale row linger before
    // this fix.
    f.app.reconcile(0).unwrap();
    f.app.delete(&a.key).unwrap();

    // Reconcile twice, both times well past what the deletion grace period
    // would have been had this gone through the inferred-absence path.
    let r1 = f.app.reconcile(1_000).unwrap();
    assert_eq!(
        r1.pending_deletion, 0,
        "an explicit delete must never enter the deletion grace period"
    );
    assert_eq!(
        r1.deleted, 0,
        "there is nothing left to (re)discover as deleted"
    );
    assert!(
        !f.app
            .list(true)
            .unwrap()
            .iter()
            .any(|t| t.uid == a.uid.as_str()),
        "the explicitly removed task must not be listed"
    );

    let r2 = f.app.reconcile(1_000 + 60_000 + 1).unwrap();
    assert_eq!(r2.pending_deletion, 0);
    assert_eq!(r2.deleted, 0);
    assert!(
        !f.app
            .list(true)
            .unwrap()
            .iter()
            .any(|t| t.uid == a.uid.as_str()),
        "the explicitly removed task must never reappear"
    );
}

#[test]
fn removing_one_task_does_not_disturb_another_mid_grace_period() {
    let f = fixture();
    let vanishing = f.app.add("vanishing").unwrap();
    let doomed = f.app.add("doomed").unwrap();
    f.app.reconcile(0).unwrap();

    // `vanishing` disappears externally and starts its own grace period,
    // entirely unrelated to anything `delete` will do below.
    std::fs::remove_file(f.vault_path.join("vanishing.md")).unwrap();
    let r = f.app.reconcile(1_000).unwrap();
    assert_eq!(
        r.pending_deletion, 1,
        "the vanished task must start its own grace period"
    );
    assert!(
        f.app
            .list(true)
            .unwrap()
            .iter()
            .any(|t| t.uid == vanishing.uid.as_str()),
        "the vanished task must still be listed during its grace period"
    );

    // An explicit `cadet rm` of a completely different task.
    f.app.delete(&doomed.key).unwrap();

    let tasks = f.app.list(true).unwrap();
    assert!(
        tasks.iter().any(|t| t.uid == vanishing.uid.as_str()),
        "an unrelated explicit delete must not disturb another task's own grace period"
    );
    assert!(
        !tasks.iter().any(|t| t.uid == doomed.uid.as_str()),
        "the explicitly removed task must be gone immediately"
    );

    // The vanished task's own grace period must still run its normal,
    // untouched course.
    let r2 = f.app.reconcile(1_000 + 60_000 + 1).unwrap();
    assert_eq!(r2.deleted, 1);
    assert!(
        !f.app
            .list(true)
            .unwrap()
            .iter()
            .any(|t| t.uid == vanishing.uid.as_str())
    );
}

#[test]
fn undo_does_not_discard_another_tasks_pending_deletion() {
    let f = fixture();

    // Task A: removed by hand (as an external sync tool might) and
    // committed on its own, entirely independent of anything `undo` will
    // later revert. Reconciled into its own, independent pending-deletion
    // window.
    let _a = f.app.add("unrelated").unwrap();
    f.app.reconcile(0).unwrap();
    std::fs::remove_file(f.vault_path.join("unrelated.md")).unwrap();
    commit_raw(&f.repo_dir, &f.vault_path, "remove unrelated by hand");
    let r1 = f.app.reconcile(1_000).unwrap();
    assert_eq!(
        r1.pending_deletion, 1,
        "task A must start its own grace period"
    );
    assert!(
        f.app
            .list(true)
            .unwrap()
            .iter()
            .any(|t| t.title == "unrelated"),
        "task A must still be listed during its grace period"
    );

    // Task B: added through the ordinary `App::add` write path — its own
    // git commit, and its own `refresh_cache` call. `refresh_cache` must
    // now carry task A's placeholder forward too (that's the fix this
    // round), so this exercises the exact path `cadet add` followed by
    // `cadet undo` takes in practice, rather than sidestepping it.
    let _b = f.app.add("mistake").unwrap();
    assert!(
        f.app
            .list(true)
            .unwrap()
            .iter()
            .any(|t| t.title == "unrelated"),
        "an unrelated write must not have dropped task A mid-grace-period"
    );
    f.app.reconcile(1_500).unwrap();

    f.app.undo().unwrap();
    let r2 = f.app.reconcile_after_undo(2_000).unwrap();
    assert_eq!(r2.deleted, 1, "the undone task must be deleted immediately");
    assert_eq!(
        r2.pending_deletion, 1,
        "task A's unrelated pending deletion must still be tracked, not silently dropped"
    );

    let tasks = f.app.list(true).unwrap();
    assert!(
        tasks.iter().any(|t| t.title == "unrelated"),
        "an unrelated task's pending-deletion grace period must survive an unconnected undo"
    );
    assert!(
        !tasks.iter().any(|t| t.title == "mistake"),
        "the undone task must be gone"
    );

    // Advance well past task A's ORIGINAL grace-period baseline (since_ms
    // = 1_000). If the undo pass had reset or extended it, this would
    // either already have fired early or would still be pending here.
    let r3 = f.app.reconcile(1_000 + 60_000 + 1).unwrap();
    assert_eq!(
        r3.deleted, 1,
        "task A's own grace period must have run its normal, untouched course"
    );
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
