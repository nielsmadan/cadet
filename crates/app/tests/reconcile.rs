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

[[fields]]
name = "estimate"
type = "int"
"#;

struct Fixture {
    _vault: tempfile::TempDir,
    _repo: tempfile::TempDir,
    app: App,
    vault_path: std::path::PathBuf,
    repo_dir: std::path::PathBuf,
}

impl Fixture {
    /// A second `App` over the same vault and the same git repo, but with a
    /// brand-new index. This is what deleting `index.db` actually looks like
    /// — unlike `App::clear_index`, which deliberately preserves the
    /// high-water mark and so cannot exercise the "rebuild from the backend
    /// alone" path (spec §1).
    fn with_a_deleted_index(&self) -> App {
        App::new(
            Box::new(FsBackend::new(self.vault_path.clone())),
            SqliteIndex::open_in_memory().unwrap(),
            GitNet::new(self.repo_dir.clone(), self.vault_path.clone()),
            "p".into(),
        )
    }
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

/// Spec §5: high-water is the max over the keys live in the snapshot, the
/// stored high-water mark, and quarantined files' keys — not the stored mark
/// alone. Deleting the index takes the stored mark with it, so a mint after a
/// rebuild has nothing but the snapshot to go on.
#[test]
fn a_mint_after_the_index_is_deleted_does_not_reuse_a_key() {
    let f = fixture();
    f.app.add("one").unwrap();
    f.app.add("two").unwrap();

    let rebuilt = f.with_a_deleted_index();
    rebuilt.reconcile(0).unwrap();
    let third = rebuilt.add("three").unwrap();

    let keys: Vec<String> = rebuilt
        .list(true)
        .unwrap()
        .iter()
        .map(|t| t.key.to_string())
        .collect();
    let unique: std::collections::BTreeSet<&String> = keys.iter().collect();
    assert_eq!(
        unique.len(),
        keys.len(),
        "keys must never be reused (spec §5), got {keys:?}"
    );
    assert_eq!(third.key.to_string(), "P-3");
}

/// The other half of the same defect: the very first sync from another
/// device. The backend already carries P-1 and P-2; this device's index has
/// never allocated anything.
#[test]
fn a_mint_after_adopting_foreign_files_does_not_reuse_a_key() {
    let f = fixture();
    for (n, title) in [(1, "alpha"), (2, "beta")] {
        std::fs::write(
            f.vault_path.join(format!("{title}.md")),
            format!(
                "---\nuid: 01ARZ3NDEKTSV4RRFFQ69G5F0{n}\nkey: P-{n}\ntitle: {title}\nstate: todo\n---\n"
            ),
        )
        .unwrap();
    }
    f.app.reconcile(0).unwrap();
    let fresh = f.app.add("mine").unwrap();
    let keys: Vec<String> = f
        .app
        .list(true)
        .unwrap()
        .iter()
        .map(|t| t.key.to_string())
        .collect();
    let unique: std::collections::BTreeSet<&String> = keys.iter().collect();
    assert_eq!(
        unique.len(),
        keys.len(),
        "an adopted foreign key must raise the high-water mark, got {keys:?}"
    );
    assert_eq!(fresh.key.to_string(), "P-3");
}

/// Two devices that have not synced can both allocate P-1. Both tasks are
/// legitimate: the later one is renumbered and records `renumbered_from`
/// (spec §5), and until then neither is unreachable.
#[test]
fn two_tasks_claiming_one_key_are_renumbered() {
    let f = fixture();
    for (n, title, created) in [
        (1, "mine", "2026-01-01T00:00:00Z"),
        (2, "theirs", "2026-01-02T00:00:00Z"),
    ] {
        std::fs::write(
            f.vault_path.join(format!("{title}.md")),
            format!(
                "---\nuid: 01ARZ3NDEKTSV4RRFFQ69G5F0{n}\nkey: P-1\ntitle: {title}\nstate: todo\ncreated: {created}\nupdated: {created}\n---\n"
            ),
        )
        .unwrap();
    }

    // First observation: the collision is recorded, no file is written yet.
    let r = f.app.reconcile(0).unwrap();
    assert_eq!(
        r.pending_renumber, 1,
        "§5: never mutate on first observation"
    );
    assert_eq!(r.renumbered, 0);
    assert!(
        std::fs::read_to_string(f.vault_path.join("theirs.md"))
            .unwrap()
            .contains("key: P-1"),
        "no file may be rewritten on first observation"
    );
    // Both tasks are reachable regardless — that is the point of resolving.
    let keys: Vec<String> = f
        .app
        .list(true)
        .unwrap()
        .iter()
        .map(|t| t.key.to_string())
        .collect();
    assert_eq!(keys.len(), 2);
    let unique: std::collections::BTreeSet<&String> = keys.iter().collect();
    assert_eq!(
        unique.len(),
        2,
        "both tasks must be reachable, got {keys:?}"
    );

    // After the grace period the loser's file is rewritten.
    let r = f.app.reconcile(60_001).unwrap();
    assert_eq!(r.renumbered, 1);
    let loser = std::fs::read_to_string(f.vault_path.join("theirs.md")).unwrap();
    assert!(
        loser.contains("key: P-2"),
        "unexpected loser file:\n{loser}"
    );
    assert!(
        loser.contains("renumbered_from: P-1"),
        "the loser must record the key it gave up:\n{loser}"
    );
    let keeper = std::fs::read_to_string(f.vault_path.join("mine.md")).unwrap();
    assert!(
        keeper.contains("key: P-1"),
        "the earlier-created task keeps the key:\n{keeper}"
    );
    assert!(!keeper.contains("renumbered_from"));

    // Settled: no further churn, and a later mint does not collide.
    let r = f.app.reconcile(120_000).unwrap();
    assert_eq!(r.renumbered, 0);
    assert_eq!(r.pending_renumber, 0);
    assert_eq!(f.app.add("third").unwrap().key.to_string(), "P-3");
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

/// `task_from_summary` reconstructs a placeholder `Task` from the cache for
/// any task the scan didn't observe this cycle (only pending deletion), so
/// it can round-trip back through `cache_tasks` without vanishing from
/// `list()` a full deletion cycle early. Now that `cache_tasks` persists
/// tags and custom fields (not just uid/key/title/state/due/priority), that
/// reconstruction has to carry them over too, or an unrelated write mid
/// grace period silently wipes them from the cache.
#[test]
fn a_task_mid_grace_period_keeps_its_tags_and_fields_through_a_cache_rebuild() {
    let f = fixture();
    std::fs::write(
        f.vault_path.join("tagged.md"),
        "---\nstate: todo\ntitle: Tagged\ntags: [home, errand]\n---\nx\n",
    )
    .unwrap();
    f.app.reconcile(0).unwrap();
    f.app.reconcile(60_001).unwrap();
    let before = f.app.list(true).unwrap();
    let tagged = before
        .iter()
        .find(|t| t.title == "Tagged")
        .expect("the hand-written note must have been adopted");
    assert_eq!(
        tagged.tags,
        vec!["home".to_string(), "errand".to_string()],
        "tags must be cached once the task is adopted"
    );

    std::fs::remove_file(f.vault_path.join("tagged.md")).unwrap();
    let r = f.app.reconcile(120_002).unwrap();
    assert_eq!(
        r.pending_deletion, 1,
        "the vanished task must start a grace period"
    );

    // An ordinary write, unrelated to the vanished task, made while it is
    // still mid grace period. This forces `cache_tasks` to rewrite the
    // whole project's cache from `cached_by_uid` (via `task_from_summary`)
    // union the freshly scanned tasks.
    f.app.add("unrelated").unwrap();

    let after = f.app.list(true).unwrap();
    let tagged_after = after
        .iter()
        .find(|t| t.title == "Tagged")
        .expect("a task mid pending-deletion grace period must survive an unrelated write");
    assert_eq!(
        tagged_after.tags,
        vec!["home".to_string(), "errand".to_string()],
        "tags must survive a cache rebuild while the task is mid grace period"
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
    assert_eq!(
        r.scan_rejected,
        Some(RejectReason::SuspectedIncompleteScan),
        "a 100% drop must reject the scan"
    );
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

/// §5 names this exact case: "closes the everyday failure where a rename
/// delivered mid-sync (create lands before delete) is read as a duplicate and
/// permanently renumbered". A copy is a first observation like any other, so
/// neither file may be touched until the grace period has run.
#[test]
fn an_external_copy_gets_a_fresh_identity_only_after_the_grace_period() {
    let f = fixture();
    let a = f.app.add("duplicatable").unwrap();
    f.app.reconcile(0).unwrap();

    let original = f.vault_path.join("duplicatable.md");
    let duplicate = f.vault_path.join("duplicatable-copy.md");
    std::fs::copy(&original, &duplicate).unwrap();
    let before = std::fs::read_to_string(&original).unwrap();

    let r = f.app.reconcile(1_000).unwrap();
    assert_eq!(r.copies, 0, "§5: never mutate on first observation");
    assert_eq!(r.pending_adoption, 1);
    assert_eq!(
        std::fs::read_to_string(&original).unwrap(),
        before,
        "the original must not lose its identity to a copy of itself"
    );
    assert_eq!(
        std::fs::read_to_string(&duplicate).unwrap(),
        before,
        "the copy must not be written to on first observation either"
    );
    let tasks = f.app.list(true).unwrap();
    assert_eq!(tasks.len(), 1, "the copy is not a task yet");
    assert_eq!(tasks[0].uid, a.uid.as_str());

    let r = f.app.reconcile(61_001).unwrap();
    assert_eq!(r.copies, 1);
    assert_eq!(
        std::fs::read_to_string(&original).unwrap(),
        before,
        "the file that held the uid first keeps it"
    );

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

/// The rebuild half of the same defect: `cp -p` a task file, delete the
/// index, and the two files share a uid with nothing in the index to say so.
/// The incremental path produces two tasks; the rebuild path must not
/// produce one (spec §1 — you get back everything the backend has).
#[test]
fn a_duplicated_uid_survives_a_rebuild_instead_of_collapsing() {
    let f = fixture();
    let a = f.app.add("alpha").unwrap();
    std::fs::copy(
        f.vault_path.join("alpha.md"),
        f.vault_path.join("alpha-copy.md"),
    )
    .unwrap();

    let rebuilt = f.with_a_deleted_index();
    rebuilt.reconcile(0).unwrap();
    assert_eq!(
        rebuilt.list(true).unwrap().len(),
        1,
        "the copy waits out its grace period first"
    );
    rebuilt.reconcile(61_001).unwrap();

    let tasks = rebuilt.list(true).unwrap();
    assert_eq!(
        tasks.len(),
        2,
        "a rebuild must not silently swallow a file, got {tasks:?}"
    );
    assert!(tasks.iter().any(|t| t.uid == a.uid.as_str()));
    assert!(tasks.iter().any(|t| t.uid != a.uid.as_str()));
}

/// Finding 6: `Copy` must retire the pending record it just consumed. Paths,
/// unlike uids, are reused — a stale row grants whatever file lands here next
/// an adoption with no grace period at all.
#[test]
fn a_path_reused_after_a_copy_does_not_inherit_instant_adoption() {
    let f = fixture();
    f.app.add("original").unwrap();
    let copy_path = f.vault_path.join("original-copy.md");
    std::fs::copy(f.vault_path.join("original.md"), &copy_path).unwrap();
    f.app.reconcile(0).unwrap();
    assert_eq!(f.app.reconcile(61_001).unwrap().copies, 1);

    // The copy is deleted and a brand-new hand-written note takes its path.
    std::fs::remove_file(&copy_path).unwrap();
    std::fs::write(&copy_path, "---\nstate: todo\ntitle: Successor\n---\n").unwrap();

    let r = f.app.reconcile(62_000).unwrap();
    assert_eq!(
        r.adopted, 0,
        "a stale pending row must not grant instant adoption at a reused path"
    );
    assert_eq!(r.pending_adoption, 1);
}

/// A backend that reports every scan as incomplete, wrapping a real
/// `FsBackend` so every other operation behaves normally. That is what a
/// single unreadable file or an unmaterialised cloud placeholder does to a
/// scan — and an incomplete snapshot is never evidence of absence (§5 guard
/// 1). `reconcile` honours that; `refresh_cache` runs on every write and
/// must not quietly reach the opposite conclusion.
struct IncompleteBackend {
    inner: FsBackend,
    incomplete: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Makes `scan` fail outright rather than return a partial view — a
    /// broken `project.toml`, a vanished vault root. Everything a write
    /// needs (`put`, `delete`) still works, so the write lands and only the
    /// cache refresh on top of it fails.
    fail_scan: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl cadet_core::Backend for IncompleteBackend {
    fn load_project(&self) -> Result<cadet_core::ProjectConfig, cadet_core::BackendError> {
        self.inner.load_project()
    }
    fn save_project(&self, cfg: cadet_core::ProjectConfig) -> Result<(), cadet_core::BackendError> {
        self.inner.save_project(cfg)
    }
    fn get(
        &self,
        uid: cadet_core::TaskUid,
    ) -> Result<Option<cadet_core::Task>, cadet_core::BackendError> {
        self.inner.get(uid)
    }
    fn put(
        &self,
        task: cadet_core::Task,
        expected: Option<cadet_core::Revision>,
    ) -> Result<cadet_core::Revision, cadet_core::BackendError> {
        self.inner.put(task, expected)
    }
    fn delete(
        &self,
        uid: cadet_core::TaskUid,
        expected: Option<cadet_core::Revision>,
    ) -> Result<(), cadet_core::BackendError> {
        self.inner.delete(uid, expected)
    }
    fn adopt(
        &self,
        path: String,
        uid: cadet_core::TaskUid,
        key: cadet_core::TaskKey,
        now: jiff::Timestamp,
    ) -> Result<cadet_core::Task, cadet_core::BackendError> {
        self.inner.adopt(path, uid, key, now)
    }
    fn scan(
        &self,
        since: Option<cadet_core::Cursor>,
    ) -> Result<cadet_core::ChangeSet, cadet_core::BackendError> {
        if self.fail_scan.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(cadet_core::BackendError::Io("scan is unavailable".into()));
        }
        let mut cs = self.inner.scan(since)?;
        if !self.incomplete.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(cs);
        }
        if let cadet_core::ChangeSet::Snapshot {
            snapshot,
            mut tasks,
        } = cs
        {
            // An incomplete scan is one that did not see everything: drop a
            // file from the snapshot as well as clearing the flag.
            let mut observed = snapshot.observed;
            if !observed.is_empty() {
                let d = observed.remove(0);
                tasks.remove(&d.path);
            }
            cs = cadet_core::ChangeSet::Snapshot {
                snapshot: cadet_core::Snapshot {
                    complete: false,
                    observed,
                },
                tasks,
            };
        }
        Ok(cs)
    }
}

#[test]
fn an_incomplete_scan_does_not_drop_a_task_from_the_cache_on_a_write() {
    let f = fixture();
    let a = f.app.add("unseen").unwrap();
    let b = f.app.add("visible").unwrap();
    f.app.reconcile(0).unwrap();

    let incomplete = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app = App::new(
        Box::new(IncompleteBackend {
            inner: FsBackend::new(f.vault_path.clone()),
            incomplete: std::sync::Arc::clone(&incomplete),
            fail_scan: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }),
        SqliteIndex::open_in_memory().unwrap(),
        GitNet::new(f.repo_dir.clone(), f.vault_path.clone()),
        "p".into(),
    );
    app.reconcile(0).unwrap();
    assert_eq!(
        app.list(true).unwrap().len(),
        2,
        "both tasks start out cached"
    );

    incomplete.store(true, std::sync::atomic::Ordering::Relaxed);
    // An ordinary write, whose `refresh_cache` sees the partial scan.
    app.set_state(&b.key, "done").unwrap();

    let tasks = app.list(true).unwrap();
    assert!(
        tasks.iter().any(|t| t.uid == a.uid.as_str()),
        "an incomplete scan is not evidence a task is gone, got {tasks:?}"
    );
}

/// Finding 8: the reason a scan was rejected has to reach the user, or the
/// message names the wrong cause — "a large number of tasks disappeared"
/// when the truth is one file Cadet could not open.
#[cfg(unix)]
#[test]
fn an_unreadable_file_rejects_the_scan_as_incomplete_not_as_a_mass_deletion() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    f.app.add("locked").unwrap();
    f.app.add("other").unwrap();
    f.app.reconcile(0).unwrap();

    let locked = f.vault_path.join("locked.md");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = f.app.reconcile(1_000).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(r.scan_rejected, Some(RejectReason::Incomplete));
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

/// A hand-written task file, uid and key exactly as given — including a key
/// the project's own prefix does not cover, or one that does not parse at
/// all. `App::add` cannot produce these; a sync from another vault, a
/// hand-edited note, or a restored backup can.
fn write_raw_task(
    dir: &std::path::Path,
    name: &str,
    uid: &str,
    key: &str,
    title: &str,
    created: &str,
) {
    std::fs::write(
        dir.join(name),
        format!(
            "---\nuid: {uid}\nkey: {key}\ntitle: {title}\nstate: todo\ncreated: {created}\nupdated: {created}\n---\n"
        ),
    )
    .unwrap();
}

fn uid_str(n: u8) -> String {
    format!("01ARZ3NDEKTSV4RRFFQ69G5F{n:02}")
}

/// D1c: a genuine duplicate key plus any incomplete snapshot. The
/// `!complete` early return in `renumber_duplicate_keys` skipped resolution
/// entirely while `cache_tasks` stayed unconditional, so `ls` — a pure read
/// — died with a raw SQLite error and deleting the index did not recover,
/// because the fresh index IS the failing state.
#[cfg(unix)]
#[test]
fn a_duplicate_key_under_an_incomplete_scan_does_not_kill_every_read() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    write_raw_task(
        &f.vault_path,
        "dup-a.md",
        &uid_str(1),
        "P-1",
        "dup a",
        "2026-01-01T00:00:00Z",
    );
    write_raw_task(
        &f.vault_path,
        "dup-b.md",
        &uid_str(2),
        "P-1",
        "dup b",
        "2026-01-02T00:00:00Z",
    );
    write_raw_task(
        &f.vault_path,
        "locked.md",
        &uid_str(3),
        "P-9",
        "locked",
        "2026-01-03T00:00:00Z",
    );
    let locked = f.vault_path.join("locked.md");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let result = f.app.reconcile(0);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        result.is_ok(),
        "a read must never be killed by data the backend legitimately contains: {result:?}"
    );

    let tasks = f.app.list(true).unwrap();
    assert_eq!(
        tasks.len(),
        2,
        "both duplicates must be listed, got {tasks:?}"
    );
    let mut keys: Vec<String> = tasks.iter().map(|t| t.key.to_string()).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        2,
        "the duplicate key must have been resolved, got {tasks:?}"
    );
}

/// D1a: two files carrying a key under a prefix this project does not own.
/// `collision_candidates` filtered them out of resolution, but `cache_tasks`
/// still had to store both. Reproduces only when the files carry a `uid` —
/// without one they are adopted, and `Backend::adopt` restamps the key under
/// the project prefix before the conflict is ever reached.
#[test]
fn two_files_sharing_a_foreign_prefix_key_do_not_kill_every_read() {
    let f = fixture();
    write_raw_task(
        &f.vault_path,
        "f1.md",
        &uid_str(4),
        "OTHER-1",
        "foreign one",
        "2026-01-01T00:00:00Z",
    );
    write_raw_task(
        &f.vault_path,
        "f2.md",
        &uid_str(5),
        "OTHER-1",
        "foreign two",
        "2026-01-02T00:00:00Z",
    );

    assert!(
        f.app.reconcile(0).is_ok(),
        "a foreign-prefix duplicate must not abort the read"
    );
    let tasks = f.app.list(true).unwrap();
    assert_eq!(tasks.len(), 2, "got {tasks:?}");
    let mut keys: Vec<String> = tasks.iter().map(|t| t.key.to_string()).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        2,
        "the duplicate key must have been resolved, got {tasks:?}"
    );
}

