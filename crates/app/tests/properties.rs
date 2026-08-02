use cadet_app::*;
use cadet_backend_markdown::MarkdownBackend;
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

    /// Spec §9.2 — the invariant the whole architecture rests on: *delete the
    /// local index, point at the same backend, get everything back*.
    ///
    /// The index is deleted the way a user would delete it — the file is
    /// removed and a new one opened. `App::clear_index` is deliberately NOT
    /// used: it preserves the high-water mark by design, so it cannot
    /// exercise the rebuild-from-the-backend-alone path at all. And the
    /// rebuilt index has to keep working afterwards, so the property runs a
    /// mint after the rebuild and asserts every key is still distinct.
    #[test]
    fn index_rebuild_is_lossless(ops in ops()) {
        let vault = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let index_path = home.path().join("index.db");
        std::fs::write(vault.path().join("project.toml"), CFG).unwrap();
        let git = || GitNet::new(repo.path().join("r.git"), vault.path().to_path_buf());
        git().ensure_init().unwrap();
        let open = |index: SqliteIndex| App::new(
            Box::new(MarkdownBackend::new(vault.path().to_path_buf())),
            index,
            git(),
            "p".into(),
        );
        let app = open(SqliteIndex::open(&index_path).unwrap());

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
        drop(app);
        std::fs::remove_file(&index_path).unwrap();

        let rebuilt = open(SqliteIndex::open(&index_path).unwrap());
        rebuilt.reconcile(0).unwrap();
        let after = rebuilt.list(true).unwrap();

        prop_assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(after.iter()) {
            prop_assert_eq!(&a.uid, &b.uid);
            prop_assert_eq!(&a.key, &b.key);
            prop_assert_eq!(&a.title, &b.title);
            prop_assert_eq!(&a.state, &b.state);
        }

        // The rebuilt index must still allocate correctly: a key handed out
        // now may not collide with one already on disk (spec §5). Asserted
        // against the FILES, not the index — the index resolves duplicate
        // keys as it caches them, so it is exactly the wrong place to look
        // for one.
        rebuilt.add("after the rebuild").unwrap();
        let mut keys: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(vault.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            if let Some(line) = raw.lines().find(|l| l.starts_with("key: ")) {
                keys.push(line["key: ".len()..].to_string());
            }
        }
        let unique: std::collections::BTreeSet<&String> = keys.iter().collect();
        prop_assert_eq!(
            unique.len(),
            keys.len(),
            "a mint after the rebuild reused a key: {:?}",
            keys
        );

        let all = rebuilt.list(true).unwrap();
        prop_assert_eq!(all.len(), keys.len(), "every task on disk must be listed");
        let uids: std::collections::BTreeSet<_> = all.iter().map(|t| t.uid.clone()).collect();
        prop_assert_eq!(uids.len(), all.len(), "a rebuild collapsed two tasks into one");
    }
}
