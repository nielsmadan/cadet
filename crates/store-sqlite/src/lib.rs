pub mod schema;

use cadet_core::{IndexEntry, IndexView, Priority, Revision, Task, TaskKey, TaskUid};
use rusqlite::{Connection, params};

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// What a list view needs. Deliberately not a full `Task`: rendering a list
/// must never require the body or custom fields, so it never requires a file read.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSummary {
    pub uid: String,
    pub key: TaskKey,
    pub title: String,
    pub state: String,
    pub due: Option<String>,
    pub priority: Priority,
}

pub struct SqliteIndex {
    conn: Connection,
}

impl SqliteIndex {
    pub fn open(path: &std::path::Path) -> Result<Self, IndexError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(schema::DDL)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self, IndexError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(schema::DDL)?;
        Ok(Self { conn })
    }

    pub fn view(&self, project: &str) -> Result<IndexView, IndexError> {
        let mut v = IndexView::default();

        let mut st = self
            .conn
            .prepare("SELECT uid, path, revision, first_seen_ms FROM entries WHERE project = ?1")?;
        let rows = st.query_map(params![project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (uid, path, rev, seen) = row?;
            if let Some(uid) = TaskUid::parse(&uid) {
                v.entries.push(IndexEntry {
                    uid,
                    path,
                    revision: Revision::from_raw(rev),
                    first_seen_ms: seen,
                });
            }
        }

        let mut st = self
            .conn
            .prepare("SELECT path, revision, since_ms FROM pending WHERE project = ?1")?;
        let rows = st.query_map(params![project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (path, rev, since) = row?;
            v.pending.insert(path, (Revision::from_raw(rev), since));
        }

        let mut st = self
            .conn
            .prepare("SELECT uid, since_ms FROM pending_deletions WHERE project = ?1")?;
        let rows = st.query_map(params![project], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (uid, since) = row?;
            if let Some(uid) = TaskUid::parse(&uid) {
                v.pending_deletions.insert(uid, since);
            }
        }

        Ok(v)
    }

    /// Replaces the project's entries wholesale — the index is disposable and
    /// always rebuilt from a complete snapshot.
    pub fn apply(&self, project: &str, entries: &[IndexEntry]) -> Result<(), IndexError> {
        self.conn
            .execute("DELETE FROM entries WHERE project = ?1", params![project])?;
        for e in entries {
            self.conn.execute(
                "INSERT OR REPLACE INTO entries (project, uid, path, revision, first_seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    project,
                    e.uid.as_str(),
                    e.path,
                    e.revision.as_str(),
                    e.first_seen_ms
                ],
            )?;
        }
        Ok(())
    }

    /// Records a note as pending adoption. Unlike a plain upsert, `since_ms`
    /// is preserved across repeated calls with the same `revision` — a
    /// reconcile run every time `cadet ls` is called must not restart the
    /// grace-period countdown on every poll, or a note that changes hands
    /// less often than the polling interval never finishes adopting. A
    /// genuinely changed revision still resets the clock: that's the
    /// "changed content restarts the grace period" rule (§5), preserved by
    /// the `WHERE` clause only firing when the revision actually differs.
    pub fn mark_pending(
        &self,
        project: &str,
        path: &str,
        rev: &Revision,
        since_ms: i64,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO pending (project, path, revision, since_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(project, path) DO UPDATE SET
                 revision = excluded.revision,
                 since_ms = excluded.since_ms
             WHERE pending.revision != excluded.revision",
            params![project, path, rev.as_str(), since_ms],
        )?;
        Ok(())
    }

    pub fn mark_pending_deletion(
        &self,
        project: &str,
        uid: &TaskUid,
        since_ms: i64,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO pending_deletions (project, uid, since_ms) VALUES (?1, ?2, ?3)",
            params![project, uid.as_str(), since_ms],
        )?;
        Ok(())
    }

    /// Called whenever a uid is reclaimed by an `Update` or `Rename`. Without this,
    /// a task that vanishes, returns, then vanishes again is deleted with no grace
    /// period at all, because the original absence timestamp is still on record.
    pub fn clear_pending_deletion(&self, project: &str, uid: &TaskUid) -> Result<(), IndexError> {
        self.conn.execute(
            "DELETE FROM pending_deletions WHERE project = ?1 AND uid = ?2",
            params![project, uid.as_str()],
        )?;
        Ok(())
    }

    /// Called whenever a path is claimed by an `Outcome::Adopt`. Without this,
    /// a path that is adopted, deleted, and later recreated inherits the stale
    /// `since_ms` from the first observation — paths, unlike uids, are reused
    /// — so the grace-period check is trivially satisfied and the newcomer is
    /// adopted immediately with no grace period at all, defeating the "never
    /// mutate on first observation" rule that exists to stop a rename delivered
    /// mid-sync from being written to.
    pub fn clear_pending(&self, project: &str, path: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "DELETE FROM pending WHERE project = ?1 AND path = ?2",
            params![project, path],
        )?;
        Ok(())
    }

    pub fn high_water(&self, project: &str) -> Result<u32, IndexError> {
        match self.conn.query_row(
            "SELECT value FROM high_water WHERE project = ?1",
            params![project],
            |r| r.get::<_, i64>(0),
        ) {
            Ok(v) => Ok(v as u32),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    /// Monotonic: keys are never reused, so this only ever increases (§5).
    pub fn bump_high_water(&self, project: &str, value: u32) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO high_water (project, value) VALUES (?1, ?2)
             ON CONFLICT(project) DO UPDATE SET value = MAX(value, excluded.value)",
            params![project, value as i64],
        )?;
        Ok(())
    }

    /// Replaces the cached display data for a project. Called by reconcile with
    /// the tasks the scan already parsed — no extra file reads.
    pub fn cache_tasks(&self, project: &str, tasks: &[Task]) -> Result<(), IndexError> {
        self.conn
            .execute("DELETE FROM tasks WHERE project = ?1", params![project])?;
        for t in tasks {
            self.conn.execute(
                "INSERT OR REPLACE INTO tasks
                 (project, uid, key_num, key_prefix, title, state, due, priority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    project,
                    t.uid.as_str(),
                    t.key.number as i64,
                    t.key.prefix,
                    t.title,
                    t.state,
                    t.due,
                    match t.priority {
                        Priority::High => 0i64,
                        Priority::Normal => 1,
                        Priority::Low => 2,
                    },
                ],
            )?;
        }
        Ok(())
    }

    fn row_to_summary(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSummary> {
        Ok(TaskSummary {
            uid: r.get::<_, String>(0)?,
            key: TaskKey::new(r.get::<_, String>(3)?, r.get::<_, i64>(2)? as u32),
            title: r.get(4)?,
            state: r.get(5)?,
            due: r.get(6)?,
            priority: match r.get::<_, i64>(7)? {
                0 => Priority::High,
                2 => Priority::Low,
                _ => Priority::Normal,
            },
        })
    }

    /// The list view. One query — never touches the backend (spec §3).
    /// `terminal` names the states excluded unless `all` is set.
    pub fn list_tasks(
        &self,
        project: &str,
        all: bool,
        terminal: &[String],
    ) -> Result<Vec<TaskSummary>, IndexError> {
        let mut st = self.conn.prepare(
            "SELECT uid, project, key_num, key_prefix, title, state, due, priority
             FROM tasks WHERE project = ?1
             ORDER BY due IS NULL, due, priority, key_num",
        )?;
        let rows = st.query_map(params![project], Self::row_to_summary)?;
        let mut out = Vec::new();
        for row in rows {
            let s = row?;
            if all || !terminal.iter().any(|t| t == &s.state) {
                out.push(s);
            }
        }
        Ok(out)
    }

    pub fn find_by_key(
        &self,
        project: &str,
        key: &TaskKey,
    ) -> Result<Option<TaskSummary>, IndexError> {
        let mut st = self.conn.prepare(
            "SELECT uid, project, key_num, key_prefix, title, state, due, priority
             FROM tasks WHERE project = ?1 AND key_num = ?2 AND key_prefix = ?3",
        )?;
        let mut rows = st.query_map(
            params![project, key.number as i64, key.prefix],
            Self::row_to_summary,
        )?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Removes a single uid from `entries`, `tasks`, and `pending_deletions`
    /// in one call — the narrow, uid-scoped counterpart to `clear` (which is
    /// project-wide). For when the caller already knows, with certainty,
    /// that this exact task is gone (an explicit delete): there is nothing
    /// left to infer, so nothing else in the project is touched.
    pub fn forget(&self, project: &str, uid: &TaskUid) -> Result<(), IndexError> {
        for table in ["entries", "tasks", "pending_deletions"] {
            self.conn.execute(
                &format!("DELETE FROM {table} WHERE project = ?1 AND uid = ?2"),
                params![project, uid.as_str()],
            )?;
        }
        Ok(())
    }

    /// Wipes the derived view. High-water marks deliberately survive.
    pub fn clear(&self, project: &str) -> Result<(), IndexError> {
        for t in ["entries", "pending", "pending_deletions", "tasks"] {
            self.conn.execute(
                &format!("DELETE FROM {t} WHERE project = ?1"),
                params![project],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadet_core::{IndexEntry, Revision, TaskUid};

    fn idx() -> SqliteIndex {
        SqliteIndex::open_in_memory().unwrap()
    }
    fn entry(path: &str) -> IndexEntry {
        IndexEntry {
            uid: TaskUid::generate(),
            path: path.into(),
            revision: Revision::from_raw("r1"),
            first_seen_ms: 0,
        }
    }

    #[test]
    fn round_trips_entries() {
        let i = idx();
        let e = entry("a.md");
        i.apply("p", std::slice::from_ref(&e)).unwrap();
        let v = i.view("p").unwrap();
        assert_eq!(v.entries.len(), 1);
        assert_eq!(v.entries[0].path, "a.md");
        assert_eq!(v.entries[0].uid, e.uid);
    }

    #[test]
    fn apply_replaces_the_whole_project_view() {
        let i = idx();
        i.apply("p", &[entry("a.md"), entry("b.md")]).unwrap();
        i.apply("p", &[entry("c.md")]).unwrap();
        assert_eq!(i.view("p").unwrap().entries.len(), 1);
    }

    #[test]
    fn high_water_only_ever_increases() {
        let i = idx();
        assert_eq!(i.high_water("p").unwrap(), 0);
        i.bump_high_water("p", 7).unwrap();
        i.bump_high_water("p", 3).unwrap();
        assert_eq!(i.high_water("p").unwrap(), 7);
    }

    #[test]
    fn projects_are_isolated() {
        let i = idx();
        i.apply("a", &[entry("x.md")]).unwrap();
        i.bump_high_water("a", 5).unwrap();
        assert_eq!(i.view("b").unwrap().entries.len(), 0);
        assert_eq!(i.high_water("b").unwrap(), 0);
    }

    #[test]
    fn pending_adoptions_round_trip() {
        let i = idx();
        i.mark_pending("p", "new.md", &Revision::from_raw("r1"), 123)
            .unwrap();
        let v = i.view("p").unwrap();
        assert_eq!(v.pending.get("new.md").unwrap().1, 123);
    }

    #[test]
    fn marking_pending_again_with_the_same_revision_preserves_since_ms() {
        let i = idx();
        i.mark_pending("p", "new.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        // Simulates a later poll (e.g. another `cadet ls`) observing the
        // same, unchanged file — must not restart the grace-period clock.
        i.mark_pending("p", "new.md", &Revision::from_raw("r1"), 999_000)
            .unwrap();
        let v = i.view("p").unwrap();
        assert_eq!(
            v.pending.get("new.md").unwrap().1,
            100,
            "an unchanged revision must not reset since_ms"
        );
    }

    #[test]
    fn marking_pending_with_a_changed_revision_resets_since_ms() {
        let i = idx();
        i.mark_pending("p", "new.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending("p", "new.md", &Revision::from_raw("r2"), 999_000)
            .unwrap();
        let v = i.view("p").unwrap();
        assert_eq!(
            v.pending.get("new.md").unwrap().1,
            999_000,
            "a genuinely changed revision must restart the grace period"
        );
        assert_eq!(v.pending.get("new.md").unwrap().0, Revision::from_raw("r2"));
    }

    #[test]
    fn clear_wipes_a_project_but_keeps_high_water() {
        let i = idx();
        i.apply("p", &[entry("a.md")]).unwrap();
        i.cache_tasks("p", &[task(1, "x", "todo", None)]).unwrap();
        i.bump_high_water("p", 9).unwrap();
        i.clear("p").unwrap();
        assert_eq!(i.view("p").unwrap().entries.len(), 0);
        assert_eq!(i.list_tasks("p", true, &[]).unwrap().len(), 0);
        assert_eq!(
            i.high_water("p").unwrap(),
            9,
            "high-water must survive an index rebuild"
        );
    }

    #[test]
    fn forget_removes_one_uid_from_every_table_but_leaves_others_untouched() {
        let i = idx();
        let gone = entry("gone.md");
        let other = entry("other.md");
        i.apply("p", &[gone.clone(), other.clone()]).unwrap();
        i.cache_tasks(
            "p",
            &[
                task_with_uid(gone.uid.clone(), 1, "gone"),
                task_with_uid(other.uid.clone(), 2, "other"),
            ],
        )
        .unwrap();
        i.mark_pending_deletion("p", &gone.uid, 100).unwrap();
        i.mark_pending_deletion("p", &other.uid, 100).unwrap();

        i.forget("p", &gone.uid).unwrap();

        let v = i.view("p").unwrap();
        assert!(
            !v.entries.iter().any(|e| e.uid == gone.uid),
            "forget must remove the entries row"
        );
        assert!(
            v.entries.iter().any(|e| e.uid == other.uid),
            "forget must not touch a different uid's entry"
        );
        assert!(!v.pending_deletions.contains_key(&gone.uid));
        assert!(v.pending_deletions.contains_key(&other.uid));

        let tasks = i.list_tasks("p", true, &[]).unwrap();
        assert!(!tasks.iter().any(|t| t.uid == gone.uid.as_str()));
        assert!(tasks.iter().any(|t| t.uid == other.uid.as_str()));
    }

    fn task(num: u32, title: &str, state: &str, due: Option<&str>) -> cadet_core::Task {
        cadet_core::Task {
            uid: TaskUid::generate(),
            key: TaskKey::new("P", num),
            title: title.into(),
            state: state.into(),
            created: jiff::Timestamp::UNIX_EPOCH,
            updated: jiff::Timestamp::UNIX_EPOCH,
            due: due.map(str::to_string),
            priority: Priority::Normal,
            tags: vec![],
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: Default::default(),
            body: String::new(),
        }
    }

    fn task_with_uid(uid: TaskUid, num: u32, title: &str) -> cadet_core::Task {
        cadet_core::Task {
            uid,
            ..task(num, title, "todo", None)
        }
    }

    #[test]
    fn list_tasks_hides_terminal_states_unless_all_is_set() {
        let i = idx();
        i.cache_tasks(
            "p",
            &[
                task(1, "open", "todo", None),
                task(2, "closed", "done", None),
            ],
        )
        .unwrap();
        let terminal = vec!["done".to_string()];
        assert_eq!(i.list_tasks("p", false, &terminal).unwrap().len(), 1);
        assert_eq!(i.list_tasks("p", true, &terminal).unwrap().len(), 2);
    }

    #[test]
    fn list_tasks_sorts_by_due_then_priority_then_key() {
        let i = idx();
        i.cache_tasks(
            "p",
            &[
                task(3, "no due", "todo", None),
                task(1, "later", "todo", Some("2026-09-01")),
                task(2, "sooner", "todo", Some("2026-08-01")),
            ],
        )
        .unwrap();
        let titles: Vec<_> = i
            .list_tasks("p", true, &[])
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(
            titles,
            vec!["sooner", "later", "no due"],
            "undated tasks sort last"
        );
    }

    #[test]
    fn find_by_key_returns_one_task_or_none() {
        let i = idx();
        i.cache_tasks("p", &[task(4, "target", "todo", None)])
            .unwrap();
        assert_eq!(
            i.find_by_key("p", &TaskKey::new("P", 4))
                .unwrap()
                .unwrap()
                .title,
            "target"
        );
        assert!(i.find_by_key("p", &TaskKey::new("P", 9)).unwrap().is_none());
    }

    #[test]
    fn cache_tasks_replaces_rather_than_accumulating() {
        let i = idx();
        i.cache_tasks(
            "p",
            &[task(1, "a", "todo", None), task(2, "b", "todo", None)],
        )
        .unwrap();
        i.cache_tasks("p", &[task(1, "a", "todo", None)]).unwrap();
        assert_eq!(i.list_tasks("p", true, &[]).unwrap().len(), 1);
    }

    #[test]
    fn high_water_propagates_a_real_error_instead_of_returning_zero() {
        let i = idx();
        i.conn.execute_batch("DROP TABLE high_water;").unwrap();
        assert!(
            i.high_water("p").is_err(),
            "a real SQL error must not be reported as high_water 0"
        );
    }

    #[test]
    fn high_water_is_zero_for_an_unknown_project() {
        let i = idx();
        assert_eq!(i.high_water("nonexistent").unwrap(), 0);
    }

    #[test]
    fn clear_pending_removes_only_the_named_path() {
        let i = idx();
        i.mark_pending("p", "a.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending("p", "b.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.clear_pending("p", "a.md").unwrap();
        let v = i.view("p").unwrap();
        assert!(!v.pending.contains_key("a.md"));
        assert!(v.pending.contains_key("b.md"));
    }

    #[test]
    fn a_reused_path_does_not_inherit_a_stale_pending_timestamp() {
        let i = idx();
        i.mark_pending("p", "new.md", &Revision::from_raw("r1"), 0)
            .unwrap();
        i.clear_pending("p", "new.md").unwrap();
        i.mark_pending("p", "new.md", &Revision::from_raw("r2"), 500_000)
            .unwrap();
        let v = i.view("p").unwrap();
        assert_eq!(v.pending.get("new.md").unwrap().1, 500_000);
    }
}