/// D1b: two files whose `key:` does not parse. Both read back as the `?-0`
/// placeholder, so both were excluded from resolution and both landed in
/// `cache_tasks` under `?-0`.
#[test]
fn two_files_with_an_unparseable_key_do_not_kill_every_read() {
    let f = fixture();
    write_raw_task(
        &f.vault_path,
        "b1.md",
        &uid_str(6),
        "not a key!!",
        "broken one",
        "2026-01-01T00:00:00Z",
    );
    write_raw_task(
        &f.vault_path,
        "b2.md",
        &uid_str(7),
        "also bad!!",
        "broken two",
        "2026-01-02T00:00:00Z",
    );

    assert!(
        f.app.reconcile(0).is_ok(),
        "an unparseable duplicate key must not abort the read"
    );
    let tasks = f.app.list(true).unwrap();
    assert_eq!(tasks.len(), 2, "got {tasks:?}");
    let mut keys: Vec<String> = tasks.iter().map(|t| t.key.to_string()).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        2,
        "the duplicate placeholder key must have been resolved, got {tasks:?}"
    );
}

/// D2: `cp -p` of any note made every mutating command exit 1 AFTER its
/// write had already landed. `reconcile_with` drops a `PendingCopy` from
/// `parsed`; `refresh_cache` had no identity pass at all, so both files
/// reached `cache_tasks` under one uid.
#[test]
fn a_duplicated_uid_does_not_fail_a_write_that_already_landed() {
    let f = fixture();
    let alpha = f.app.add("alpha").unwrap();
    f.app.add("beta").unwrap();
    // The index has to know `alpha.md` BEFORE the copy appears, or this is
    // the cold-index case instead, where the copy legitimately keeps the uid
    // (the §5 limitation recorded in the spec, not this regression).
    f.app.reconcile(0).unwrap();
    // `cp -p`: the copy carries alpha's uid. Retitled afterwards purely so
    // the two are told apart below — `alpha-copy.md` sorts BEFORE `alpha.md`
    // ('-' < '.'), so a resolver that just takes the first path silently
    // hands the original's row to the copy.
    let original = std::fs::read_to_string(f.vault_path.join("alpha.md")).unwrap();
    std::fs::write(
        f.vault_path.join("alpha-copy.md"),
        original.replace("title: alpha", "title: alpha copy"),
    )
    .unwrap();
    f.app.reconcile(0).unwrap();

    let gamma = f.app.add("gamma");
    assert!(
        gamma.is_ok(),
        "the write landed; it must not report failure: {gamma:?}"
    );

    let beta_key = cadet_core::TaskKey::new("P", 2);
    let removed = f.app.delete(&beta_key);
    assert!(
        removed.is_ok(),
        "the delete landed; it must not report failure: {removed:?}"
    );

    let tasks = f.app.list(true).unwrap();
    let titles: Vec<&str> = tasks.iter().map(|t| t.title.as_str()).collect();
    assert!(titles.contains(&"gamma"), "got {titles:?}");
    assert!(!titles.contains(&"beta"), "got {titles:?}");
    // The file the index already records for that uid keeps the row. A bare
    // refresh that chose otherwise would disagree with the next reconcile
    // and the two would swap the row on alternate commands.
    assert!(
        titles.contains(&"alpha"),
        "the original must keep its row, got {titles:?}"
    );
    assert!(
        !titles.contains(&"alpha copy"),
        "the copy must not take it, got {titles:?}"
    );
    assert_eq!(f.app.get_by_key(&alpha.key).unwrap().uid, alpha.uid);
}

