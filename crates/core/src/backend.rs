use crate::canonical::Revision;
use crate::config::ProjectConfig;
use crate::identity::Snapshot;
use crate::model::{Task, TaskKey, TaskUid};
use std::path::Path;

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
        /// `Some` when this backend can resume incrementally from this exact
        /// point; `None` when it cannot (`backend-markdown`, which never
        /// serves deltas at all). Without this a reconcile that took the
        /// snapshot path has nothing to store, and the next scan is a full
        /// scan again — forever, even for a backend that could have resumed
        /// cheaply. A snapshot is a point in time; one that cannot say which
        /// point is the bug, not a feature of "just a snapshot".
        cursor: Option<Cursor>,
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
    #[error("malformed project config at {path}: {reason}")]
    MalformedProjectConfig { path: String, reason: String },
    #[error("this backend does not support {capability}")]
    Unsupported { capability: String },
}

impl BackendError {
    pub fn malformed_project_config(path: &Path, reason: impl std::fmt::Display) -> Self {
        match std::path::absolute(path) {
            Ok(abs) => Self::MalformedProjectConfig {
                path: abs.display().to_string(),
                reason: reason.to_string(),
            },
            Err(e) => Self::Io(format!(
                "could not resolve the project config path `{}`: {e}",
                path.display()
            )),
        }
    }
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

    /// Where this backend stores `uid`, if it stores tasks as files.
    ///
    /// Exists solely so the local git safety net can stage exactly the files
    /// cadet wrote. Without it the net runs `add --all` over the whole work
    /// tree, sweeping in every unrelated note the user happens to have edited
    /// — and `undo` then reverts those too. Path-shaped, like `adopt`, and
    /// like `adopt` it is meaningless to a backend with no filesystem: the
    /// default returns `None`, and such a backend gets no safety net at all.
    fn location_of(&self, _uid: TaskUid) -> Result<Option<String>, BackendError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::BackendError;
    use std::path::Path;

    #[test]
    fn display_distinguishes_task_file_from_project_config() {
        let task_file = BackendError::Malformed {
            path: "task.md".to_string(),
            reason: "missing or invalid uid".to_string(),
        };
        let project_config = BackendError::MalformedProjectConfig {
            path: "/tmp/project.toml".to_string(),
            reason: "config parse error".to_string(),
        };

        assert_eq!(
            task_file.to_string(),
            "malformed task file at task.md: missing or invalid uid"
        );
        assert_eq!(
            project_config.to_string(),
            "malformed project config at /tmp/project.toml: config parse error"
        );
    }

    #[test]
    fn malformed_project_config_makes_relative_path_absolute() {
        let err = BackendError::malformed_project_config("project.toml".as_ref(), "invalid toml");

        let BackendError::MalformedProjectConfig { path, reason } = err else {
            panic!("expected malformed project config error");
        };

        let path = Path::new(&path);
        assert!(path.is_absolute());
        assert!(path.ends_with("project.toml"));
        assert_eq!(reason, "invalid toml");
    }

    #[test]
    fn malformed_project_config_reports_absolute_resolution_failure_as_io() {
        let err = BackendError::malformed_project_config(Path::new(""), "invalid toml");

        let BackendError::Io(message) = err else {
            panic!("expected io error for an unresolvable config path");
        };

        assert!(message.starts_with("could not resolve the project config path ``: "));
    }
}
