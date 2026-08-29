pub mod schema;

use cadet_core::{
    Cursor, FieldValue, IndexEntry, IndexView, Priority, Revision, Task, TaskKey, TaskUid,
};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

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
    pub tags: Vec<String>,
    pub fields: BTreeMap<String, FieldValue>,
}

fn encode_field(v: &FieldValue) -> (&'static str, String) {
    match v {
        FieldValue::Str(s) => ("str", s.clone()),
        FieldValue::Int(i) => ("int", i.to_string()),
        FieldValue::Float(f) => ("float", f.to_string()),
        FieldValue::Bool(b) => ("bool", b.to_string()),
        FieldValue::Date(d) => ("date", d.clone()),
        // A bare `join` collapses `[]` and `[""]` to the same string ("").
        // Reserve "" for the empty list and prefix every non-empty list with
        // a leading separator, so `[""]` encodes as "\u{1f}" and decodes back
        // to one empty-string element instead of zero elements.
        FieldValue::List(items) => {
            if items.is_empty() {
                ("list", String::new())
            } else {
                ("list", format!("\u{1f}{}", items.join("\u{1f}")))
            }
        }
    }
}

fn decode_field(kind: &str, raw: &str) -> FieldValue {
    match kind {
        "int" => raw
            .parse()
            .map(FieldValue::Int)
            .unwrap_or_else(|_| FieldValue::Str(raw.into())),
        "float" => raw
            .parse()
            .map(FieldValue::Float)
            .unwrap_or_else(|_| FieldValue::Str(raw.into())),
        "bool" => FieldValue::Bool(raw == "true"),
        "date" => FieldValue::Date(raw.into()),
        "list" => {
            if raw.is_empty() {
                FieldValue::List(vec![])
            } else {
                let rest = raw.strip_prefix('\u{1f}').unwrap_or(raw);
                FieldValue::List(rest.split('\u{1f}').map(str::to_string).collect())
            }
        }
        _ => FieldValue::Str(raw.into()),
    }
}

pub struct SqliteIndex {
    conn: Connection,
}