/// D4: `add`/`done`/`rm` run backend write → index update → cache refresh.
/// The first two are durable by the time the third runs, so a failure there
/// is a stale display, not a failed operation. Reporting it as one made
/// every retry duplicate the work — the exact failure `commit_or_warn` was
/// written to prevent on the git side.
#[test]
fn a_failed_cache_refresh_warns_instead_of_failing_a_durable_write() {
    let f = fixture();
    let fail_scan = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app = App::new(
        Box::new(IncompleteBackend {
            inner: FsBackend::new(f.vault_path.clone()),
            incomplete: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_scan: std::sync::Arc::clone(&fail_scan),
        }),
        SqliteIndex::open_in_memory().unwrap(),
        GitNet::new(f.repo_dir.clone(), f.vault_path.clone()),
        "p".into(),
    );

    fail_scan.store(true, std::sync::atomic::Ordering::Relaxed);
    let added = app.add("durable");
    assert!(
        added.is_ok(),
        "the file was written before the refresh ran; the command must not report failure: {added:?}"
    );
    assert!(
        f.vault_path.join("durable.md").exists(),
        "the write really did land"
    );
    let warnings = app.drain_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("could not be refreshed")),
        "the failure must still reach the user, got {warnings:?}"
    );
}

/// D3, the eighth instance of the same signature: `renumber_duplicate_keys`
/// cleared `pending_renumbers` only for paths still in `parsed`, and only
/// under a complete scan, while `forget` cleared three of five tables. The
/// row for a file that is simply gone had nothing left to reap it. Growth is
/// the mild half; the sharp half is that a byte-identical file restored at
/// the same path inherits a countdown that is already satisfied.
#[test]
fn a_pending_renumber_row_does_not_outlive_its_file() {
    let f = fixture();
    f.app.add("keeper").unwrap();
    // Created LATER than `keeper`, so §5 makes `dup.md` the loser and the
    // `pending_renumbers` row is the one that names it. With the timestamps
    // the other way round the row lands on `keeper.md`, which stays on disk
    // and is cleared by the ordinary settled path — the reaper never runs.
    std::fs::write(
        f.vault_path.join("dup.md"),
        "---\nuid: 01ARZ3NDEKTSV4RRFFQ69G5F42\nkey: P-1\ntitle: dup\nstate: todo\ncreated: 2099-01-01T00:00:00Z\nupdated: 2099-01-01T00:00:00Z\n---\n",
    )
    .unwrap();
    f.app.reconcile(0).unwrap();
    assert_eq!(
        f.app.renumber_status().unwrap().pending,
        1,
        "the collision must be on record before the reap can be tested"
    );

    std::fs::remove_file(f.vault_path.join("dup.md")).unwrap();
    for now in [1_000, 2_000, 3_000] {
        f.app.reconcile(now).unwrap();
    }
    assert_eq!(
        f.app.renumber_status().unwrap().pending,
        0,
        "a pending_renumbers row must not outlive the file it names"
    );
}

