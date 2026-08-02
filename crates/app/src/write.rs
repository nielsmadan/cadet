use crate::reconcile::{App, AppError};
use cadet_core::*;
use cadet_store_sqlite::TaskSummary;
use std::collections::{BTreeMap, BTreeSet};

/// Everything `add_with` needs to create a task. `title` is the only
/// required field; everything else falls back to the same defaults `add`
/// always used (`state` to the project workflow's `initial`, `priority` to
/// `Priority::default()`, `due`/`tags`/`fields` to empty).
#[derive(Debug, Clone, Default)]
pub struct TaskDraft {
    pub title: String,
    pub state: Option<String>,
    pub due: Option<String>,
    pub priority: Option<Priority>,
    pub tags: Vec<String>,
    pub fields: BTreeMap<String, FieldValue>,
}

/// Everything `update` can change on an existing task. Every field is a
/// no-op when left at its default: `None` means "leave alone" everywhere,
/// including `due`, where the outer `Option` distinguishes that from `Some(None)`
/// ("clear it") and a field entry's `Option<FieldValue>` distinguishes "leave
/// alone" (absent from the map) from "remove it" (`None`).
#[derive(Debug, Clone, Default)]
pub struct TaskChanges {
    pub title: Option<String>,
    pub state: Option<String>,
    pub due: Option<Option<String>>,
    pub priority: Option<Priority>,
    pub tags: Option<Vec<String>>,
    pub fields: BTreeMap<String, Option<FieldValue>>,
}

