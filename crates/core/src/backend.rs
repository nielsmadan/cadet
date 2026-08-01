use crate::canonical::Revision;
use crate::config::ProjectConfig;
use crate::identity::Snapshot;
use crate::model::{Task, TaskKey, TaskUid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(pub Vec<u8>);

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeSet {
    /// `snapshot` drives identity resolution; `tasks` (keyed by path) is the
    /// parsed content, which the caller caches so reads never touch the
    /// backend again (spec §3). A filesystem scan parses every file anyway,
    /// so carrying the tasks here is free.
    Snapshot {
        snapshot: Snapshot,
        tasks: std::collections::BTreeMap<String, Task>,
    },
    Delta {
        upserts: Vec<Task>,
        deletes: Vec<TaskUid>,
        cursor: Cursor,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("io: {0}")]
    Io(String),
    #[error("revision mismatch — the file changed underneath us")]
    RevisionMismatch,
    #[error("not found")]
    NotFound,
    #[error("malformed task file at {path}: {reason}")]
    Malformed { path: String, reason: String },
}

/// All parameters are by value: UniFFI cannot express references in foreign
/// trait methods, so a reference-taking trait can never be implemented in Swift.
pub trait Backend {
    fn load_project(&self) -> Result<ProjectConfig, BackendError>;
    fn save_project(&self, cfg: ProjectConfig) -> Result<(), BackendError>;

    fn get(&self, uid: TaskUid) -> Result<Option<Task>, BackendError>;
    fn put(&self, task: Task, expected: Option<Revision>) -> Result<Revision, BackendError>;
    fn delete(&self, uid: TaskUid, expected: Option<Revision>) -> Result<(), BackendError>;

    /// Stamps `uid`, `key` and timestamps into a task-shaped file that has none.
    /// Lives on the trait so all file I/O stays inside the backend — `app` must
    /// never touch the work tree directly.
    fn adopt(
        &self,
        path: String,
        uid: TaskUid,
        key: TaskKey,
        now: jiff::Timestamp,
    ) -> Result<Task, BackendError>;

    fn scan(&self, since: Option<Cursor>) -> Result<ChangeSet, BackendError>;
}