/// D7: `report.renumbered` counts what THIS reconcile pass did, and the pass
/// that does the renumbering is whichever command reconciles first — almost
/// never `doctor`. The user's diagnostic therefore reported 0 for a renumber
/// that had demonstrably happened.
#[test]
fn doctor_reports_a_renumber_that_an_earlier_command_performed() {
    let f = fixture();
    f.app.add("keeper").unwrap();
    std::fs::write(
        f.vault_path.join("dup.md"),
        "---\nuid: 01ARZ3NDEKTSV4RRFFQ69G5F43\nkey: P-1\ntitle: dup\nstate: todo\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\n",
    )
    .unwrap();
    f.app.reconcile(0).unwrap();
    // The pass that actually renumbers — stand-in for the `cadet ls` the
    // user ran before thinking to ask `doctor`.
    let renumbering_pass = f.app.reconcile(60_001).unwrap();
    assert_eq!(
        renumbering_pass.renumbered, 1,
        "the renumber really happened"
    );

    // The `doctor` invocation itself: its own reconcile has nothing left to
    // do, so the per-pass counter is 0 and always will be.
    let doctor_pass = f.app.reconcile(60_002).unwrap();
    assert_eq!(doctor_pass.renumbered, 0);
    assert_eq!(
        f.app.renumber_status().unwrap().recorded,
        1,
        "doctor must report the renumber from its durable breadcrumb, not from its own pass"
    );
}

