use crate::git::GitNet;
use cadet_core::*;
use cadet_store_sqlite::{SqliteIndex, TaskSummary};

#[derive(Debug, Default, PartialEq)]
pub struct ReconcileReport {
    pub adopted: usize,
    pub pending_adoption: usize,
    pub updated: usize,
    pub renamed: usize,
    pub copies: usize,
    pub pending_deletion: usize,
    pub deleted: usize,
    pub renumbered: usize,
    pub pending_renumber: usize,
    /// `None` when the scan was trusted. The reason matters to the user:
    /// "a large number of tasks disappeared" and "one file could not be read"
    /// call for completely different responses.
    pub scan_rejected: Option<RejectReason>,
}

pub const GRACE_MS: i64 = 60_000;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Index(#[from] cadet_store_sqlite::IndexError),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Git(#[from] crate::git::GitError),
    #[error("no task with key {0}")]
    NoSuchKey(String),
}

pub struct App {
    pub(crate) backend: Box<dyn Backend>,
    pub(crate) index: SqliteIndex,
    pub(crate) git: GitNet,
    pub(crate) project: String,
    /// Non-fatal warnings queued by a write (currently: a failed safety-net
    /// commit). The backend is the source of truth and git is a local
    /// convenience on top of it, so a commit failure must never fail the
    /// operation that produced it — but it also must not vanish silently.
    /// `app` is a library and must not print to the terminal itself; a
    /// caller (the CLI) drains this and decides how to show it.
    pub(crate) warnings: std::sync::Mutex<Vec<String>>,
}