/// `due` is read straight off frontmatter with no validation and compared as
/// a plain string by `TaskFilter`, which is calendar-correct only when the
/// format is fixed-width — a task written with `due: 2026-8-10` sorts and
/// filters wrong forever after. `is_date_like` is the same gate
/// `parse_field_value` applies to every declared `Date`/`DateTime` field;
/// `add_with` and `update` are the only two places Cadet ever writes `due`,
/// so this is the one place that has to call it.
fn validate_due(due: &Option<String>) -> Result<(), CoreError> {
    if let Some(d) = due
        && !is_date_like(d)
    {
        return Err(CoreError::FieldType {
            field: "due".to_string(),
            expected: "a date such as 2026-08-10".to_string(),
        });
    }
    Ok(())
}

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
    /// duplicate or try to delete something already gone. The change stays on
    /// disk either way; only the safety-net record of it is missing.
    ///
    /// `paths` names exactly the files cadet wrote. A backend with no
    /// filesystem reports none, and then there is nothing to commit.
    fn commit_or_warn(&self, message: &str, paths: &[String]) {
        if let Err(e) = self.git.commit(message, paths) {
            self.warn(format!(
                "change saved, but the safety net could not record it: {e}"
            ));
        }
    }

    /// The file the backend stores this task in, as a one-element pathspec for
    /// the safety net. Empty when the backend is not filesystem backed, or
    /// when the location can no longer be resolved — a safety-net gap is
    /// never worth failing a write that already succeeded.
    fn location(&self, uid: &TaskUid) -> Vec<String> {
        match self.backend.location_of(uid.clone()) {
            Ok(Some(p)) => vec![p],
            _ => vec![],
        }
    }

    pub fn add(&self, title: &str) -> Result<Task, AppError> {
        self.add_with(TaskDraft {
            title: title.to_string(),
            ..Default::default()
        })
    }

    pub fn add_with(&self, draft: TaskDraft) -> Result<Task, AppError> {
        let cfg = self.backend.load_project()?;
        let next = self.index.high_water(&self.project)? + 1;
        let now = Self::now();
        let task = Task {
            uid: TaskUid::generate(),
            key: TaskKey::new(cfg.prefix.clone(), next),
            title: draft.title,
            // Creation has no "from" state, so the transition graph does not
            // apply — `validate_task` below still requires the state to be
            // declared, and minting a task straight into `done` to log
            // something already finished is legitimate, not a bug.
            state: draft.state.unwrap_or_else(|| cfg.workflow.initial.clone()),
            created: now,
            updated: now,
            due: draft.due,
            priority: draft.priority.unwrap_or_default(),
            tags: draft.tags,
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: draft.fields,
            body: String::new(),
        };
        validate_due(&task.due)?;
        validate_task(&task, &cfg)?;
        self.backend.put(task.clone(), None)?;
        self.index.bump_high_water(&self.project, next)?;
        self.refresh_cache_or_warn();
        let paths = self.location(&task.uid);
        self.commit_or_warn(&format!("add {}: {}", task.key, task.title), &paths);
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
        self.update(
            key,
            TaskChanges {
                state: Some(state.to_string()),
                ..Default::default()
            },
        )
    }

    pub fn update(&self, key: &TaskKey, changes: TaskChanges) -> Result<Task, AppError> {
        // A change set carrying nothing is a caller no-op, not a write: it
        // must not rewrite the file, bump `updated`, or leave a git commit
        // behind. `TaskChanges::default()` (e.g. before any field is set by
        // a CLI flag) is exactly this case.
        if changes.title.is_none()
            && changes.state.is_none()
            && changes.due.is_none()
            && changes.priority.is_none()
            && changes.tags.is_none()
            && changes.fields.is_empty()
        {
            return self.get_by_key(key);
        }

        let cfg = self.backend.load_project()?;
        let mut task = self.get_by_key(key)?;
        if let Some(state) = &changes.state {
            check_transition(&cfg.workflow, &task.state, state)?;
        }
        // Validate only what the caller actually supplied, never the merged
        // result: `task.due` may already carry a bad date hand-written
        // straight into the file (adoption never validates `due`), and that
        // is a pre-existing condition this call did not create and has no
        // obligation to fix. Rejecting the merge would make an unrelated
        // change — even a bare state transition — permanently impossible.
        // The amendment's own words: "you cannot fix a file that already
        // contains a bad date, but you can stop Cadet writing one."
        if let Some(due) = &changes.due {
            validate_due(due)?;
        }
        // A removal request for a name the project never declared is not a
        // no-op: the backend never put an undeclared key into `task.fields`
        // in the first place (that key is preserved on disk, untouched, at
        // the backend layer — it was never Cadet's to delete), so silently
        // succeeding here reports success for a request that changed
        // nothing.
        for (name, value) in &changes.fields {
            if value.is_none() && !cfg.fields.iter().any(|d| &d.name == name) {
                return Err(CoreError::UnknownField(name.clone()).into());
            }
        }
        let expected = revision(&task);
        let state_changed = changes.state.is_some();

        if let Some(title) = changes.title {
            task.title = title;
        }
        if let Some(state) = changes.state {
            task.state = state;
        }
        if let Some(due) = changes.due {
            task.due = due;
        }
        if let Some(priority) = changes.priority {
            task.priority = priority;
        }
        if let Some(tags) = changes.tags {
            task.tags = tags;
        }
        for (name, value) in changes.fields {
            match value {
                Some(v) => {
                    task.fields.insert(name, v);
                }
                None => {
                    task.fields.remove(&name);
                }
            }
        }
        task.updated = Self::now();

        validate_task(&task, &cfg)?;
        self.backend.put(task.clone(), Some(expected))?;
        self.refresh_cache_or_warn();
        let message = if state_changed {
            format!("{} -> {}", task.key, task.state)
        } else {
            format!("update {}", task.key)
        };
        let paths = self.location(&task.uid);
        self.commit_or_warn(&message, &paths);
        Ok(task)
    }

    pub fn delete(&self, key: &TaskKey) -> Result<(), AppError> {
        let task = self.get_by_key(key)?;
        let expected = revision(&task);
        // Resolved before the delete: afterwards there is no file to locate,
        // and the safety net would never record the removal.
        let paths = self.location(&task.uid);
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
        self.commit_or_warn(&format!("remove {key}"), &paths);
        Ok(())
    }

    /// The list view. One SQL query — never touches the backend (spec §3).
    /// `all` includes terminal states. Sorting happens in SQL.
    pub fn list(&self, all: bool) -> Result<Vec<TaskSummary>, AppError> {
        self.list_filtered(all, &TaskFilter::default())
    }

    /// Same as `list`, narrowed by `filter`. Filtering happens in Rust, not
    /// SQL: the tag and field predicates would each need a correlated
    /// subquery, and the list is already fully materialised for display.
    pub fn list_filtered(
        &self,
        all: bool,
        filter: &TaskFilter,
    ) -> Result<Vec<TaskSummary>, AppError> {
        let cfg = self.backend.load_project()?;
        // Naming a state explicitly is itself a request to see it, so the
        // terminal-state exclusion defers to that: `--state done` must not
        // be silently emptied by the same hiding that `--all` exists to
        // override. Without a state filter, the exclusion still applies.
        let include_terminal = all || !filter.states.is_empty();
        let rows =
            self.index
                .list_tasks(&self.project, include_terminal, &cfg.workflow.terminal)?;
        if filter.is_empty() {
            return Ok(rows);
        }
        Ok(rows
            .into_iter()
            .filter(|s| {
                filter.matches(&FilterTarget {
                    state: &s.state,
                    due: s.due.as_deref(),
                    priority: s.priority,
                    tags: &s.tags,
                    fields: &s.fields,
                })
            })
            .collect())
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