/// D9: the rebuild-path copy gets its fresh uid through `Backend::adopt`,
/// not the renumber path, so it recorded neither `renumbered_from` nor
/// `possible_duplicate_of` — a file silently lost its identity and said
/// nothing. `possible_duplicate_of` was dead plumbing: read back by the
/// backend, never written by production code.
#[test]
fn a_copy_given_a_fresh_identity_records_what_it_was_copied_from() {
    let f = fixture();
    let shared = "01ARZ3NDEKTSV4RRFFQ69G5F44";
    for name in ["a.md", "b.md"] {
        std::fs::write(
            f.vault_path.join(name),
            format!(
                "---\nuid: {shared}\nkey: P-1\ntitle: twin\nstate: todo\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\n"
            ),
        )
        .unwrap();
    }
    // `a.md` claims the uid; `b.md` is the copy and waits out the grace period.
    f.app.reconcile(0).unwrap();
    let r = f.app.reconcile(60_001).unwrap();
    assert_eq!(r.copies, 1, "the copy must have been given an identity");

    let copy = std::fs::read_to_string(f.vault_path.join("b.md")).unwrap();
    assert!(
        copy.contains(&format!("possible_duplicate_of: {shared}")),
        "the copy must record the identity it was split from:\n{copy}"
    );
    assert!(
        !copy.contains(&format!("uid: {shared}")),
        "the copy must have a fresh uid of its own:\n{copy}"
    );
    let kept = std::fs::read_to_string(f.vault_path.join("a.md")).unwrap();
    assert!(
        kept.contains(&format!("uid: {shared}")),
        "the original must keep the uid:\n{kept}"
    );
}

