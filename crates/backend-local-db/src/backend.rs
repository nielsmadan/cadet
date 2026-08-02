use crate::schema;
use cadet_core::{
    Backend, BackendError, ChangeSet, Cursor, FieldType, FieldValue, Observed, Priority,
    ProjectConfig, Revision, Snapshot, Task, TaskKey, TaskUid, revision,
};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct LocalDbBackend {
    conn: Connection,
    config_path: PathBuf,
    config: OnceLock<ProjectConfig>,
}

/// Mirrors `crates/store-sqlite/src/lib.rs::encode_field`/`decode_field`
/// exactly. A bare `join` collapses `[]` and `[""]` to the same string —
/// that bug was found and fixed once already in this codebase.
fn encode_field(v: &FieldValue) -> (&'static str, String) {
    match v {
        FieldValue::Str(s) => ("str", s.clone()),
        FieldValue::Int(i) => ("int", i.to_string()),
        FieldValue::Float(f) => ("float", f.to_string()),
        FieldValue::Bool(b) => ("bool", b.to_string()),
        FieldValue::Date(d) => ("date", d.clone()),
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

fn priority_to_i64(p: Priority) -> i64 {
    match p {
        Priority::High => 0,
        Priority::Normal => 1,
        Priority::Low => 2,
    }
}

fn priority_from_i64(v: i64) -> Priority {
    match v {
        0 => Priority::High,
        2 => Priority::Low,
        _ => Priority::Normal,
    }
}

/// Coerces a field's stored `value` text to whatever `FieldType` is
/// *currently* declared for it. Mirrors `MarkdownBackend::coerce_field`,
/// which does the same for a raw frontmatter scalar: the value a field was
/// written with can predate a later edit to `project.toml`, and the
/// declaration governs what `task.fields` reports, not the `kind` tag
/// stored at write time.
fn coerce_stored_field(ty: &FieldType, raw: &str) -> FieldValue {
    match ty {
        FieldType::Int => raw
            .parse()
            .map(FieldValue::Int)
            .unwrap_or_else(|_| FieldValue::Str(raw.into())),
        FieldType::Float => raw
            .parse()
            .map(FieldValue::Float)
            .unwrap_or_else(|_| FieldValue::Str(raw.into())),
        FieldType::Bool => FieldValue::Bool(raw == "true"),
        FieldType::Date | FieldType::DateTime => FieldValue::Date(raw.into()),
        FieldType::Str | FieldType::Text | FieldType::Enum(_) => FieldValue::Str(raw.into()),
        FieldType::ListStr => decode_field("list", raw),
    }
}

impl LocalDbBackend {
    /// `db_path` is the SQLite file. The project config is read from the
    /// sibling `.toml` — `scratch.db` pairs with `scratch.toml`.
    pub fn open(db_path: &Path) -> Result<Self, BackendError> {
        let config_path = db_path.with_extension("toml");
        let conn = Connection::open(db_path).map_err(Self::io)?;
        Self::from_connection(conn, config_path)
    }

    pub fn open_in_memory(config_path: PathBuf) -> Result<Self, BackendError> {
        let conn = Connection::open_in_memory().map_err(Self::io)?;
        Self::from_connection(conn, config_path)
    }

    fn from_connection(conn: Connection, config_path: PathBuf) -> Result<Self, BackendError> {
        conn.execute_batch(schema::DDL).map_err(Self::io)?;
        conn.execute(
            "INSERT OR IGNORE INTO meta (k, v) VALUES ('change_seq', '0')",
            [],
        )
        .map_err(Self::io)?;
        // `-1` means "no cursor has ever been served" — every real seq is
        // >= 0, so no incoming cursor can be older than that. Once a delta
        // has been served for some seq, this becomes a watermark: a cursor
        // presented later that is older than the watermark may reference
        // tombstones this backend has already pruned, so `scan` must refuse
        // to serve it incrementally rather than silently under-report.
        conn.execute(
            "INSERT OR IGNORE INTO meta (k, v) VALUES ('prune_floor', '-1')",
            [],
        )
        .map_err(Self::io)?;
        Ok(Self {
            conn,
            config_path,
            config: OnceLock::new(),
        })
    }

    fn io<E: std::fmt::Display>(e: E) -> BackendError {
        BackendError::Io(e.to_string())
    }

    /// Cached project config: parsed from the sibling `.toml` at most once
    /// per process, since `load_fields` needs it — to coerce every stored
    /// field to its currently declared type — on every task read a scan
    /// touches. Mirrors `MarkdownBackend::config`.
    fn config(&self) -> Result<&ProjectConfig, BackendError> {
        if let Some(c) = self.config.get() {
            return Ok(c);
        }
        let cfg = self.load_project()?;
        Ok(self.config.get_or_init(|| cfg))
    }

    /// Takes `&Connection` rather than `&self` so it can run against either
    /// the plain connection (unused today, kept for symmetry) or a
    /// `Transaction` — `Transaction` derefs to `Connection`, so `&tx` coerces
    /// straight in and the bump becomes part of the same atomic write.
    fn bump_seq(conn: &Connection) -> Result<i64, BackendError> {
        conn.execute(
            "UPDATE meta SET v = CAST(v AS INTEGER) + 1 WHERE k = 'change_seq'",
            [],
        )
        .map_err(Self::io)?;
        conn.query_row(
            "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'change_seq'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map_err(Self::io)
    }

    fn load_tags(&self, uid: &TaskUid) -> Result<Vec<String>, BackendError> {
        let mut st = self
            .conn
            .prepare("SELECT tag FROM task_tags WHERE uid = ?1 ORDER BY ord")
            .map_err(Self::io)?;
        let rows = st
            .query_map(params![uid.as_str()], |r| r.get::<_, String>(0))
            .map_err(Self::io)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(Self::io)?);
        }
        Ok(out)
    }

    /// Only a field the project currently declares enters `task.fields`,
    /// coerced to its currently declared type — mirrors
    /// `MarkdownBackend::read_task`. A stored key the project no longer
    /// declares is not Cadet's to keep reporting: if it stayed, every
    /// future `validate_task` on this task would reject it as unknown, and
    /// changing or removing a field declaration is an ordinary edit to
    /// `project.toml`, not something that should be able to brick a
    /// project.
    fn load_fields(&self, uid: &TaskUid) -> Result<BTreeMap<String, FieldValue>, BackendError> {
        let cfg = self.config()?;
        let mut st = self
            .conn
            .prepare("SELECT name, value FROM task_fields WHERE uid = ?1")
            .map_err(Self::io)?;
        let rows = st
            .query_map(params![uid.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(Self::io)?;
        let mut raw: BTreeMap<String, String> = BTreeMap::new();
        for row in rows {
            let (name, value) = row.map_err(Self::io)?;
            raw.insert(name, value);
        }
        let mut out = BTreeMap::new();
        for def in &cfg.fields {
            if let Some(value) = raw.get(def.name.as_str()) {
                out.insert(def.name.clone(), coerce_stored_field(&def.ty, value));
            }
        }
        Ok(out)
    }

    /// Every task currently in the store, as a complete `Snapshot`. `scan`
    /// calls this for `None`, for an unparseable cursor, and for a cursor
    /// below the prune floor — RFC 6578's fallback to full synchronization
    /// when an incremental sync token can no longer be honoured.
    fn full_snapshot(&self) -> Result<ChangeSet, BackendError> {
        let uids = self.query_uids("SELECT uid FROM tasks", None, "tasks")?;

        let mut observed = Vec::new();
        let mut tasks = BTreeMap::new();
        for uid in uids {
            let task = self
                .get(uid.clone())?
                .ok_or_else(|| BackendError::Malformed {
                    path: uid.as_str().to_string(),
                    reason: "task vanished mid-scan".into(),
                })?;
            let rev = revision(&task);
            observed.push(Observed {
                uid: Some(uid.clone()),
                path: uid.as_str().to_string(),
                revision: rev,
            });
            tasks.insert(uid.as_str().to_string(), task);
        }

        Ok(ChangeSet::Snapshot {
            snapshot: Snapshot {
                complete: true,
                observed,
            },
            tasks,
        })
    }

    /// Shared by `full_snapshot`, `tasks_since` and `deletes_since`: read a
    /// column of stored uid strings and parse each into a `TaskUid`, naming
    /// `table` in the error a malformed row produces. `since` filters to
    /// `seq > since` when given, or reads the whole table when `None`.
    fn query_uids(
        &self,
        sql: &str,
        since: Option<i64>,
        table: &str,
    ) -> Result<Vec<TaskUid>, BackendError> {
        let mut st = self.conn.prepare(sql).map_err(Self::io)?;
        let bound: Vec<i64> = since.into_iter().collect();
        let rows = st
            .query_map(rusqlite::params_from_iter(&bound), |r| {
                r.get::<_, String>(0)
            })
            .map_err(Self::io)?;

        let mut out = Vec::new();
        for row in rows {
            let uid_str = row.map_err(Self::io)?;
            let uid = TaskUid::parse(&uid_str).ok_or_else(|| BackendError::Malformed {
                path: uid_str.clone(),
                reason: format!("invalid uid stored in {table} table"),
            })?;
            out.push(uid);
        }
        Ok(out)
    }

    fn current_seq(&self) -> Result<i64, BackendError> {
        self.conn
            .query_row(
                "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'change_seq'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(Self::io)
    }

    /// The newest `seq` any `scan(Some(_))` call has ever been served for —
    /// deliberately *not* `MIN(seq) FROM deleted`, because a full prune
    /// empties that table and would make a stale cursor look safe again.
    fn prune_floor(&self) -> Result<i64, BackendError> {
        self.conn
            .query_row(
                "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'prune_floor'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(Self::io)
    }

    /// Takes `&Connection` rather than `&self` for the same reason as
    /// `bump_seq`: a `Transaction` derefs to `Connection`, so this runs
    /// against either a plain connection or as part of a larger transaction.
    fn raise_prune_floor(conn: &Connection, seq: i64) -> Result<(), BackendError> {
        conn.execute(
            "UPDATE meta SET v = CAST(MAX(CAST(v AS INTEGER), ?1) AS TEXT)
             WHERE k = 'prune_floor'",
            params![seq],
        )
        .map_err(Self::io)?;
        Ok(())
    }

    fn tasks_since(&self, seq: i64) -> Result<Vec<Task>, BackendError> {
        let uids = self.query_uids("SELECT uid FROM tasks WHERE seq > ?1", Some(seq), "tasks")?;
        let mut out = Vec::new();
        for uid in uids {
            let task = self
                .get(uid.clone())?
                .ok_or_else(|| BackendError::Malformed {
                    path: uid.as_str().to_string(),
                    reason: "task vanished mid-scan".into(),
                })?;
            out.push(task);
        }
        Ok(out)
    }

    fn deletes_since(&self, seq: i64) -> Result<Vec<TaskUid>, BackendError> {
        self.query_uids(
            "SELECT uid FROM deleted WHERE seq > ?1",
            Some(seq),
            "deleted",
        )
    }
}

fn parse_cursor(c: &Cursor) -> Option<i64> {
    std::str::from_utf8(&c.0).ok()?.parse().ok()
}

impl Backend for LocalDbBackend {
    fn load_project(&self) -> Result<ProjectConfig, BackendError> {
        let src = std::fs::read_to_string(&self.config_path).map_err(Self::io)?;
        ProjectConfig::parse(&src).map_err(|e| BackendError::Malformed {
            path: self.config_path.display().to_string(),
            reason: e.to_string(),
        })
    }

    /// Not implemented: a from-scratch renderer would overwrite the sibling
    /// `.toml` wholesale, destroying whatever comments, unmodelled keys and
    /// section ordering a user wrote — the exact bug `render_project_toml`
    /// in `crates/cli/src/project.rs` was written to prevent (see its doc
    /// comment). Nothing calls this today; when something does, it should
    /// go through that renderer — parse the existing document, mutate only
    /// the keys that changed, write it back — not a fresh implementation
    /// here that repeats the bug in a second place.
    fn save_project(&self, _cfg: ProjectConfig) -> Result<(), BackendError> {
        Err(BackendError::Unsupported {
            capability: "writing the project config".into(),
        })
    }

    fn get(&self, uid: TaskUid) -> Result<Option<Task>, BackendError> {
        let row = self.conn.query_row(
            "SELECT key_prefix, key_num, title, state, created, updated, due, priority, body,
                    renumbered_from, possible_duplicate_of
             FROM tasks WHERE uid = ?1",
            params![uid.as_str()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            },
        );
        let (
            key_prefix,
            key_num,
            title,
            state,
            created,
            updated,
            due,
            priority,
            body,
            renumbered_from,
            possible_duplicate_of,
        ) = match row {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(Self::io(e)),
        };
        let created = created.parse().map_err(|e| BackendError::Malformed {
            path: uid.as_str().to_string(),
            reason: format!("invalid `created` timestamp: {e}"),
        })?;
        let updated = updated.parse().map_err(|e| BackendError::Malformed {
            path: uid.as_str().to_string(),
            reason: format!("invalid `updated` timestamp: {e}"),
        })?;
        let renumbered_from = renumbered_from
            .as_deref()
            .map(|s| {
                TaskKey::parse(s).ok_or_else(|| BackendError::Malformed {
                    path: uid.as_str().to_string(),
                    reason: format!("invalid `renumbered_from` key: {s}"),
                })
            })
            .transpose()?;
        let possible_duplicate_of = possible_duplicate_of
            .as_deref()
            .map(|s| {
                TaskUid::parse(s).ok_or_else(|| BackendError::Malformed {
                    path: uid.as_str().to_string(),
                    reason: format!("invalid `possible_duplicate_of` uid: {s}"),
                })
            })
            .transpose()?;
        let tags = self.load_tags(&uid)?;
        let fields = self.load_fields(&uid)?;
        Ok(Some(Task {
            uid,
            key: TaskKey::new(key_prefix, key_num as u32),
            title,
            state,
            created,
            updated,
            due,
            priority: priority_from_i64(priority),
            tags,
            renumbered_from,
            possible_duplicate_of,
            fields,
            body,
        }))
    }

    fn put(&self, task: Task, expected: Option<Revision>) -> Result<Revision, BackendError> {
        if let Some(want) = &expected {
            let current = self
                .get(task.uid.clone())?
                .ok_or(BackendError::RevisionMismatch)?;
            if &revision(&current) != want {
                return Err(BackendError::RevisionMismatch);
            }
        }

        // A currently declared field absent from `task.fields` is an
        // explicit removal and gets deleted below. Anything else —
        // including a stored row whose name the project no longer declares
        // — is left completely alone: mirrors `MarkdownBackend::put`, which
        // only ever emits a splice removal for a field in `cfg.fields`, so
        // an undeclared key already on disk is untouched. Filtering reads
        // to declared fields without this would mean the very next ordinary
        // edit — read (fields hidden), modify something else, write back —
        // permanently deletes every field the project doesn't currently
        // declare, rather than merely hiding it while undeclared.
        let cfg = self.config()?;
        let declared_and_absent: Vec<&str> = cfg
            .fields
            .iter()
            .map(|d| d.name.as_str())
            .filter(|name| !task.fields.contains_key(*name))
            .collect();

        // One transaction for the row, its tags, its fields and the
        // tombstone clear: a mid-write failure must leave the task exactly
        // as it was before `put` was called, never a blend of old and new.
        // `unchecked_transaction` is used because `Backend` methods take
        // `&self`, not `&mut self` — this backend is not shared across
        // threads, so the lack of a compile-time exclusivity check is safe
        // in practice.
        let tx = self.conn.unchecked_transaction().map_err(Self::io)?;
        let seq = Self::bump_seq(&tx)?;
        tx.execute(
            "INSERT OR REPLACE INTO tasks
             (uid, key_prefix, key_num, title, state, created, updated, due, priority, body,
              renumbered_from, possible_duplicate_of, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                task.uid.as_str(),
                task.key.prefix,
                task.key.number as i64,
                task.title,
                task.state,
                task.created.to_string(),
                task.updated.to_string(),
                task.due,
                priority_to_i64(task.priority),
                task.body,
                task.renumbered_from.as_ref().map(TaskKey::to_string),
                task.possible_duplicate_of.as_ref().map(TaskUid::as_str),
                seq,
            ],
        )
        .map_err(Self::io)?;

        tx.execute(
            "DELETE FROM task_tags WHERE uid = ?1",
            params![task.uid.as_str()],
        )
        .map_err(Self::io)?;
        for (i, tag) in task.tags.iter().enumerate() {
            tx.execute(
                "INSERT INTO task_tags (uid, ord, tag) VALUES (?1, ?2, ?3)",
                params![task.uid.as_str(), i as i64, tag],
            )
            .map_err(Self::io)?;
        }

        for name in declared_and_absent {
            tx.execute(
                "DELETE FROM task_fields WHERE uid = ?1 AND name = ?2",
                params![task.uid.as_str(), name],
            )
            .map_err(Self::io)?;
        }
        for (name, value) in &task.fields {
            let (kind, encoded) = encode_field(value);
            tx.execute(
                "INSERT OR REPLACE INTO task_fields (uid, name, kind, value) VALUES (?1, ?2, ?3, ?4)",
                params![task.uid.as_str(), name, kind, encoded],
            )
            .map_err(Self::io)?;
        }

        tx.execute(
            "DELETE FROM deleted WHERE uid = ?1",
            params![task.uid.as_str()],
        )
        .map_err(Self::io)?;

        tx.commit().map_err(Self::io)?;
        Ok(revision(&task))
    }

    fn delete(&self, uid: TaskUid, expected: Option<Revision>) -> Result<(), BackendError> {
        let current = self.get(uid.clone())?.ok_or(BackendError::NotFound)?;
        if let Some(want) = expected
            && revision(&current) != want
        {
            return Err(BackendError::RevisionMismatch);
        }

        // Same atomicity argument as `put`: the row, its tags, its fields
        // and the tombstone insert must land together or not at all.
        let tx = self.conn.unchecked_transaction().map_err(Self::io)?;
        let seq = Self::bump_seq(&tx)?;
        tx.execute("DELETE FROM tasks WHERE uid = ?1", params![uid.as_str()])
            .map_err(Self::io)?;
        tx.execute(
            "DELETE FROM task_tags WHERE uid = ?1",
            params![uid.as_str()],
        )
        .map_err(Self::io)?;
        tx.execute(
            "DELETE FROM task_fields WHERE uid = ?1",
            params![uid.as_str()],
        )
        .map_err(Self::io)?;
        tx.execute(
            "INSERT OR REPLACE INTO deleted (uid, seq) VALUES (?1, ?2)",
            params![uid.as_str(), seq],
        )
        .map_err(Self::io)?;
        tx.commit().map_err(Self::io)?;
        Ok(())
    }

    /// A database has no loose rows: every row cadet writes already carries
    /// a uid, so there is nothing here for `adopt` to claim.
    fn adopt(
        &self,
        _path: String,
        _uid: TaskUid,
        _key: TaskKey,
        _now: jiff::Timestamp,
    ) -> Result<Task, BackendError> {
        Err(BackendError::Unsupported {
            capability: "adopting an existing note".into(),
        })
    }

    fn scan(&self, since: Option<Cursor>) -> Result<ChangeSet, BackendError> {
        let Some(cursor) = since else {
            return self.full_snapshot();
        };
        let Some(seq) = parse_cursor(&cursor) else {
            return self.full_snapshot();
        };
        let head = self.current_seq()?;
        // A cursor greater than the current head is not a valid delta
        // request — malformed input, a cursor from another project's DB, a
        // corrupted `cursors` row — and must be rejected before it can touch
        // `prune_floor` at all: accepting it would raise the floor past
        // every legitimately-issued cursor still in flight, permanently
        // forcing every later `scan(Some(_))` onto the full-snapshot path.
        if seq > head {
            return self.full_snapshot();
        }
        // A cursor older than the watermark of some previously served delta
        // may reference tombstones this backend has already pruned in
        // response to that earlier call, so it cannot be served
        // incrementally. RFC 6578 calls for exactly this fallback when a
        // sync token is no longer valid.
        if seq < self.prune_floor()? {
            return self.full_snapshot();
        }

        let upserts = self.tasks_since(seq)?;
        let deletes = self.deletes_since(seq)?;

        // A consumer that has acknowledged `seq` will never ask for anything
        // at or below it again, so those tombstones can go. Single consumer
        // per database — see the spec. The prune and the floor raise must
        // land together: same atomicity argument as `put` and `delete` — a
        // mid-sequence failure must never leave tombstones pruned with the
        // floor unraised, which would silently hide the deletions that
        // pruning is supposed to have already handed to the caller.
        let tx = self.conn.unchecked_transaction().map_err(Self::io)?;
        tx.execute("DELETE FROM deleted WHERE seq <= ?1", params![seq])
            .map_err(Self::io)?;
        Self::raise_prune_floor(&tx, seq)?;
        tx.commit().map_err(Self::io)?;

        Ok(ChangeSet::Delta {
            upserts,
            deletes,
            cursor: Cursor(head.to_string().into_bytes()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `get` (and so `delete`, which always calls it) reads the declared
    /// fields via `config()` on every call now, so the config path must
    /// resolve to a real, valid `project.toml` even for tests that never
    /// call `load_project` directly — the returned `TempDir` must be kept
    /// alive for as long as the backend is used, or the directory (and the
    /// file `config()` reads) is gone.
    fn backend() -> (tempfile::TempDir, LocalDbBackend) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("t.toml"),
            "[project]\nid = \"t\"\nname = \"T\"\nprefix = \"T\"\n\n\
             [tasks]\nmatch = \"frontmatter\"\n\n\
             [workflow]\nstates = [\"todo\", \"doing\", \"done\"]\n\
             initial = \"todo\"\nterminal = [\"done\"]\n\n\
             [[fields]]\nname = \"a\"\ntype = \"int\"\n",
        )
        .unwrap();
        let b = LocalDbBackend::open_in_memory(dir.path().join("t.toml")).unwrap();
        (dir, b)
    }

    fn task(n: u32) -> Task {
        Task {
            uid: TaskUid::generate(),
            key: TaskKey::new("T", n),
            title: "half written".into(),
            state: "todo".into(),
            created: jiff::Timestamp::UNIX_EPOCH,
            updated: jiff::Timestamp::UNIX_EPOCH,
            due: None,
            priority: Priority::Normal,
            tags: vec!["ok".into()],
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: BTreeMap::new(),
            body: String::new(),
        }
    }

    /// `put` writes a row, its tags and its fields as several statements.
    /// A trigger fires partway through the field inserts (after the tags —
    /// which are also several statements — have already landed), simulating
    /// any real mid-write failure (disk full, a constraint violation on a
    /// concurrent writer, `SQLITE_BUSY`). Without a transaction wrapping the
    /// whole write, the row and its tags survive even though `put` reports
    /// `Err` — the caller is told nothing landed when part of it did.
    #[test]
    fn a_failed_put_does_not_leave_a_half_written_task_behind() {
        let (_dir, b) = backend();
        b.conn
            .execute_batch(
                "CREATE TRIGGER inject_failure BEFORE INSERT ON task_fields
                 WHEN NEW.name = 'boom'
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();

        let mut t = task(1);
        t.fields.insert("a".into(), FieldValue::Int(1));
        t.fields.insert("boom".into(), FieldValue::Int(2));

        let err = b.put(t.clone(), None).unwrap_err();
        assert!(matches!(err, BackendError::Io(_)), "{err:?}");

        assert!(
            b.get(t.uid).unwrap().is_none(),
            "a failed put must not leave a half-written task behind — \
             the row and its tags were written before the field insert \
             that failed, and without a transaction they are never undone"
        );
    }

    /// Same argument as `put`, for `delete`: the row delete, the tag
    /// delete, the field delete and the final tombstone insert must land
    /// together. The trigger fires on the tombstone insert — the last of
    /// the four statements — so an un-transactional `delete` would have
    /// already removed the row, its tag and its field for good by the time
    /// it fails: a caller told "this delete failed" would have no reason to
    /// suspect the task is now permanently gone with no tombstone to show
    /// for it.
    #[test]
    fn a_failed_delete_leaves_the_task_fully_intact() {
        let (_dir, b) = backend();
        let mut t = task(1);
        t.fields.insert("a".into(), FieldValue::Int(1));
        b.put(t.clone(), None).unwrap();

        b.conn
            .execute_batch(
                "CREATE TRIGGER inject_failure BEFORE INSERT ON deleted
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();

        let err = b.delete(t.uid.clone(), None).unwrap_err();
        assert!(matches!(err, BackendError::Io(_)), "{err:?}");

        assert_eq!(
            b.get(t.uid.clone()).unwrap(),
            Some(t),
            "a failed delete must leave the task exactly as it was before \
             `delete` was called"
        );
    }

    /// `scan`'s prune-and-advance is two statements: the tombstone `DELETE`
    /// and the `prune_floor` `UPDATE`. Same argument as `put` and `delete`:
    /// without a transaction wrapping both, a mid-sequence failure can prune
    /// tombstones the floor was never raised to acknowledge — which reopens,
    /// by partial failure, exactly the "the floor looks safe again after a
    /// prune" bug the floor exists to close by construction. The trigger
    /// fires on the `prune_floor` update, which runs after the tombstone
    /// delete has already landed.
    #[test]
    fn a_failed_scan_prune_does_not_desync_the_floor_from_the_tombstones() {
        let (_dir, b) = backend();
        let t1 = task(1);
        b.put(t1.clone(), None).unwrap(); // seq 1
        b.delete(t1.uid.clone(), None).unwrap(); // seq 2, tombstone(seq=2)
        let t2 = task(2);
        b.put(t2.clone(), None).unwrap(); // seq 3
        b.delete(t2.uid.clone(), None).unwrap(); // seq 4, tombstone(seq=4)

        b.conn
            .execute_batch(
                "CREATE TRIGGER inject_failure BEFORE UPDATE ON meta
                 WHEN NEW.k = 'prune_floor'
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();

        let err = b.scan(Some(Cursor(b"2".to_vec()))).unwrap_err();
        assert!(matches!(err, BackendError::Io(_)), "{err:?}");

        let remaining: i64 = b
            .conn
            .query_row("SELECT COUNT(*) FROM deleted", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            remaining, 2,
            "a failed scan must not prune any tombstones — the delete and \
             the floor raise must land together or not at all"
        );

        let floor: i64 = b
            .conn
            .query_row(
                "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'prune_floor'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            floor, -1,
            "a failed scan must not raise the prune floor either — a \
             pruned-but-unacknowledged tombstone is invisible to a later \
             scan at the same cursor"
        );
    }
}