impl SqliteIndex {
    pub fn open(path: &std::path::Path) -> Result<Self, IndexError> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, IndexError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, IndexError> {
        conn.execute_batch(schema::DDL)?;
        if conn.execute_batch(schema::UNIQUE_KEY_INDEX).is_err() {
            // A cache built before the constraint existed may already hold
            // the duplicate keys the constraint exists to prevent. `tasks` is
            // disposable display data, refilled by the next reconcile — empty
            // it and retry rather than refusing to open the index at all.
            conn.execute_batch("DELETE FROM tasks;")?;
            conn.execute_batch(schema::UNIQUE_KEY_INDEX)?;
        }
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
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM entries WHERE project = ?1", params![project])?;
        for e in entries {
            Self::upsert_entry_row(&tx, project, e)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Merges `entries` into the project's index without touching any other
    /// uid's row — the delta counterpart to `apply`, which replaces the whole
    /// project. A delta describes a change, not the store: handing it to
    /// `apply` would wipe every uid this call doesn't mention, including
    /// ones from a delta batch too small to say anything about them (an
    /// empty batch would wipe the table outright).
    pub fn apply_upsert(&self, project: &str, entries: &[IndexEntry]) -> Result<(), IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        for e in entries {
            Self::upsert_entry_row(&tx, project, e)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn upsert_entry_row(
        conn: &Connection,
        project: &str,
        e: &IndexEntry,
    ) -> Result<(), IndexError> {
        conn.execute(
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

    /// Records a path whose key must be renumbered, so the write can wait out
    /// the §5 grace period. `since_ms` survives repeated calls with the same
    /// revision for the same reason `mark_pending` preserves its own: a
    /// reconcile runs on every command, and re-marking on each poll would
    /// mean the countdown never finishes.
    pub fn mark_pending_renumber(
        &self,
        project: &str,
        path: &str,
        rev: &Revision,
        since_ms: i64,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO pending_renumbers (project, path, revision, since_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(project, path) DO UPDATE SET
                 revision = excluded.revision,
                 since_ms = excluded.since_ms
             WHERE pending_renumbers.revision != excluded.revision",
            params![project, path, rev.as_str(), since_ms],
        )?;
        Ok(())
    }

    pub fn clear_pending_renumber(&self, project: &str, path: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "DELETE FROM pending_renumbers WHERE project = ?1 AND path = ?2",
            params![project, path],
        )?;
        Ok(())
    }

    /// path -> (revision when the collision was first seen, timestamp ms).
    pub fn pending_renumbers(
        &self,
        project: &str,
    ) -> Result<std::collections::BTreeMap<String, (Revision, i64)>, IndexError> {
        let mut st = self
            .conn
            .prepare("SELECT path, revision, since_ms FROM pending_renumbers WHERE project = ?1")?;
        let rows = st.query_map(params![project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = std::collections::BTreeMap::new();
        for row in rows {
            let (path, rev, since) = row?;
            out.insert(path, (Revision::from_raw(rev), since));
        }
        Ok(out)
    }

    /// Drops every grace-period row whose subject no longer exists — across
    /// all three tables, in one function, deliberately not three near-copies
    /// of one rule. Three near-copies is how "one half of a symmetric pair
    /// fixed, the other left" kept reproducing here; a reaper that covered
    /// `pending_deletions` alone left `pending` and `pending_renumbers` to
    /// grow without bound, and a stale row is worse than clutter — it reads
    /// as "already mid grace period", so the subject gets no grace period at
    /// all when it comes back.
    ///
    /// The two halves are keyed differently and cannot share a predicate.
    /// `pending_deletions` is keyed on uid and orphaned when `entries` no
    /// longer knows it. `pending` and `pending_renumbers` are keyed on path,
    /// and `pending` holds paths that by definition have no `entries` row yet
    /// (they are awaiting adoption), so only the scan's own live-path set can
    /// judge them. Pass `None` for `live_paths` when the scan was incomplete:
    /// absence is not evidence then, and reaping on it would wipe every
    /// adoption countdown in the project.
    pub fn reap_orphans(
        &self,
        project: &str,
        live_paths: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<usize, IndexError> {
        let mut reaped = self.conn.execute(
            "DELETE FROM pending_deletions
             WHERE project = ?1
               AND uid NOT IN (SELECT uid FROM entries WHERE project = ?1)",
            params![project],
        )?;
        let Some(live) = live_paths else {
            return Ok(reaped);
        };
        for table in ["pending", "pending_renumbers"] {
            let mut st = self
                .conn
                .prepare(&format!("SELECT path FROM {table} WHERE project = ?1"))?;
            let rows = st.query_map(params![project], |r| r.get::<_, String>(0))?;
            let mut orphans = Vec::new();
            for row in rows {
                let path = row?;
                if !live.contains(&path) {
                    orphans.push(path);
                }
            }
            for path in orphans {
                reaped += self.conn.execute(
                    &format!("DELETE FROM {table} WHERE project = ?1 AND path = ?2"),
                    params![project, path],
                )?;
            }
        }
        Ok(reaped)
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
             ON CONFLICT(project) DO UPDATE SET value = excluded.value
             WHERE high_water.value < excluded.value",
            params![project, value as i64],
        )?;
        Ok(())
    }

    /// One task's row-and-children insert, shared by `cache_tasks` (which
    /// deletes the whole project first) and `cache_upsert_tasks` (which
    /// deletes just this uid first). Clears this uid's `task_tags` and
    /// `task_fields` before re-inserting them — without that, a task that
    /// loses a tag or a field keeps it in the cache forever.
    fn insert_one(conn: &Connection, project: &str, t: &Task) -> Result<(), IndexError> {
        conn.execute(
            "DELETE FROM task_tags WHERE project = ?1 AND uid = ?2",
            params![project, t.uid.as_str()],
        )?;
        conn.execute(
            "DELETE FROM task_fields WHERE project = ?1 AND uid = ?2",
            params![project, t.uid.as_str()],
        )?;
        // A plain INSERT, not `INSERT OR REPLACE`: with `tasks_unique_key`
        // in place, `OR REPLACE` would silently delete whichever task
        // already held the key and put the duplicate in its place. Keys
        // are never reused, so a conflict here is a bug in the caller and
        // must surface as one.
        conn.execute(
            "INSERT INTO tasks
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
        for (i, tag) in t.tags.iter().enumerate() {
            conn.execute(
                "INSERT INTO task_tags (project, uid, ord, tag) VALUES (?1, ?2, ?3, ?4)",
                params![project, t.uid.as_str(), i as i64, tag],
            )?;
        }
        for (name, value) in &t.fields {
            let (kind, encoded) = encode_field(value);
            conn.execute(
                "INSERT INTO task_fields (project, uid, name, kind, value) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![project, t.uid.as_str(), name, kind, encoded],
            )?;
        }
        Ok(())
    }

    /// Replaces the cached display data for a project. Called by reconcile with
    /// the tasks the scan already parsed — no extra file reads.
    pub fn cache_tasks(&self, project: &str, tasks: &[Task]) -> Result<(), IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM tasks WHERE project = ?1", params![project])?;
        tx.execute("DELETE FROM task_tags WHERE project = ?1", params![project])?;
        tx.execute(
            "DELETE FROM task_fields WHERE project = ?1",
            params![project],
        )?;
        for t in tasks {
            Self::insert_one(&tx, project, t)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replaces just these tasks in the cached view, leaving every other
    /// cached task alone. `cache_tasks` deletes the whole project first,
    /// which a delta must not do — it describes a change, not the store.
    pub fn cache_upsert_tasks(&self, project: &str, tasks: &[Task]) -> Result<(), IndexError> {
        let tx = self.conn.unchecked_transaction()?;
        for t in tasks {
            // A plain INSERT would collide on the primary key for a task that
            // is merely being updated, which is the common case for a delta.
            tx.execute(
                "DELETE FROM tasks WHERE project = ?1 AND uid = ?2",
                params![project, t.uid.as_str()],
            )?;
            Self::insert_one(&tx, project, t)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn cursor(&self, project: &str) -> Result<Option<Cursor>, IndexError> {
        let mut st = self
            .conn
            .prepare("SELECT cursor FROM cursors WHERE project = ?1")?;
        let mut rows = st.query_map(params![project], |r| r.get::<_, Vec<u8>>(0))?;
        match rows.next() {
            Some(v) => Ok(Some(Cursor(v?))),
            None => Ok(None),
        }
    }

    pub fn set_cursor(&self, project: &str, cursor: &Cursor) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO cursors (project, cursor) VALUES (?1, ?2)
             ON CONFLICT(project) DO UPDATE SET cursor = excluded.cursor",
            params![project, cursor.0],
        )?;
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
            tags: Vec::new(),
            fields: BTreeMap::new(),
        })
    }

    fn hydrate(&self, project: &str, out: &mut [TaskSummary]) -> Result<(), IndexError> {
        if out.is_empty() {
            return Ok(());
        }
        let mut by_uid: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, s) in out.iter().enumerate() {
            by_uid.insert(s.uid.clone(), i);
        }
        let mut st = self
            .conn
            .prepare("SELECT uid, tag FROM task_tags WHERE project = ?1 ORDER BY uid, ord")?;
        let rows = st.query_map(params![project], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (uid, tag) = row?;
            if let Some(&i) = by_uid.get(uid.as_str()) {
                out[i].tags.push(tag);
            }
        }
        let mut st = self
            .conn
            .prepare("SELECT uid, name, kind, value FROM task_fields WHERE project = ?1")?;
        let rows = st.query_map(params![project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (uid, name, kind, value) = row?;
            if let Some(&i) = by_uid.get(uid.as_str()) {
                out[i].fields.insert(name, decode_field(&kind, &value));
            }
        }
        Ok(())
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
        self.hydrate(project, &mut out)?;
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
        let found = rows.next().transpose()?;
        let Some(s) = found else {
            return Ok(None);
        };
        let mut v = vec![s];
        self.hydrate(project, &mut v)?;
        Ok(Some(v.remove(0)))
    }

    /// Removes a single uid from all seven tables — the narrow, uid-scoped
    /// counterpart to `clear` (which is project-wide). For when the caller
    /// already knows, with certainty, that this exact task is gone (an
    /// explicit delete): there is nothing left to infer, so nothing else in
    /// the project is touched.
    ///
    /// The uid's path has to be read out of `entries` first, because
    /// `pending` and `pending_renumbers` are keyed on path. Leaving those two
    /// behind is not just clutter: the path is reused, and whatever file
    /// appears there next inherits a countdown that is already satisfied.
    pub fn forget(&self, project: &str, uid: &TaskUid) -> Result<(), IndexError> {
        let path: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM entries WHERE project = ?1 AND uid = ?2",
                params![project, uid.as_str()],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        for table in [
            "entries",
            "tasks",
            "pending_deletions",
            "task_tags",
            "task_fields",
        ] {
            self.conn.execute(
                &format!("DELETE FROM {table} WHERE project = ?1 AND uid = ?2"),
                params![project, uid.as_str()],
            )?;
        }
        if let Some(path) = path {
            for table in ["pending", "pending_renumbers"] {
                self.conn.execute(
                    &format!("DELETE FROM {table} WHERE project = ?1 AND path = ?2"),
                    params![project, path],
                )?;
            }
        }
        Ok(())
    }

    /// Wipes the derived view, including the tags and custom fields cached
    /// alongside `tasks` — they are as much a part of the derived view as the
    /// row they're keyed on. High-water marks deliberately survive.
    pub fn clear(&self, project: &str) -> Result<(), IndexError> {
        for t in [
            "entries",
            "pending",
            "pending_deletions",
            "pending_renumbers",
            "tasks",
            "task_tags",
            "task_fields",
            "cursors",
        ] {
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
    use cadet_core::{FieldValue, IndexEntry, Revision, TaskUid};
    use std::collections::BTreeMap;

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

    /// The delta counterpart to `apply_replaces_the_whole_project_view`:
    /// `apply_upsert` must do the opposite — leave every uid it wasn't
    /// handed alone, including when the batch it's handed is empty (the
    /// no-op-delta case, which `apply` would treat as "wipe everything").
    #[test]
    fn apply_upsert_touches_only_the_given_entries() {
        let i = idx();
        i.apply("p", &[entry("a.md"), entry("b.md")]).unwrap();
        i.apply_upsert("p", &[entry("c.md")]).unwrap();
        assert_eq!(
            i.view("p").unwrap().entries.len(),
            3,
            "a and b must survive an upsert that only mentions c"
        );

        i.apply_upsert("p", &[]).unwrap();
        assert_eq!(
            i.view("p").unwrap().entries.len(),
            3,
            "an empty upsert batch (a no-op delta) must not wipe the project"
        );
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

    /// Keys are never reused (spec §5), so two live tasks under one key is
    /// always a bug. Before the constraint it was a silent one: `find_by_key`
    /// returned whichever row SQLite yielded first and the other task was
    /// unreachable by `show`/`done`/`mv`/`rm` — the only interface there is.
    #[test]
    fn caching_two_tasks_under_one_key_is_rejected() {
        let i = idx();
        let err = i.cache_tasks(
            "p",
            &[
                task(4, "first", "todo", None),
                task(4, "second", "todo", None),
            ],
        );
        assert!(
            err.is_err(),
            "a duplicate key must be a loud error, not a silently unreachable task"
        );
    }

    #[test]
    fn the_same_key_in_two_projects_is_fine() {
        let i = idx();
        i.cache_tasks("a", &[task(1, "x", "todo", None)]).unwrap();
        i.cache_tasks("b", &[task(1, "y", "todo", None)]).unwrap();
        assert_eq!(i.list_tasks("a", true, &[]).unwrap().len(), 1);
        assert_eq!(i.list_tasks("b", true, &[]).unwrap().len(), 1);
    }

    #[test]
    fn pending_renumbers_round_trip_and_preserve_since_ms() {
        let i = idx();
        i.mark_pending_renumber("p", "a.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending_renumber("p", "a.md", &Revision::from_raw("r1"), 999_000)
            .unwrap();
        assert_eq!(i.pending_renumbers("p").unwrap()["a.md"].1, 100);
        i.mark_pending_renumber("p", "a.md", &Revision::from_raw("r2"), 999_000)
            .unwrap();
        assert_eq!(i.pending_renumbers("p").unwrap()["a.md"].1, 999_000);
        i.clear_pending_renumber("p", "a.md").unwrap();
        assert!(i.pending_renumbers("p").unwrap().is_empty());
    }

    /// A `pending_deletions` row for a uid `entries` no longer knows is worse
    /// than clutter: it reads as "already mid grace period", so if that uid
    /// ever reappears and vanishes again it is deleted with no grace period
    /// at all.
    #[test]
    fn orphaned_pending_deletions_are_reaped_and_live_ones_are_not() {
        let i = idx();
        let live = entry("live.md");
        let orphan = TaskUid::generate();
        i.apply("p", std::slice::from_ref(&live)).unwrap();
        i.mark_pending_deletion("p", &live.uid, 100).unwrap();
        i.mark_pending_deletion("p", &orphan, 100).unwrap();

        assert_eq!(i.reap_orphans("p", None).unwrap(), 1);
        let v = i.view("p").unwrap();
        assert!(v.pending_deletions.contains_key(&live.uid));
        assert!(!v.pending_deletions.contains_key(&orphan));
    }

    /// The reaper covers all three grace-period tables, not one. Three
    /// near-copies of this rule is how "one half of a symmetric pair fixed,
    /// the other left" kept reproducing in this codebase.
    #[test]
    fn reaping_covers_the_path_keyed_tables_too() {
        let i = idx();
        i.mark_pending("p", "live.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending("p", "gone.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending_renumber("p", "live.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending_renumber("p", "gone.md", &Revision::from_raw("r1"), 100)
            .unwrap();

        let live: std::collections::BTreeSet<String> =
            ["live.md".to_string()].into_iter().collect();
        i.reap_orphans("p", Some(&live)).unwrap();

        let v = i.view("p").unwrap();
        assert!(
            v.pending.contains_key("live.md"),
            "a live path must survive"
        );
        assert!(!v.pending.contains_key("gone.md"));
        let renumbers = i.pending_renumbers("p").unwrap();
        assert!(renumbers.contains_key("live.md"));
        assert!(!renumbers.contains_key("gone.md"));
    }

    /// `pending` holds paths that by definition have no `entries` row yet —
    /// they are awaiting adoption. An incomplete scan is not evidence a path
    /// is gone, and reaping on one would wipe every adoption countdown in
    /// the project, handing each note a fresh 60s wait on every command.
    #[test]
    fn an_incomplete_scan_reaps_nothing_path_keyed() {
        let i = idx();
        i.mark_pending("p", "awaiting.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending_renumber("p", "awaiting.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.reap_orphans("p", None).unwrap();
        assert!(i.view("p").unwrap().pending.contains_key("awaiting.md"));
        assert!(
            i.pending_renumbers("p")
                .unwrap()
                .contains_key("awaiting.md")
        );
    }

    /// `forget` has to clear all five tables. `pending` and
    /// `pending_renumbers` are keyed on path, and a path is reused: leaving
    /// a row behind means whatever file appears there next inherits a
    /// countdown that is already satisfied.
    #[test]
    fn forget_clears_the_path_keyed_tables_for_that_uid_too() {
        let i = idx();
        let gone = entry("gone.md");
        let other = entry("other.md");
        i.apply("p", &[gone.clone(), other.clone()]).unwrap();
        i.mark_pending("p", "gone.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending("p", "other.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending_renumber("p", "gone.md", &Revision::from_raw("r1"), 100)
            .unwrap();
        i.mark_pending_renumber("p", "other.md", &Revision::from_raw("r1"), 100)
            .unwrap();

        i.forget("p", &gone.uid).unwrap();

        let v = i.view("p").unwrap();
        assert!(!v.pending.contains_key("gone.md"));
        assert!(
            v.pending.contains_key("other.md"),
            "a different uid's path must be untouched"
        );
        let renumbers = i.pending_renumbers("p").unwrap();
        assert!(!renumbers.contains_key("gone.md"));
        assert!(renumbers.contains_key("other.md"));
    }

    #[test]
    fn reaping_one_project_leaves_another_alone() {
        let i = idx();
        let orphan = TaskUid::generate();
        i.mark_pending_deletion("a", &orphan, 100).unwrap();
        i.mark_pending_deletion("b", &orphan, 100).unwrap();
        i.reap_orphans("a", None).unwrap();
        assert!(i.view("a").unwrap().pending_deletions.is_empty());
        assert!(i.view("b").unwrap().pending_deletions.contains_key(&orphan));
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
    fn a_cursor_round_trips_and_is_absent_until_set() {
        let ix = SqliteIndex::open_in_memory().unwrap();
        assert_eq!(ix.cursor("p").unwrap(), None);
        ix.set_cursor("p", &Cursor(b"42".to_vec())).unwrap();
        assert_eq!(ix.cursor("p").unwrap(), Some(Cursor(b"42".to_vec())));
        ix.set_cursor("p", &Cursor(b"43".to_vec())).unwrap();
        assert_eq!(ix.cursor("p").unwrap(), Some(Cursor(b"43".to_vec())));
    }

    #[test]
    fn clearing_a_project_drops_its_cursor() {
        let ix = SqliteIndex::open_in_memory().unwrap();
        ix.set_cursor("p", &Cursor(b"42".to_vec())).unwrap();
        ix.clear("p").unwrap();
        assert_eq!(
            ix.cursor("p").unwrap(),
            None,
            "a cleared index must force a full snapshot, not resume from a stale cursor"
        );
    }

    #[test]
    fn cache_upsert_tasks_leaves_other_cached_tasks_alone() {
        let i = idx();
        i.cache_tasks(
            "p",
            &[task(1, "a", "todo", None), task(2, "b", "todo", None)],
        )
        .unwrap();
        let before = i.list_tasks("p", true, &[]).unwrap();
        // `cache_upsert_tasks` matches on uid, so it has to reuse the uid
        // already in the cache rather than minting a fresh one.
        let b_uid = TaskUid::parse(&before.iter().find(|t| t.title == "b").unwrap().uid).unwrap();
        let mut updated = task_with_uid(b_uid, 2, "b updated");
        updated.state = "doing".into();

        i.cache_upsert_tasks("p", &[updated]).unwrap();

        let after = i.list_tasks("p", true, &[]).unwrap();
        assert_eq!(after.len(), 2, "the untouched task must survive");
        assert!(after.iter().any(|t| t.title == "a"));
        let b_after = after.iter().find(|t| t.title == "b updated").unwrap();
        assert_eq!(b_after.state, "doing");
    }

    /// `insert_one` must clear a uid's tags before re-inserting, or a task
    /// that loses a tag keeps it in the cache forever.
    #[test]
    fn cache_upsert_tasks_drops_a_tag_the_task_no_longer_has() {
        let i = idx();
        let uid = TaskUid::generate();
        let mut first = task_with_uid(uid.clone(), 1, "t");
        first.tags = vec!["a".into(), "b".into()];
        i.cache_tasks("p", &[first]).unwrap();
        assert_eq!(i.list_tasks("p", true, &[]).unwrap()[0].tags.len(), 2);

        let mut second = task_with_uid(uid, 1, "t");
        second.tags = vec!["a".into()];
        i.cache_upsert_tasks("p", &[second]).unwrap();

        let after = i.list_tasks("p", true, &[]).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].tags, vec!["a".to_string()]);
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
    fn a_failed_cache_replacement_keeps_the_previous_cache() {
        let i = idx();
        i.cache_tasks("p", &[task(1, "original", "todo", None)])
            .unwrap();
        let replacements = [
            task(2, "first replacement", "todo", None),
            task(2, "duplicate key", "todo", None),
        ];

        assert!(i.cache_tasks("p", &replacements).is_err());

        let tasks = i.list_tasks("p", true, &[]).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "original");
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

    #[test]
    fn cached_tasks_round_trip_tags_and_custom_fields() {
        let ix = SqliteIndex::open_in_memory().unwrap();
        let mut t = Task {
            uid: TaskUid::generate(),
            key: TaskKey::new("T", 1),
            title: "with extras".into(),
            state: "todo".into(),
            created: "2026-08-02T00:00:00Z".parse().unwrap(),
            updated: "2026-08-02T00:00:00Z".parse().unwrap(),
            due: None,
            priority: Priority::Normal,
            tags: vec!["home".into(), "errand".into()],
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: BTreeMap::new(),
            body: String::new(),
        };
        t.fields.insert("estimate".into(), FieldValue::Int(3));
        t.fields
            .insert("area".into(), FieldValue::Str("kitchen".into()));

        ix.cache_tasks("p", &[t.clone()]).unwrap();

        let got = ix.list_tasks("p", true, &[]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].tags, vec!["home".to_string(), "errand".to_string()]);
        assert_eq!(got[0].fields.get("estimate"), Some(&FieldValue::Int(3)));
        assert_eq!(
            got[0].fields.get("area"),
            Some(&FieldValue::Str("kitchen".into()))
        );

        let one = ix.find_by_key("p", &TaskKey::new("T", 1)).unwrap().unwrap();
        assert_eq!(one.tags, vec!["home".to_string(), "errand".to_string()]);
        assert_eq!(one.fields.get("estimate"), Some(&FieldValue::Int(3)));
    }

    #[test]
    fn recaching_replaces_tags_and_fields_rather_than_accumulating() {
        let ix = SqliteIndex::open_in_memory().unwrap();
        let uid = TaskUid::generate();
        let mk = |tags: Vec<String>| Task {
            uid: uid.clone(),
            key: TaskKey::new("T", 1),
            title: "t".into(),
            state: "todo".into(),
            created: "2026-08-02T00:00:00Z".parse().unwrap(),
            updated: "2026-08-02T00:00:00Z".parse().unwrap(),
            due: None,
            priority: Priority::Normal,
            tags,
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: BTreeMap::new(),
            body: String::new(),
        };
        ix.cache_tasks("p", &[mk(vec!["a".into(), "b".into()])])
            .unwrap();
        ix.cache_tasks("p", &[mk(vec!["c".into()])]).unwrap();
        let got = ix.list_tasks("p", true, &[]).unwrap();
        assert_eq!(got[0].tags, vec!["c".to_string()]);
    }

    #[test]
    fn forget_clears_the_tag_and_field_tables_too() {
        let i = idx();
        let gone = entry("gone.md");
        i.apply("p", std::slice::from_ref(&gone)).unwrap();
        let mut t = task_with_uid(gone.uid.clone(), 1, "gone");
        t.tags = vec!["a".into(), "b".into()];
        t.fields.insert("x".into(), FieldValue::Int(1));
        i.cache_tasks("p", &[t]).unwrap();

        i.forget("p", &gone.uid).unwrap();

        let tag_count: i64 = i
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_tags WHERE project = 'p' AND uid = ?1",
                params![gone.uid.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        let field_count: i64 = i
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_fields WHERE project = 'p' AND uid = ?1",
                params![gone.uid.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tag_count, 0, "forget must remove the uid's tags too");
        assert_eq!(field_count, 0, "forget must remove the uid's fields too");
    }

    #[test]
    fn clear_wipes_the_tag_and_field_tables_too() {
        let i = idx();
        let mut t = task(1, "x", "todo", None);
        t.tags = vec!["a".into()];
        t.fields.insert("y".into(), FieldValue::Str("z".into()));
        i.cache_tasks("p", &[t]).unwrap();

        i.clear("p").unwrap();

        let tag_count: i64 = i
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_tags WHERE project = 'p'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let field_count: i64 = i
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_fields WHERE project = 'p'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tag_count, 0, "clear must wipe tags too");
        assert_eq!(field_count, 0, "clear must wipe fields too");
    }

    #[test]
    fn list_field_round_trips_a_single_empty_string_element() {
        let (kind, encoded) = encode_field(&FieldValue::List(vec!["".into()]));
        assert_eq!(
            decode_field(kind, &encoded),
            FieldValue::List(vec!["".into()]),
            "a list containing exactly one empty string must not decode as an empty list"
        );
    }

    #[test]
    fn list_field_round_trips_empty_elements_interspersed_with_non_empty_ones() {
        let items = vec!["a".to_string(), "".to_string(), "b".to_string()];
        let (kind, encoded) = encode_field(&FieldValue::List(items.clone()));
        assert_eq!(decode_field(kind, &encoded), FieldValue::List(items));
    }

    #[test]
    fn list_field_round_trips_the_empty_list() {
        let (kind, encoded) = encode_field(&FieldValue::List(vec![]));
        assert_eq!(decode_field(kind, &encoded), FieldValue::List(vec![]));
    }
}