/// D8: `path_for` aborted on `Permission denied` and on `Malformed` where
/// `scan` degrades gracefully. `put` resolves a uid through `path_for`
/// first, so one unreadable note meant `cadet add` could never succeed again
/// anywhere in the project.
#[cfg(unix)]
#[test]
fn one_unreadable_file_does_not_block_every_write() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    f.app.add("existing").unwrap();

    let locked = f.vault_path.join("locked.md");
    std::fs::write(
        &locked,
        "---\nuid: 01ARZ3NDEKTSV4RRFFQ69G5F45\nkey: P-9\ntitle: locked\nstate: todo\n---\n",
    )
    .unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // A second bad file, unreadable for the other reason `scan` tolerates:
    // task-shaped, but with no uid `read_task` can parse.
    std::fs::write(
        f.vault_path.join("nouid.md"),
        "---\nkey: P-8\ntitle: no uid\nstate: todo\n---\n",
    )
    .unwrap();

    let added = f.app.add("new one");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        added.is_ok(),
        "one file Cadet cannot read must not block every write: {added:?}"
    );
    assert!(f.vault_path.join("new-one.md").exists());
}

#[test]
fn add_with_writes_every_field_to_the_file() {
    let f = fixture();
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("estimate".to_string(), cadet_core::FieldValue::Int(3));
    let t = f
        .app
        .add_with(cadet_app::TaskDraft {
            title: "full task".into(),
            state: Some("doing".into()),
            due: Some("2026-08-10".into()),
            priority: Some(cadet_core::Priority::High),
            tags: vec!["home".into(), "urgent".into()],
            fields,
        })
        .unwrap();

    assert_eq!(t.state, "doing");
    assert_eq!(t.due.as_deref(), Some("2026-08-10"));
    assert_eq!(t.tags, vec!["home".to_string(), "urgent".to_string()]);

    let src = std::fs::read_to_string(f.vault_path.join("full-task.md")).unwrap();
    assert!(src.contains("due: 2026-08-10"), "{src}");
    assert!(src.contains("estimate: 3"), "{src}");
    assert!(src.contains("urgent"), "{src}");
}

