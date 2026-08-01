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
CREATE INDEX IF NOT EXISTS tasks_by_key ON tasks (project, key_num);
"#;