impl App {
    pub fn new(
        backend: Box<dyn Backend>,
        index: SqliteIndex,
        git: GitNet,
        project: String,
    ) -> Self {
        Self {
            backend,
            index,
            git,
            project,
            warnings: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn clear_index(&self) -> Result<(), AppError> {
        self.index.clear(&self.project)?;
        Ok(())
    }

    /// Records a non-fatal warning for a caller to surface later. Never
    /// fails: a poisoned lock still gets the warning appended rather than
    /// losing it.
    pub(crate) fn warn(&self, message: String) {
        let mut w = self.warnings.lock().unwrap_or_else(|e| e.into_inner());
        w.push(message);
    }

    /// Takes and clears every warning queued since the last drain. Intended
    /// for a caller (the CLI) to poll after a write.
    pub fn drain_warnings(&self) -> Vec<String> {
        let mut w = self.warnings.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut w)
    }

    pub fn reconcile(&self, now_ms: i64) -> Result<ReconcileReport, AppError> {
        self.reconcile_with(now_ms, false, false)
    }

    /// Adopts every currently pending hand-written note immediately,
    /// skipping the rest of its grace period: the user invoking `cadet
    /// adopt` is exactly the confirmation the grace period exists to wait
    /// for. Deletion grace periods are untouched.
    pub fn adopt_pending(&self, now_ms: i64) -> Result<ReconcileReport, AppError> {
        self.reconcile_with(now_ms, true, false)
    }

    /// Reconciles as `reconcile` does, except a uid observed absent for the
    /// FIRST time in this pass is deleted at once instead of entering the
    /// normal deletion grace period. Meant to be called exactly once,
    /// immediately after `undo`: a `git reset --hard` makes an absence
    /// intentional, not a possible sync artefact, so there is nothing to
    /// wait out. A uid that already had a `pending_deletions` record before
    /// this pass — mid grace period for a reason unrelated to the undo —
    /// is left exactly as it was; see `ScanClock::immediate_deletion`.
    pub fn reconcile_after_undo(&self, now_ms: i64) -> Result<ReconcileReport, AppError> {
        self.reconcile_with(now_ms, false, true)
    }

    fn reconcile_with(
        &self,
        now_ms: i64,
        force_adopt: bool,
        immediate_deletion: bool,
    ) -> Result<ReconcileReport, AppError> {
        let ChangeSet::Snapshot {
            snapshot: snap,
            tasks: mut parsed,
        } = self.backend.scan(None)?
        else {
            // `backend-fs` never produces a Delta; a Delta here is a programming error.
            return Ok(ReconcileReport::default());
        };
        // Spec §5: the high-water mark is the maximum over the keys LIVE IN
        // THE SNAPSHOT, the stored mark, and quarantined files' keys — not
        // the stored mark alone. Deleting the index takes the stored mark
        // with it, and a first sync from another device never had one, so
        // without this every rebuild starts minting at 1 again and hands out
        // keys that are already on disk. Must run before the outcomes loop:
        // `adopt` mints from this mark.
        let prefix = self.backend.load_project()?.prefix;
        if let Some(max) = parsed
            .values()
            .filter(|t| t.key.prefix == prefix)
            .map(|t| t.key.number)
            .max()
        {
            self.index.bump_high_water(&self.project, max)?;
        }

        let view = self.index.view(&self.project)?;
        let clock = ScanClock {
            now_ms,
            grace_ms: GRACE_MS,
            immediate_deletion,
        };
        let mut outcomes = resolve_identity(&snap, &view, clock);
        if force_adopt {
            for o in outcomes.iter_mut() {
                match o {
                    Outcome::PendingAdoption { path } => {
                        *o = Outcome::Adopt { path: path.clone() };
                    }
                    Outcome::PendingCopy { source, path } => {
                        *o = Outcome::Copy {
                            source: source.clone(),
                            path: path.clone(),
                        };
                    }
                    _ => {}
                }
            }
        }

        let mut report = ReconcileReport::default();
        if let Some(Outcome::ScanRejected { reason }) = outcomes
            .iter()
            .find(|o| matches!(o, Outcome::ScanRejected { .. }))
        {
            report.scan_rejected = Some(reason.clone());
            return Ok(report);
        }

        let mut entries: Vec<IndexEntry> = Vec::new();
        let observed_by_path = |p: &str| snap.observed.iter().find(|o| o.path == p);

        // Tasks the scan didn't touch (only a `PendingDeletion` outcome can
        // leave a known uid off disk this cycle) still have to survive the
        // `cache_tasks` replace below, or a task in its grace period would
        // vanish from `list()` a full deletion cycle early.
        let cached_by_uid: std::collections::BTreeMap<String, TaskSummary> = self
            .index
            .list_tasks(&self.project, true, &[])?
            .into_iter()
            .map(|s| (s.uid.clone(), s))
            .collect();

        for outcome in &outcomes {
            match outcome {
                Outcome::PendingAdoption { path } => {
                    report.pending_adoption += 1;
                    if let Some(o) = observed_by_path(path) {
                        self.index
                            .mark_pending(&self.project, path, &o.revision, now_ms)?;
                    }
                }
                Outcome::Adopt { path } => {
                    report.adopted += 1;
                    // Clear the pending-adoption record for this path. Paths, unlike
                    // uids, are reused — delete a file and create another at the same
                    // name and a stale `since_ms` would let the newcomer adopt with no
                    // grace period at all. Same bug class as `clear_pending_deletion`,
                    // on the adoption side.
                    self.index.clear_pending(&self.project, path)?;
                    match observed_by_path(path).and_then(|o| o.uid.clone()) {
                        // The observed file already carries a valid uid — the index
                        // just doesn't recognise it (e.g. right after `clear_index`).
                        // Register the existing identity as-is: `self.adopt` calls
                        // `Backend::adopt`, which unconditionally stamps a FRESH uid,
                        // and would otherwise mint a new identity for every task on
                        // the vault on every rebuild.
                        Some(uid) => {
                            if let Some(o) = observed_by_path(path) {
                                entries.push(IndexEntry {
                                    uid,
                                    path: path.clone(),
                                    revision: o.revision.clone(),
                                    first_seen_ms: now_ms,
                                });
                            }
                        }
                        None => self.adopt(path, now_ms, &mut entries, &mut parsed)?,
                    }
                }
                Outcome::Update { uid, path } => {
                    report.updated += 1;
                    // Reclaiming a uid MUST clear any pending-deletion record for it.
                    // Otherwise a task that vanishes, returns, then vanishes again is
                    // deleted immediately next scan: the stale timestamp is already
                    // older than the grace period, so guard 3 never fires.
                    // `resolve_identity` cannot do this itself — it is pure and holds
                    // only an immutable `&IndexView`.
                    self.index.clear_pending_deletion(&self.project, uid)?;
                    if let Some(o) = observed_by_path(path) {
                        entries.push(IndexEntry {
                            uid: uid.clone(),
                            path: path.clone(),
                            revision: o.revision.clone(),
                            first_seen_ms: now_ms,
                        });
                    }
                }
                Outcome::Rename { uid, to } => {
                    report.renamed += 1;
                    self.index.clear_pending_deletion(&self.project, uid)?;
                    if let Some(o) = observed_by_path(to) {
                        entries.push(IndexEntry {
                            uid: uid.clone(),
                            path: to.clone(),
                            revision: o.revision.clone(),
                            first_seen_ms: now_ms,
                        });
                    }
                }
                Outcome::PendingCopy { path, .. } => {
                    report.pending_adoption += 1;
                    if let Some(o) = observed_by_path(path) {
                        self.index
                            .mark_pending(&self.project, path, &o.revision, now_ms)?;
                    }
                    // Drop it from the parsed set for as long as it is
                    // pending. Its uid still belongs to the file it was
                    // copied from, and both the `entries` and `tasks` tables
                    // are keyed on `(project, uid)` — caching it here would
                    // overwrite the original with the copy rather than show
                    // both.
                    parsed.remove(path);
                }
                Outcome::Copy { path, .. } => {
                    report.copies += 1;
                    // Same reason `Adopt` clears it: this path's pending
                    // record has served its purpose, and paths are reused. A
                    // stale row would grant whatever file appears here next
                    // an adoption with no grace period at all.
                    self.index.clear_pending(&self.project, path)?;
                    self.adopt(path, now_ms, &mut entries, &mut parsed)?;
                }
                Outcome::PendingDeletion { uid } => {
                    report.pending_deletion += 1;
                    self.index
                        .mark_pending_deletion(&self.project, uid, now_ms)?;
                    if let Some(e) = view.entries.iter().find(|e| &e.uid == uid) {
                        entries.push(e.clone());
                    }
                }
                Outcome::Delete { uid } => {
                    report.deleted += 1;
                    // A confirmed deletion must retire its `pending_deletions`
                    // row, not just leave it stale: `carry_absent_tasks`
                    // (see below, and `refresh_cache`) trusts that table as
                    // "still mid grace period" — an uncleared row would let a
                    // just-deleted task get silently resurrected into the
                    // cache by nothing more than an unrelated later write.
                    self.index.clear_pending_deletion(&self.project, uid)?;
                }
                Outcome::ScanRejected { .. } => unreachable!("handled above"),
            }
        }

        self.renumber_duplicate_keys(
            now_ms,
            &prefix,
            snap.complete,
            &mut parsed,
            &mut entries,
            &mut report,
        )?;

        self.index.apply(&self.project, &entries)?;
        // `apply` replaces `entries` wholesale, so any uid it just dropped
        // leaves its pending-deletion row behind with nothing left to reap
        // it. Read AFTER `apply` for exactly that reason.
        self.index.reap_orphaned_pending_deletions(&self.project)?;
        // Cache the parsed content so every later read is a SQL query, never a
        // file read (spec §3). The scan already parsed these — this is free.
        // `carry_absent_tasks` adds back the tasks the scan didn't see
        // this cycle but that are only pending deletion, not deleted yet —
        // read AFTER the loop above so it sees this pass's own
        // mark_pending_deletion/clear_pending_deletion calls.
        let observed_uids: std::collections::BTreeSet<&str> = snap
            .observed
            .iter()
            .filter_map(|o| o.uid.as_ref().map(TaskUid::as_str))
            .collect();
        let pending_deletions = self.index.view(&self.project)?.pending_deletions;
        let mut cached: Vec<Task> = parsed.into_values().collect();
        cached.extend(carry_absent_tasks(
            &pending_deletions,
            cached_by_uid.values(),
            &observed_uids,
            snap.complete,
        ));
        self.index.cache_tasks(&self.project, &cached)?;
        Ok(report)
    }

    /// Applies the §5 whole-scan collision rule: every task claiming a key
    /// another task also claims is renumbered from the high-water mark, in
    /// `(created, canonical_hash, path)` order, and the loser records
    /// `renumbered_from`.
    ///
    /// The resolved key lands in `parsed` — and therefore in the index —
    /// immediately, but the *file* write waits out the grace period, because
    /// renumbering is one of the three situations in which reconcile mutates
    /// a user file (§5). Splitting it that way is what keeps the index
    /// duplicate-free while the countdown runs: two live tasks under one key
    /// have no answer for `cadet show`, and `tasks_unique_key` now refuses to
    /// store them at all.
    fn renumber_duplicate_keys(
        &self,
        now_ms: i64,
        prefix: &str,
        complete: bool,
        parsed: &mut std::collections::BTreeMap<String, Task>,
        entries: &mut [IndexEntry],
        report: &mut ReconcileReport,
    ) -> Result<(), AppError> {
        // An incomplete snapshot is not a whole-scan view, and the rule is
        // defined over one: renumbering on a partial tree could move a key
        // away from the task that legitimately owns it.
        if !complete {
            return Ok(());
        }
        let high_water = self.index.high_water(&self.project)?;
        let resolutions = resolve_collisions(collision_candidates(parsed, prefix), high_water);
        let waiting = self.index.pending_renumbers(&self.project)?;

        for r in resolutions {
            let (Some(new_key), Some(from)) = (r.new_key, r.renumbered_from) else {
                self.index.clear_pending_renumber(&self.project, &r.path)?;
                continue;
            };
            let Some(task) = parsed.get_mut(&r.path) else {
                continue;
            };
            let current = revision(task);
            task.key = new_key.clone();
            task.renumbered_from = Some(from);

            let ready = waiting
                .get(&r.path)
                .is_some_and(|(rev, since)| rev == &current && now_ms - since >= GRACE_MS);
            if !ready {
                // Deliberately no `bump_high_water` here. The mark is what
                // `resolve_collisions` counts up from, so raising it for a key
                // that has not been written yet would hand the same file a
                // different key on every poll, and the index would show a key
                // that never settles.
                report.pending_renumber += 1;
                self.index
                    .mark_pending_renumber(&self.project, &r.path, &current, now_ms)?;
                continue;
            }
            let written = self.backend.put(task.clone(), Some(current))?;
            self.index.bump_high_water(&self.project, new_key.number)?;
            self.index.clear_pending_renumber(&self.project, &r.path)?;
            report.renumbered += 1;
            if let Some(e) = entries.iter_mut().find(|e| e.uid == task.uid) {
                e.revision = written;
            }
        }
        Ok(())
    }

    /// Stamps `uid` and `key` into a file that lacks them. The only case where
    /// Cadet modifies a file it did not create, and it is purely additive (§5).
    /// All file I/O goes through the backend — `app` never touches the work tree.
    fn adopt(
        &self,
        path: &str,
        now_ms: i64,
        entries: &mut Vec<IndexEntry>,
        parsed: &mut std::collections::BTreeMap<String, Task>,
    ) -> Result<(), AppError> {
        let cfg = self.backend.load_project()?;
        let next = self.index.high_water(&self.project)? + 1;
        let uid = TaskUid::generate();
        let key = TaskKey::new(cfg.prefix.clone(), next);
        let now = jiff::Timestamp::from_millisecond(now_ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH);

        let task = self
            .backend
            .adopt(path.to_string(), uid.clone(), key, now)?;

        self.index.bump_high_water(&self.project, next)?;
        entries.push(IndexEntry {
            uid,
            path: path.to_string(),
            revision: revision(&task),
            first_seen_ms: now_ms,
        });
        parsed.insert(path.to_string(), task);
        Ok(())
    }
}

/// The §5 collision input: every parsed task carrying this project's prefix.
/// A task whose `key` did not parse reads as the `?-0` placeholder and is
/// excluded — those are not competing for a real key, and renumbering them
/// would invent one under the wrong prefix.
pub(crate) fn collision_candidates(
    parsed: &std::collections::BTreeMap<String, Task>,
    prefix: &str,
) -> Vec<Candidate> {
    parsed
        .iter()
        .filter(|(_, t)| t.key.prefix == prefix)
        .map(|(path, task)| Candidate {
            task: task.clone(),
            path: path.clone(),
        })
        .collect()
}

/// Task placeholders for every uid the project's index currently has
/// recorded as mid pending-deletion grace period, for any such uid
/// `observed` (the uids a fresh scan just saw) doesn't cover. Without this,
/// a task counting down its grace period vanishes from `list()` the instant
/// *anything* replaces the cache — not only the reconcile pass that first
/// noticed its absence. Shared by `reconcile_with` (whose own outcomes loop
/// keeps `pending_deletions` current before calling this) and
/// `App::refresh_cache` in `write.rs` (which has no identity-resolution
/// pass of its own and would otherwise trust a bare scan that cannot tell
/// "gone" from "still counting down").
pub(crate) fn carry_absent_tasks<'a>(
    pending_deletions: &std::collections::BTreeMap<TaskUid, i64>,
    cached: impl Iterator<Item = &'a TaskSummary>,
    observed: &std::collections::BTreeSet<&str>,
    complete: bool,
) -> Vec<Task> {
    cached
        .filter(|s| !observed.contains(s.uid.as_str()))
        .filter(|s| {
            // An incomplete snapshot is not evidence of absence at all (§5
            // guard 1) — a single unreadable file or an unmaterialised cloud
            // placeholder makes it so. Under one, nothing the scan missed may
            // be dropped, whatever its deletion state.
            !complete || TaskUid::parse(&s.uid).is_some_and(|u| pending_deletions.contains_key(&u))
        })
        .map(task_from_summary)
        .collect()
}

/// Reconstructs just enough of a `Task` to round-trip through `cache_tasks`,
/// for a task the scan didn't observe this cycle (only pending deletion).
/// `cache_tasks` only persists uid/key/title/state/due/priority — every
/// other field here is a placeholder and never reaches the `tasks` table.
fn task_from_summary(s: &TaskSummary) -> Task {
    Task {
        uid: TaskUid::parse(&s.uid).unwrap_or_else(TaskUid::generate),
        key: s.key.clone(),
        title: s.title.clone(),
        state: s.state.clone(),
        created: jiff::Timestamp::UNIX_EPOCH,
        updated: jiff::Timestamp::UNIX_EPOCH,
        due: s.due.clone(),
        priority: s.priority,
        tags: vec![],
        renumbered_from: None,
        possible_duplicate_of: None,
        fields: std::collections::BTreeMap::new(),
        body: String::new(),
    }
}