#[test]
fn update_changes_only_what_is_named() {
    let f = fixture();
    let t = f
        .app
        .add_with(cadet_app::TaskDraft {
            title: "keep most".into(),
            due: Some("2026-08-10".into()),
            tags: vec!["home".into()],
            ..Default::default()
        })
        .unwrap();

    let after = f
        .app
        .update(
            &t.key,
            cadet_app::TaskChanges {
                priority: Some(cadet_core::Priority::High),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(after.priority, cadet_core::Priority::High);
    assert_eq!(after.due.as_deref(), Some("2026-08-10"), "due must survive");
    assert_eq!(after.tags, vec!["home".to_string()], "tags must survive");
    assert_eq!(after.title, "keep most");
}

#[test]
fn update_can_clear_a_due_date_and_remove_a_field() {
    let f = fixture();
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("estimate".to_string(), cadet_core::FieldValue::Int(3));
    let t = f
        .app
        .add_with(cadet_app::TaskDraft {
            title: "clear me".into(),
            due: Some("2026-08-10".into()),
            fields,
            ..Default::default()
        })
        .unwrap();

    let mut changes = cadet_app::TaskChanges {
        due: Some(None),
        ..Default::default()
    };
    changes.fields.insert("estimate".to_string(), None);
    let after = f.app.update(&t.key, changes).unwrap();

    assert_eq!(after.due, None);
    assert!(!after.fields.contains_key("estimate"));
}

#[test]
fn set_state_still_works_and_is_now_an_update() {
    let f = fixture();
    let t = f.app.add("legacy path").unwrap();
    let after = f.app.set_state(&t.key, "doing").unwrap();
    assert_eq!(after.state, "doing");
}

#[test]
fn add_with_rejects_a_badly_formatted_due_date_and_writes_no_file() {
    let f = fixture();
    let err = f
        .app
        .add_with(cadet_app::TaskDraft {
            title: "bad due".into(),
            due: Some("2026-8-10".into()),
            ..Default::default()
        })
        .unwrap_err();
    assert!(err.to_string().contains("due"), "{err}");
    assert!(!f.vault_path.join("bad-due.md").exists());
}

/// Reviewer-caught regression: validating the *merged* `due` (rather than
/// only what the caller supplied in `changes.due`) means a task whose file
/// already carries a hand-edited bad date — something Cadet itself never
/// wrote, and `reconcile`/adoption never validates — becomes permanently
/// stuck: `set_state` (and therefore `cadet done`, `cadet mv`) would refuse
/// every future transition forever, with no CLI flag able to fix `due` to
/// get unstuck. The amendment's own words: "you cannot fix a file that
/// already contains a bad date, but you can stop Cadet writing one" — i.e.
/// validate only what's supplied, not what's merely carried forward.
#[test]
fn set_state_still_works_when_due_on_disk_is_already_malformed() {
    let f = fixture();
    let t = f.app.add("hand edited due").unwrap();
    let path = f.vault_path.join("hand-edited-due.md");
    let src = std::fs::read_to_string(&path).unwrap();
    let with_bad_due = src.replacen("state: todo\n", "state: todo\ndue: 2026-8-10\n", 1);
    assert_ne!(
        src, with_bad_due,
        "test setup must actually inject a bad due line"
    );
    std::fs::write(&path, with_bad_due).unwrap();

    let after = f
        .app
        .set_state(&t.key, "doing")
        .expect("a pre-existing bad `due` on disk must not block an unrelated transition");
    assert_eq!(after.state, "doing");
    assert_eq!(after.due.as_deref(), Some("2026-8-10"), "due is left alone");
}

#[test]
fn update_rejects_removing_a_field_the_project_never_declared() {
    let f = fixture();
    let t = f.app.add("undeclared field").unwrap();
    let path = f.vault_path.join("undeclared-field.md");
    let src = std::fs::read_to_string(&path).unwrap();
    let with_extra = src.replacen("state: todo\n", "state: todo\nmycolumn: hello\n", 1);
    std::fs::write(&path, with_extra).unwrap();

    let mut changes = cadet_app::TaskChanges::default();
    changes.fields.insert("mycolumn".to_string(), None);
    let err = f
        .app
        .update(&t.key, changes)
        .expect_err("removing an undeclared field must fail, not silently no-op");
    assert!(err.to_string().contains("mycolumn"), "{err}");

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("mycolumn: hello"),
        "the undeclared field must be untouched on disk: {after}"
    );
}

#[test]
fn a_no_op_update_does_not_write_or_commit() {
    let f = fixture();
    let t = f.app.add("no-op test").unwrap();
    let path = f.vault_path.join("no-op-test.md");
    let before = std::fs::read_to_string(&path).unwrap();
    let commits_before = commit_count(&f.repo_dir, &f.vault_path);

    let after = f
        .app
        .update(&t.key, cadet_app::TaskChanges::default())
        .unwrap();

    assert_eq!(
        after.updated, t.updated,
        "a change set carrying nothing must not bump `updated`"
    );
    let src = std::fs::read_to_string(&path).unwrap();
    assert_eq!(src, before, "a no-op update must not rewrite the file");
    assert_eq!(
        commit_count(&f.repo_dir, &f.vault_path),
        commits_before,
        "a no-op update must not create a git commit"
    );
}

#[test]
fn list_filtered_narrows_by_tag_and_state() {
    let f = fixture();
    f.app
        .add_with(cadet_app::TaskDraft {
            title: "home one".into(),
            tags: vec!["home".into()],
            ..Default::default()
        })
        .unwrap();
    f.app
        .add_with(cadet_app::TaskDraft {
            title: "work one".into(),
            tags: vec!["work".into()],
            ..Default::default()
        })
        .unwrap();

    let filter = cadet_core::TaskFilter {
        tags: vec!["home".into()],
        ..Default::default()
    };
    let got = f.app.list_filtered(false, &filter).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title, "home one");
}

