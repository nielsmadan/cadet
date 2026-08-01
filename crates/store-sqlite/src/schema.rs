pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS entries (
    project   TEXT NOT NULL,
    uid       TEXT NOT NULL,
    path      TEXT NOT NULL,
    revision  TEXT NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    PRIMARY KEY (project, uid)
);
CREATE TABLE IF NOT EXISTS pending (
    project   TEXT NOT NULL,
    path      TEXT NOT NULL,
    revision  TEXT NOT NULL,
    since_ms  INTEGER NOT NULL,
    PRIMARY KEY (project, path)
);
CREATE TABLE IF NOT EXISTS pending_deletions (
    project   TEXT NOT NULL,
    uid       TEXT NOT NULL,
    since_ms  INTEGER NOT NULL,
    PRIMARY KEY (project, uid)
);
CREATE TABLE IF NOT EXISTS high_water (
    project   TEXT PRIMARY KEY,
    value     INTEGER NOT NULL
);
-- Cached display data. Spec §3: reads are served from the index, never from
-- the backend. Disposable like every other table here.
CREATE TABLE IF NOT EXISTS tasks (
    project   TEXT NOT NULL,
    uid       TEXT NOT NULL,
    key_num   INTEGER NOT NULL,
    key_prefix TEXT NOT NULL,
    title     TEXT NOT NULL,
    state     TEXT NOT NULL,
    due       TEXT,
    priority  INTEGER NOT NULL,
    PRIMARY KEY (project, uid)
);
-- Paths whose key is due to be renumbered but has not been written back yet.
-- Renumbering is one of the three situations in which reconcile writes to a
-- user file (spec §5), so it waits out the same grace period as adoption:
-- no file is written until it has been observed unchanged across two scans
-- at least 60s apart.
CREATE TABLE IF NOT EXISTS pending_renumbers (
    project   TEXT NOT NULL,
    path      TEXT NOT NULL,
    revision  TEXT NOT NULL,
    since_ms  INTEGER NOT NULL,
    PRIMARY KEY (project, path)
);
-- Tags and custom fields for the display cache. Separate tables rather than
-- delimited columns: a tag or a list item may contain any character, and a
-- delimiter that appears in the data is a silent corruption. Disposable like
-- `tasks` — rewritten wholesale by `cache_tasks`.
CREATE TABLE IF NOT EXISTS task_tags (
    project   TEXT NOT NULL,
    uid       TEXT NOT NULL,
    ord       INTEGER NOT NULL,
    tag       TEXT NOT NULL,
    PRIMARY KEY (project, uid, ord)
);
CREATE TABLE IF NOT EXISTS task_fields (
    project   TEXT NOT NULL,
    uid       TEXT NOT NULL,
    name      TEXT NOT NULL,
    kind      TEXT NOT NULL,
    value     TEXT NOT NULL,
    PRIMARY KEY (project, uid, name)
);
"#;

/// Created separately from `DDL` because it can legitimately fail on an index
/// built before the constraint existed — see `SqliteIndex::open`. Keys are
/// never reused (spec §5), so two live tasks sharing one key is always a bug;
/// this makes it a loud error at the point of insertion instead of a silent
/// duplicate that leaves one of the two tasks permanently unreachable by
/// `show`/`done`/`mv`/`rm`.
/// The name is deliberately NOT the old non-unique `tasks_by_key`: `IF NOT
/// EXISTS` matches on name alone, so reusing it would silently leave an index
/// built before this constraint without one.
pub const UNIQUE_KEY_INDEX: &str = "DROP INDEX IF EXISTS tasks_by_key;
CREATE UNIQUE INDEX IF NOT EXISTS tasks_unique_key ON tasks (project, key_prefix, key_num);";
