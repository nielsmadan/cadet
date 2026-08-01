use crate::reconcile::{App, AppError};
use cadet_core::*;
use cadet_store_sqlite::TaskSummary;
use std::collections::{BTreeMap, BTreeSet};

impl App {
    fn now() -> jiff::Timestamp {
        jiff::Timestamp::now()
    }

    /// Re-reads the backend once and refreshes the cached display data, so an
    /// index-backed `list()` is correct immediately after a mutation.
    /// One pass, not one pass per task.
    ///
    /// It has no identity-resolution pass of its own, so it must not blindly
    /// trust the scan as the complete picture. Both places that used to make
    /// it wrong now run through the same two functions `reconcile_with` uses:
    /// `resolve_duplicates` (one path per uid, one task per key) and
    /// `assemble_cache` (a task mid pending-deletion grace period is
    /// legitimately absent from disk right now, and a bare scan-and-replace
    /// would drop it from `list()` the moment *any* write ran).
    fn refresh_cache(&self) -> Result<(), AppError> {
        if let ChangeSet::Snapshot {
            snapshot,
            mut tasks,
        } = self.backend.scan(None)?
        {
            let prefix = self.backend.load_project()?.prefix;
            // Resolutions are discarded here on purpose: this path never
            // writes to a user file. It only has to leave the cache
            // consistent with what reconcile will settle on.
            self.resolve_duplicates(&mut tasks, &prefix)?;
            let observed: BTreeSet<&str> = snapshot
                .observed
                .iter()
                .filter_map(|o| o.uid.as_ref().map(TaskUid::as_str))
                .collect();
            let previously_cached = self.index.list_tasks(&self.project, true, &[])?;
            let cached = self.assemble_cache(
                snapshot.complete,
                tasks,
                &observed,
                previously_cached.iter(),
            )?;
            self.index.cache_tasks(&self.project, &cached)?;
        }
        Ok(())
    }

    /// The backend write and the index update that precede every call to
    /// this are already durable; the cache is derived display data on top of
    /// them. A failure here must never make an already-successful write look
    /// like it failed — a user who retries duplicates the work. Exactly the
    /// reasoning `commit_or_warn` applies to git, and exactly what `add`,
    /// `done` and `rm` lacked when they exited 1 after their write had
    /// already landed.
    fn refresh_cache_or_warn(&self) {
        if let Err(e) = self.refresh_cache() {
            self.warn(format!(
                "change saved, but the task list could not be refreshed: {e}"
            ));
        }
    }

    /// The backend write and index update that precede this call are already
    /// durable — git is a local safety net on top of them, not the source of
    /// truth. A commit failure here (a broken repo, a permissions glitch)
    /// must not make an already-successful write look like it failed: that
    /// would make a caller (or a user hitting Ctrl-C and retrying) create a
    /// duplicate or try to delete something already gone. `GitNet::commit`
    /// runs `git add --all`, so the next successful commit sweeps up the
    /// orphaned change on its own — nothing is lost, only left uncommitted.
    fn commit_or_warn(&self, message: &str) {
        if let Err(e) = self.git.commit(message) {
            self.warn(format!(
                "change saved, but the safety net could not record it: {e}"
            ));
        }
    }

    pub fn add(&self, title: &str) -> Result<Task, AppError> {
        let cfg = self.backend.load_project()?;
        let next = self.index.high_water(&self.project)? + 1;
        let now = Self::now();
        let task = Task {
            uid: TaskUid::generate(),
            key: TaskKey::new(cfg.prefix.clone(), next),
            title: title.to_string(),
            state: cfg.workflow.initial.clone(),
            created: now,
            updated: now,
            due: None,
            priority: Priority::Normal,
            tags: vec![],
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: BTreeMap::new(),
            body: String::new(),
        };
        validate_task(&task, &cfg)?;
        self.backend.put(task.clone(), None)?;
        self.index.bump_high_water(&self.project, next)?;
        self.refresh_cache_or_warn();
        self.commit_or_warn(&format!("add {}: {}", task.key, task.title));
        Ok(task)
    }

    /// Resolves a key via the index (one query), then reads the full task from
    /// the backend — the body and custom fields are not cached.
    pub fn get_by_key(&self, key: &TaskKey) -> Result<Task, AppError> {
        let summary = self
            .index
            .find_by_key(&self.project, key)?
            .ok_or_else(|| AppError::NoSuchKey(key.to_string()))?;
        let uid =
            TaskUid::parse(&summary.uid).ok_or_else(|| AppError::NoSuchKey(key.to_string()))?;
        self.backend
            .get(uid)?
            .ok_or_else(|| AppError::NoSuchKey(key.to_string()))
    }

    pub fn set_state(&self, key: &TaskKey, state: &str) -> Result<Task, AppError> {
        let cfg = self.backend.load_project()?;
        let mut task = self.get_by_key(key)?;
        check_transition(&cfg.workflow, &task.state, state)?;
        let expected = revision(&task);
        task.state = state.to_string();
        task.updated = Self::now();
        validate_task(&task, &cfg)?;
        self.backend.put(task.clone(), Some(expected))?;
        self.refresh_cache_or_warn();
        self.commit_or_warn(&format!("{} -> {}", task.key, state));
        Ok(task)
    }

    pub fn delete(&self, key: &TaskKey) -> Result<(), AppError> {
        let task = self.get_by_key(key)?;
        let expected = revision(&task);
        self.backend.delete(task.uid.clone(), Some(expected))?;
        // An explicit `cadet rm` is certain, not inferred — the file's
        // absence is never going to be resolved by a sync tool catching up.
        // Retire the identity outright instead of leaving it in `entries`
        // for the next `reconcile` to rediscover, which would look exactly
        // like an ordinary external deletion: routed through the deletion
        // grace period (and counted in its mass-deletion `ScanRejected`
        // guard) for no reason, and misreported by `cadet doctor` as still
        // pending when the user already confirmed it.
        self.index.forget(&self.project, &task.uid)?;
        self.refresh_cache_or_warn();
        self.commit_or_warn(&format!("remove {key}"));
        Ok(())
    }

    /// The list view. One SQL query — never touches the backend (spec §3).
    /// `all` includes terminal states. Sorting happens in SQL.
    pub fn list(&self, all: bool) -> Result<Vec<TaskSummary>, AppError> {
        let cfg = self.backend.load_project()?;
        Ok(self
            .index
            .list_tasks(&self.project, all, &cfg.workflow.terminal)?)
    }

    /// Unlike the write paths above, a failed undo must fail loudly: undo is
    /// not itself a write with a durable backend result to fall back on, so
    /// there is no already-successful outcome to protect by swallowing the
    /// error into a warning.
    pub fn undo(&self) -> Result<(), AppError> {
        self.git.undo()?;
        Ok(())
    }
}
