pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    uid        TEXT PRIMARY KEY,
    key_prefix TEXT NOT NULL,
    key_num    INTEGER NOT NULL,
    title      TEXT NOT NULL,
    state      TEXT NOT NULL,
    created    TEXT NOT NULL,
    updated    TEXT NOT NULL,
    due        TEXT,
    priority   INTEGER NOT NULL,
    body       TEXT NOT NULL,
    renumbered_from TEXT,
    possible_duplicate_of TEXT,
    seq        INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS task_tags (
    uid  TEXT NOT NULL,
    ord  INTEGER NOT NULL,
    tag  TEXT NOT NULL,
    PRIMARY KEY (uid, ord)
);
CREATE TABLE IF NOT EXISTS task_fields (
    uid   TEXT NOT NULL,
    name  TEXT NOT NULL,
    kind  TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (uid, name)
);
-- Deletions have to be remembered, or a delta cannot report that something
-- went away. Pruned in Task 3, once there is a cursor to prune against.
CREATE TABLE IF NOT EXISTS deleted (
    uid TEXT PRIMARY KEY,
    seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
"#;
