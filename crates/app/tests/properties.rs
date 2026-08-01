use cadet_app::*;
use cadet_backend_fs::FsBackend;
use cadet_store_sqlite::SqliteIndex;
use proptest::prelude::*;

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

#[derive(Debug, Clone)]
enum Op {
    Add(String),
    Done(usize),
    Remove(usize),
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            "[a-z]{3,12}".prop_map(Op::Add),
            (0usize..8).prop_map(Op::Done),
            (0usize..8).prop_map(Op::Remove),
        ],
        1..12,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Spec §9.2 — the invariant the whole architecture rests on.
    #[test]
    fn index_rebuild_is_lossless(ops in ops()) {
        let vault = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(vault.path().join("project.toml"), CFG).unwrap();
        let git = GitNet::new(repo.path().join("r.git"), vault.path().to_path_buf());
        git.ensure_init().unwrap();
        let app = App::new(
            Box::new(FsBackend::new(vault.path().to_path_buf())),
            SqliteIndex::open_in_memory().unwrap(),
            git,
            "p".into(),
        );

        for op in ops {
            let live = app.list(true).unwrap();
            match op {
                Op::Add(title) => { let _ = app.add(&title); }
                Op::Done(i) => {
                    if let Some(t) = live.get(i % live.len().max(1)) {
                        let _ = app.set_state(&t.key, "done");
                    }
                }
                Op::Remove(i) => {
                    if let Some(t) = live.get(i % live.len().max(1)) {
                        let _ = app.delete(&t.key);
                    }
                }
            }
        }

        let before = app.list(true).unwrap();
        app.clear_index().unwrap();
        app.reconcile(0).unwrap();
        let after = app.list(true).unwrap();

        prop_assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(after.iter()) {
            prop_assert_eq!(&a.uid, &b.uid);
            prop_assert_eq!(&a.key, &b.key);
            prop_assert_eq!(&a.title, &b.title);
            prop_assert_eq!(&a.state, &b.state);
        }
    }
}
