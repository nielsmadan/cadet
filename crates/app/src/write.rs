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
    pub body: String,
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
    pub body: Option<String>,
    pub fields: BTreeMap<String, Option<FieldValue>>,
}

/// `due` is read straight off frontmatter with no validation and compared as
/// a plain string by `TaskFilter`, which is calendar-correct only when the
/// format is fixed-width — a task written with `due: 2026-8-10` sorts and
/// filters wrong forever after. `add_with` and `update` are the only two places
/// Cadet ever writes `due`, so this is the one place that has to parse it as a
/// real calendar date.
fn validate_due(due: &Option<String>) -> Result<(), CoreError> {
    if let Some(d) = due
        && !matches!(canonical_due_date(d), Ok(canonical) if canonical == *d)
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
            // `scan(None)` always yields a `Snapshot` (never a `Delta`) by
            // contract, so there is no cursor bookkeeping to do here — this
            // pass exists only to keep the display cache honest after a
            // write, independent of `reconcile`'s own cursor tracking.
            cursor: _,
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
        // No work tree, no safety net. Not a warning: a backend that stores
        // tasks in a database has nothing for git to hold, and saying so on
        // every write would be noise about a permanent property.
        let Some(git) = &self.git else { return };
        if let Err(e) = git.commit(message, paths) {
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
            body: draft.body,
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
            && changes.body.is_none()
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
        if let Some(body) = changes.body {
            task.body = body;
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

    /// Move many tasks into one state as a single administrative operation:
    /// one git commit and one cache refresh for the whole batch, not one per
    /// task. `cadet project state rm --move-to`, `state rename` and
    /// `doctor repair-state` are all this function.
    ///
    /// Only the destination is validated. The transition graph is skipped on
    /// purpose — the source state is typically one being removed, or one no
    /// longer declared at all, and both callers exist precisely to rescue
    /// tasks the ordinary write path refuses to touch. For the same reason
    /// the merged task is not revalidated: a task stranded by a hand edit may
    /// carry other pre-existing problems, and none of them are this call's to
    /// fix or to block on.
    pub fn move_tasks(&self, keys: &[TaskKey], to_state: &str) -> Result<usize, AppError> {
        let cfg = self.backend.load_project()?;
        if !cfg.workflow.states.iter().any(|s| s == to_state) {
            return Err(CoreError::UnknownState(to_state.to_string()).into());
        }
        let now = Self::now();
        let mut paths = Vec::new();
        let mut moved = 0;
        for key in keys {
            let mut task = self.get_by_key(key)?;
            if task.state == to_state {
                continue;
            }
            let expected = revision(&task);
            task.state = to_state.to_string();
            task.updated = now;
            self.backend.put(task.clone(), Some(expected))?;
            paths.extend(self.location(&task.uid));
            moved += 1;
        }
        if moved > 0 {
            self.finish_batch(&format!("move {moved} task(s) -> {to_state}"), &paths);
        }
        Ok(moved)
    }

    /// The tail every batch write shares: refresh the cache once, commit once.
    /// Pairing these by hand at each call site is how one of them gets left
    /// out, or run per task and turned into N commits in the user's vault.
    fn finish_batch(&self, message: &str, paths: &[String]) {
        self.refresh_cache_or_warn();
        self.commit_or_warn(message, paths);
    }

    /// Resolves every key before any of them is written, so a typo in the
    /// last key of a batch does not leave the first three already applied.
    /// Deduplicates by uid: naming one task twice must not write it twice.
    fn resolve_batch(&self, keys: &[TaskKey]) -> Result<Vec<Task>, AppError> {
        let mut seen = BTreeSet::new();
        let mut tasks = Vec::new();
        for key in keys {
            let task = self.get_by_key(key)?;
            if seen.insert(task.uid.clone()) {
                tasks.push(task);
            }
        }
        Ok(tasks)
    }

    /// `cadet done`, for any number of keys. Unlike `move_tasks` this honours
    /// the transition graph — a workflow that forbids todo→done must still
    /// forbid it here — and like `resolve_batch` it checks every task before
    /// writing any.
    pub fn complete_tasks(&self, keys: &[TaskKey]) -> Result<Vec<TaskKey>, AppError> {
        let cfg = self.backend.load_project()?;
        let mut tasks = self.resolve_batch(keys)?;
        for t in &tasks {
            check_transition(&cfg.workflow, &t.state, "done")?;
        }
        let now = Self::now();
        let mut paths = Vec::new();
        for task in &mut tasks {
            let expected = revision(task);
            task.state = "done".to_string();
            task.updated = now;
            self.backend.put(task.clone(), Some(expected))?;
            paths.extend(self.location(&task.uid));
        }
        match tasks.as_slice() {
            [] => {}
            [one] => self.finish_batch(&format!("{} -> done", one.key), &paths),
            many => self.finish_batch(&format!("done {} task(s)", many.len()), &paths),
        }
        Ok(tasks.into_iter().map(|t| t.key).collect())
    }

    /// `cadet rm`, for any number of keys. Ten keys leave one commit.
    pub fn delete_many(&self, keys: &[TaskKey]) -> Result<Vec<TaskKey>, AppError> {
        let tasks = self.resolve_batch(keys)?;
        let mut paths = Vec::new();
        for task in &tasks {
            let expected = revision(task);
            // Resolved before the delete: afterwards there is no file to
            // locate, and the safety net would never record the removal.
            paths.extend(self.location(&task.uid));
            self.backend.delete(task.uid.clone(), Some(expected))?;
            // An explicit `cadet rm` is certain, not inferred — the file's
            // absence is never going to be resolved by a sync tool catching
            // up. Retire the identity outright instead of leaving it in
            // `entries` for the next `reconcile` to rediscover, which would
            // look exactly like an ordinary external deletion: routed through
            // the deletion grace period (and counted in its mass-deletion
            // `ScanRejected` guard) for no reason, and misreported by
            // `cadet doctor` as still pending when the user already confirmed
            // it.
            self.index.forget(&self.project, &task.uid)?;
        }
        match tasks.as_slice() {
            [] => {}
            [one] => self.finish_batch(&format!("remove {}", one.key), &paths),
            many => self.finish_batch(&format!("remove {} task(s)", many.len()), &paths),
        }
        Ok(tasks.into_iter().map(|t| t.key).collect())
    }

    /// Every task whose state is not declared in the project's workflow —
    /// stranded by a hand edit, a pull, or another machine. The ordinary
    /// write path refuses all of them, so `doctor` reports them and
    /// `move_tasks` is the way out.
    pub fn stranded(&self) -> Result<Vec<TaskSummary>, AppError> {
        let cfg = self.backend.load_project()?;
        Ok(self
            .list_filtered(true, &TaskFilter::default())?
            .into_iter()
            .filter(|s| !cfg.workflow.states.contains(&s.state))
            .collect())
    }

    /// The file to open in `$EDITOR`, or an error for a backend that has no
    /// files. Resolved before anything is spawned, so an unsupported backend
    /// never launches an editor on nothing.
    pub fn edit_path(&self, key: &TaskKey) -> Result<String, AppError> {
        let task = self.get_by_key(key)?;
        match self.backend.location_of(task.uid)? {
            Some(p) => Ok(p),
            None => Err(BackendError::Unsupported {
                capability: "edit".to_string(),
            }
            .into()),
        }
    }

    /// Records a hand edit in the safety net. `GitNet::commit` no-ops on an
    /// empty diff, so an editor the user quit without saving leaves nothing
    /// behind.
    pub fn record_edit(&self, key: &TaskKey, path: &str) {
        self.commit_or_warn(
            &format!("edit {key}"),
            std::slice::from_ref(&path.to_string()),
        );
    }

    /// One key is a batch of one — same function, so the single and multi
    /// paths cannot drift apart.
    pub fn delete(&self, key: &TaskKey) -> Result<(), AppError> {
        self.delete_many(std::slice::from_ref(key)).map(|_| ())
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
        let mut rows =
            self.index
                .list_tasks(&self.project, include_terminal, &cfg.workflow.terminal)?;
        if !filter.is_empty() {
            rows.retain(|s| {
                filter.matches(&FilterTarget {
                    state: &s.state,
                    due: s.due.as_deref(),
                    priority: s.priority,
                    tags: &s.tags,
                    fields: &s.fields,
                })
            });
        }
        // The declared state order is the column order, Trello-style. The
        // sort is stable, so SQL's due/priority/key ordering survives inside
        // each group; a state the workflow no longer declares sorts last,
        // where `doctor` tells you to go looking for it. The store stays
        // workflow-agnostic — it has no idea states are ordered.
        rows.sort_by_key(|s| {
            cfg.workflow
                .states
                .iter()
                .position(|st| st == &s.state)
                .unwrap_or(cfg.workflow.states.len())
        });
        Ok(rows)
    }

    /// Unlike the write paths above, a failed undo must fail loudly: undo is
    /// not itself a write with a durable backend result to fall back on, so
    /// there is no already-successful outcome to protect by swallowing the
    /// error into a warning.
    ///
    /// A backend with no safety net has no undo either, and that must fail
    /// before any other work: a message about an empty repository would name
    /// the wrong cause when the truth is that this backend has nothing for
    /// git to hold at all.
    pub fn undo(&self) -> Result<(), AppError> {
        let Some(git) = &self.git else {
            return Err(BackendError::Unsupported {
                capability: "undo".to_string(),
            }
            .into());
        };
        git.undo()?;
        Ok(())
    }
}