#[test]
fn list_filtered_with_an_empty_filter_equals_list() {
    let f = fixture();
    f.app.add("a").unwrap();
    f.app.add("b").unwrap();
    let plain = f.app.list(false).unwrap();
    let filtered = f
        .app
        .list_filtered(false, &cadet_core::TaskFilter::default())
        .unwrap();
    assert_eq!(plain.len(), filtered.len());
    assert_eq!(plain.len(), 2);
}

#[test]
fn list_filtered_names_a_terminal_state_explicitly_and_sees_it_without_all() {
    let f = fixture();
    f.app
        .add_with(cadet_app::TaskDraft {
            title: "finished one".into(),
            state: Some("done".into()),
            ..Default::default()
        })
        .unwrap();

    let filter = cadet_core::TaskFilter {
        states: vec!["done".into()],
        ..Default::default()
    };
    let got = f.app.list_filtered(false, &filter).unwrap();
    assert_eq!(got.len(), 1, "naming a terminal state must surface it");
    assert_eq!(got[0].title, "finished one");
}

#[test]
fn bare_list_still_hides_terminal_states_without_all() {
    let f = fixture();
    f.app
        .add_with(cadet_app::TaskDraft {
            title: "finished one".into(),
            state: Some("done".into()),
            ..Default::default()
        })
        .unwrap();
    f.app.add("open one").unwrap();

    let got = f.app.list(false).unwrap();
    assert_eq!(
        got.len(),
        1,
        "an unfiltered list must still hide terminal states"
    );
    assert_eq!(got[0].title, "open one");
}

fn commit_count(repo_dir: &std::path::Path, work_tree: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo_dir)
        .arg("--work-tree")
        .arg(work_tree)
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
