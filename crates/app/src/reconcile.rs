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

fn snapshot_matches_index(snapshot: &Snapshot, view: &IndexView) -> bool {
    if !snapshot.complete || snapshot.observed.len() != view.entries.len() {
        return false;
    }
    let mut observed_set = std::collections::BTreeSet::new();
    for observed in &snapshot.observed {
        let Some(uid) = &observed.uid else {
            return false;
        };
        if !observed_set.insert((uid, observed.path.as_str(), &observed.revision)) {
            return false;
        }
    }
    let indexed: std::collections::BTreeSet<_> = view
        .entries
        .iter()
        .map(|entry| (&entry.uid, entry.path.as_str(), &entry.revision))
        .collect();
    observed_set == indexed
}

fn entries_match(current: &[IndexEntry], next: &[IndexEntry]) -> bool {
    if current.len() != next.len() {
        return false;
    }
    fn identity(entry: &IndexEntry) -> (&TaskUid, &str, &Revision) {
        (&entry.uid, entry.path.as_str(), &entry.revision)
    }
    let current: std::collections::BTreeSet<_> = current.iter().map(identity).collect();
    let next: std::collections::BTreeSet<_> = next.iter().map(identity).collect();
    current == next
}

fn cache_matches<'a>(
    task_count: usize,
    tasks: impl Iterator<Item = &'a Task>,
    cached: &std::collections::BTreeMap<String, TaskSummary>,
) -> bool {
    task_count == cached.len()
        && tasks.into_iter().all(|task| {
            cached.get(task.uid.as_str()).is_some_and(|summary| {
                summary.key == task.key
                    && summary.title == task.title
                    && summary.state == task.state
                    && summary.due == task.due
                    && summary.priority == task.priority
                    && summary.tags == task.tags
                    && summary.fields == task.fields
            })
        })
}

/// What `cadet doctor` reports about renumbering. Both figures are standing
/// state read fresh, not a tally of what one reconcile pass did.
#[derive(Debug, Default, PartialEq)]
pub struct RenumberStatus {
    /// Tasks on disk carrying a `renumbered_from` breadcrumb.
    pub recorded: usize,
    /// Collisions resolved in the index but still waiting out the §5 grace
    /// period before the file is rewritten.
    pub pending: usize,
}

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
    pub(crate) git: Option<GitNet>,
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
        git: Option<GitNet>,
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

    /// Standing renumber state, for `cadet doctor`.
    ///
    /// Deliberately not `ReconcileReport::renumbered`. A renumber fires in
    /// whichever command reconciles first — almost never `doctor` itself —
    /// so by the time a user asks, this pass has nothing left to do and the
    /// report reads `renumbered: 0`, which is indistinguishable from "no
    /// renumber ever happened". `recorded` counts the durable evidence
    /// instead: the `renumbered_from` breadcrumb §5 requires the loser to
    /// carry. Worth the extra scan — `doctor` is a diagnostic, not a hot path.
    pub fn renumber_status(&self) -> Result<RenumberStatus, AppError> {
        let recorded = match self.backend.scan(None)? {
            ChangeSet::Snapshot { tasks, .. } => tasks
                .values()
                .filter(|t| t.renumbered_from.is_some())
                .count(),
            _ => 0,
        };
        Ok(RenumberStatus {
            recorded,
            pending: self.index.pending_renumbers(&self.project)?.len(),
        })
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
        // A backend that has never handed out a cursor gets asked from
        // sequence zero rather than `None`. `ChangeSet::Snapshot` carries no
        // cursor, so a reconcile that took the snapshot path would have
        // nothing to store and the next scan would be a full scan again,
        // forever. Asking from zero instead makes a delta-capable backend
        // return everything it has *and* a cursor — the same outcome as a
        // snapshot, plus what makes the next scan cheap.
        // `backend-markdown` ignores the argument and always returns a
        // `Snapshot`, so this changes nothing for it.
        let since = self
            .index
            .cursor(&self.project)?
            .unwrap_or_else(|| Cursor(b"0".to_vec()));
        let (snap, mut parsed, snap_cursor) = match self.backend.scan(Some(since))? {
            ChangeSet::Snapshot {
                snapshot,
                tasks,
                cursor,
            } => (snapshot, tasks, cursor),
            ChangeSet::Delta {
                upserts,
                deletes,
                cursor,
            } => {
                return self.apply_delta(upserts, deletes, cursor, now_ms);
            }
        };
        // Spec §5: the high-water mark is the maximum over the keys LIVE IN
        // THE SNAPSHOT, the stored mark, and quarantined files' keys — not
        // the stored mark alone. Deleting the index takes the stored mark
        // with it, and a first sync from another device never had one, so
        // without this every rebuild starts minting at 1 again and hands out
        // keys that are already on disk. Must run before the outcomes loop:
        // `adopt` mints from this mark.
        let prefix = self.backend.load_project()?.prefix;
        let max_key = parsed
            .values()
            .filter(|t| t.key.prefix == prefix)
            .map(|t| t.key.number)
            .max();
        if let Some(max) = max_key {
            self.index.bump_high_water(&self.project, max)?;
        }

        let view = self.index.view(&self.project)?;
        let cached_by_uid: std::collections::BTreeMap<String, TaskSummary> = self
            .index
            .list_tasks(&self.project, true, &[])?
            .into_iter()
            .map(|summary| (summary.uid.clone(), summary))
            .collect();
        if snapshot_matches_index(&snap, &view)
            && view.pending.is_empty()
            && view.pending_deletions.is_empty()
            && self.index.pending_renumbers(&self.project)?.is_empty()
            && cache_matches(parsed.len(), parsed.values(), &cached_by_uid)
        {
            return Ok(ReconcileReport::default());
        }
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
                        None => self.adopt(path, now_ms, &mut entries, &mut parsed, None)?,
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
                    if view.pending_deletions.contains_key(uid) {
                        self.index.clear_pending_deletion(&self.project, uid)?;
                    }
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
                    if view.pending_deletions.contains_key(uid) {
                        self.index.clear_pending_deletion(&self.project, uid)?;
                    }
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
                Outcome::Copy { source, path } => {
                    report.copies += 1;
                    // Same reason `Adopt` clears it: this path's pending
                    // record has served its purpose, and paths are reused. A
                    // stale row would grant whatever file appears here next
                    // an adoption with no grace period at all.
                    self.index.clear_pending(&self.project, path)?;
                    // A file that silently lost its identity must say so.
                    // This is the only path that hands a file a uid it did
                    // not have before, and it is not the renumber path, so
                    // `renumbered_from` never gets written here — without
                    // this the copy carries no breadcrumb at all.
                    self.adopt(path, now_ms, &mut entries, &mut parsed, Some(source))?;
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

        let resolved = self.resolve_duplicates(&mut parsed, &prefix)?;
        self.apply_renumbers(
            now_ms,
            snap.complete,
            &resolved,
            &parsed,
            &mut entries,
            &mut report,
        )?;

        if !entries_match(&view.entries, &entries) {
            self.index.apply(&self.project, &entries)?;
        }
        // `apply` replaces `entries` wholesale, so any uid it just dropped
        // leaves its pending-deletion row behind with nothing left to reap
        // it. Read AFTER `apply` for exactly that reason.
        let live_paths: std::collections::BTreeSet<String> =
            snap.observed.iter().map(|o| o.path.clone()).collect();
        self.index.reap_orphans(
            &self.project,
            // An incomplete scan is not evidence a path is gone, so the
            // path-keyed tables keep their rows until a whole-scan view says
            // otherwise. Same reasoning as §5 guard 1 for deletions.
            snap.complete.then_some(&live_paths),
        )?;
        // Cache the parsed content so every later read is a SQL query, never a
        // file read (spec §3). The scan already parsed these — this is free.
        // Runs AFTER the outcomes loop and the reap above so it sees this
        // pass's own mark_pending_deletion/clear_pending_deletion calls.
        let observed_uids: std::collections::BTreeSet<&str> = snap
            .observed
            .iter()
            .filter_map(|o| o.uid.as_ref().map(TaskUid::as_str))
            .collect();
        let cached = self.assemble_cache(
            snap.complete,
            parsed,
            &observed_uids,
            cached_by_uid.values(),
        )?;
        if !cache_matches(cached.len(), cached.iter(), &cached_by_uid) {
            self.index.cache_tasks(&self.project, &cached)?;
        }
        // A backend that can resume incrementally hands back a cursor even
        // on the snapshot path (a stale or lost cursor falling back to a
        // full resync, or a first-ever scan) — store it, or every reconcile
        // after this one asks from zero again and gets a full snapshot
        // forever, having gained nothing from the resync it just paid for.
        // `backend-markdown` always returns `None` here, so this is a no-op
        // for it. Last, mirroring `apply_delta`: a failure before this line
        // just repeats the same (idempotent) snapshot next time.
        if let Some(c) = snap_cursor {
            self.index.set_cursor(&self.project, &c)?;
        }
        Ok(report)
    }

    /// A delta is authoritative. The adoption and deletion grace periods exist
    /// because a filesystem is ambiguous — a missing file might be deleted or
    /// might be a sync tool mid-flight. A backend that tells you what changed
    /// has no such ambiguity, so none of that machinery applies.
    fn apply_delta(
        &self,
        upserts: Vec<Task>,
        deletes: Vec<TaskUid>,
        cursor: Cursor,
        now_ms: i64,
    ) -> Result<ReconcileReport, AppError> {
        let mut report = ReconcileReport::default();

        for uid in &deletes {
            self.index.forget(&self.project, uid)?;
            report.deleted += 1;
        }

        let prefix = self.backend.load_project()?.prefix;

        // §5's duplicate-key sweep, reused rather than reimplemented (the
        // snapshot path's `cache_tasks` enforces the same `tasks_unique_key`
        // constraint, and `assemble_cache`'s comment already names the
        // stakes: "letting it through kills every command in the project,
        // `ls` included"). Two upserts in the same delta can legitimately
        // share a key — independent devices minting the same next-available
        // number before syncing — so this has to run before either lands in
        // the cache. Unlike the snapshot path there is no grace period to
        // wait out: the delta already told us this happened, authoritatively,
        // so the loser is renumbered and written back immediately rather than
        // queued in `pending_renumbers`.
        let mut parsed: std::collections::BTreeMap<String, Task> = upserts
            .into_iter()
            .map(|t| (t.uid.as_str().to_string(), t))
            .collect();
        let resolved = self.resolve_duplicates(&mut parsed, &prefix)?;
        // `resolve_duplicates` fills both halves of a `DuplicateResolution`,
        // so both arms have to spend both. This is `apply_renumbers`'s opening
        // loop, which the snapshot arm runs for exactly the same reason: a
        // path that came back settled kept the key it already had, and any
        // `pending_renumbers` row still held for it is obsolete. `reap_orphans`
        // does not cover it — the delta arm passes `None` for `live_paths`, and
        // a settled task is live anyway. Left behind, the row makes `cadet
        // doctor` report a pending renumber forever, and its grace check is
        // already satisfied, so the next duplicate to land at that path is
        // renumbered with no grace period at all.
        for path in &resolved.settled {
            self.index.clear_pending_renumber(&self.project, path)?;
        }
        for change in &resolved.changes {
            let Some(task) = parsed.get(&change.path) else {
                continue;
            };
            self.backend
                .put(task.clone(), Some(change.on_disk.clone()))?;
            self.index
                .bump_high_water(&self.project, change.to.number)?;
            report.renumbered += 1;
        }
        // A duplicate can also arrive one upsert at a time: this task's key
        // may collide with a DIFFERENT uid the cache already holds from an
        // earlier delta — `resolve_duplicates` only sees this batch, so it
        // cannot catch that. Renumber only the incoming task: it is the one
        // task in this collision we have a full, real record for (fresh from
        // the backend); the already-cached task is only a display summary,
        // not enough to safely rewrite without risking real data loss.
        //
        // The counter's floor is `resolve_collisions`'s, carried across rather
        // than reinvented: the greater of the stored high-water mark and the
        // maximum key number in this batch. `high_water + 1` alone hands the
        // loser a number a legitimate task in the SAME batch is already
        // holding — neither the batch's own `resolve_duplicates` (both keys
        // were distinct when it ran) nor `find_by_key` (the other task is not
        // cached yet) can see that, and the collision only surfaces as a
        // `tasks_unique_key` abort after the rename has already been written
        // to the store.
        let mut next = self.index.high_water(&self.project)?.max(
            parsed
                .values()
                .map(|t| t.key.number)
                .max()
                .unwrap_or_default(),
        );
        for task in parsed.values_mut() {
            let Some(existing) = self.index.find_by_key(&self.project, &task.key)? else {
                continue;
            };
            if existing.uid == task.uid.as_str() {
                continue;
            }
            let on_disk = revision(task);
            next += 1;
            task.renumbered_from = Some(task.key.clone());
            task.key = TaskKey::new(prefix.clone(), next);
            self.backend.put(task.clone(), Some(on_disk))?;
            self.index.bump_high_water(&self.project, next)?;
            report.renumbered += 1;
        }

        if let Some(max) = parsed
            .values()
            .filter(|t| t.key.prefix == prefix)
            .map(|t| t.key.number)
            .max()
        {
            self.index.bump_high_water(&self.project, max)?;
        }

        // A task this project's index has never heard of is an adoption, not
        // an update — matching the snapshot path's own `Outcome::Adopt` vs.
        // `Outcome::Update` split. Read before `apply_upsert` mutates
        // `entries`, or every upsert would already look known.
        let known: std::collections::BTreeSet<TaskUid> = self
            .index
            .view(&self.project)?
            .entries
            .into_iter()
            .map(|e| e.uid)
            .collect();
        for t in parsed.values() {
            if known.contains(&t.uid) {
                report.updated += 1;
            } else {
                report.adopted += 1;
            }
            // The same rule the snapshot arm's `Update` and `Rename` outcomes
            // apply, for the same reason: reclaiming a uid MUST retire any
            // pending-deletion record for it, or a task that vanishes,
            // returns, then vanishes again is deleted immediately next scan.
            // `reap_orphans` below does not cover it — `apply_upsert` puts the
            // uid back in `entries`, so the stale row has an owner and
            // survives. A local-db project only writes such a record from the
            // snapshot arm (a stale cursor, or a rebuilt index), which is
            // exactly the state this arm then has to clear up after.
            self.index.clear_pending_deletion(&self.project, &t.uid)?;
        }

        let entries: Vec<IndexEntry> = parsed
            .values()
            .map(|t| IndexEntry {
                uid: t.uid.clone(),
                path: t.uid.as_str().to_string(),
                revision: revision(t),
                first_seen_ms: now_ms,
            })
            .collect();
        // `apply_upsert`, never `apply`: `apply` replaces the project's
        // `entries` wholesale, and a delta describes a change, not the
        // store — handing it only this batch's uids would silently forget
        // every other uid's identity (an empty, no-op delta batch would wipe
        // the table outright).
        self.index.apply_upsert(&self.project, &entries)?;

        // Through `assemble_cache`, the same final uniqueness sweep the
        // snapshot path gets, rather than a second copy of it: a row that
        // would violate a `tasks` UNIQUE constraint is dropped and reported,
        // because "letting it through kills every command in the project, `ls`
        // included". Nothing is carried in from the previous cache — a delta
        // says what changed and the rest of the cache is left standing — so
        // the `complete` flag and the observed-uid set have nothing to gate
        // and are passed at their most conservative.
        let swept = self.assemble_cache(
            false,
            parsed,
            &std::collections::BTreeSet::new(),
            std::iter::empty(),
        )?;
        // Touch only what changed. `cache_tasks` replaces a whole project and
        // would need every task re-read from the backend to rebuild — one
        // query per cached task, which is worse than the single scan this
        // path exists to avoid.
        self.index.cache_upsert_tasks(&self.project, &swept)?;
        // A defensive backstop, not a load-bearing step for this backend
        // under ordinary single-consumer operation (its `pending*` tables
        // are only ever written by the snapshot path): if an earlier
        // snapshot-arm pass left a `pending_deletions` row for a uid this
        // delta just forgot in a way `forget` didn't already clean up, this
        // reaps it rather than leaving it to read as "already mid grace
        // period" forever.
        self.index.reap_orphans(&self.project, None)?;
        self.index.set_cursor(&self.project, &cursor)?;
        Ok(report)
    }

    /// Resolves everything that would violate a `tasks` uniqueness
    /// constraint, in `parsed`, before either cache-filling path can hand it
    /// to `cache_tasks`: one path per uid first, then one path per key.
    ///
    /// This is the single copy of that rule. `reconcile_with` and
    /// `refresh_cache` used to be two hand-maintained copies of it that
    /// disagreed — about the `complete` gate and about identity — and each
    /// disagreement was its own critical regression: a duplicate key killed
    /// every read, and a `cp -p` of any note made every mutating command
    /// exit 1 after its write had already landed.
    pub(crate) fn resolve_duplicates(
        &self,
        parsed: &mut std::collections::BTreeMap<String, Task>,
        prefix: &str,
    ) -> Result<DuplicateResolution, AppError> {
        // One live path per uid, chosen exactly as `resolve_identity`
        // chooses: the path the index already records for that uid keeps it
        // (that is the `Update` arm), and only when the index records
        // neither does the lexicographically smallest path win (the
        // `Adopt`-then-`Copy` arms, which classify in path order). Picking
        // differently here would make a bare refresh disagree with the next
        // reconcile about which file is the copy, and the two would fight
        // over the row on alternate commands.
        let recorded: std::collections::BTreeMap<TaskUid, String> = self
            .index
            .view(&self.project)?
            .entries
            .into_iter()
            .map(|e| (e.uid, e.path))
            .collect();
        let mut by_uid: std::collections::BTreeMap<&TaskUid, Vec<&String>> =
            std::collections::BTreeMap::new();
        for (path, task) in parsed.iter() {
            by_uid.entry(&task.uid).or_default().push(path);
        }
        let mut copies: Vec<String> = Vec::new();
        for (uid, paths) in by_uid {
            if paths.len() < 2 {
                continue;
            }
            let keeper = recorded
                .get(uid)
                .filter(|p| paths.contains(p))
                .cloned()
                .unwrap_or_else(|| paths[0].clone());
            for path in paths {
                if path != &keeper {
                    self.warn(format!(
                        "{path} carries the same identity as {keeper} — it stays out of the task \
                         list until `cadet adopt` gives it one of its own"
                    ));
                    copies.push(path.clone());
                }
            }
        }
        for path in copies {
            parsed.remove(&path);
        }

        // One task per key, over EVERY parsed task — a foreign prefix and the
        // `?-0` placeholder included. `cache_tasks` enforces uniqueness over
        // `(key_prefix, key_num)` for every row it stores, so resolution has
        // to be total over exactly that domain or it hands the index a
        // duplicate it was never taught to resolve.
        let high_water = self.index.high_water(&self.project)?;
        let mut out = DuplicateResolution::default();
        for r in resolve_collisions(collision_candidates(parsed), high_water) {
            let (Some(new_key), Some(from)) = (r.new_key, r.renumbered_from) else {
                out.settled.push(r.path);
                continue;
            };
            let Some(task) = parsed.get_mut(&r.path) else {
                continue;
            };
            // Always mint under this project's own prefix. `resolve_collisions`
            // keeps the group's prefix, which is right for the ordinary case
            // and wrong for a duplicate carried in from another vault: the
            // loser needs a key this project owns and allocates against its
            // own high-water mark, not a second claim on a namespace it does
            // not control.
            let new_key = TaskKey::new(prefix.to_string(), new_key.number);
            out.changes.push(KeyChange {
                path: r.path,
                to: new_key.clone(),
                on_disk: revision(task),
            });
            task.key = new_key;
            task.renumbered_from = Some(from);
        }
        Ok(out)
    }

    /// Writes the resolved keys back to the files that lost them. The key
    /// itself already landed in `parsed` — and therefore in the index — in
    /// `resolve_duplicates`; only the *file* write waits, because
    /// renumbering is one of the three situations in which reconcile mutates
    /// a user file (§5).
    ///
    /// Splitting it that way is the whole point: an incomplete snapshot is
    /// not a whole-scan view, so it must not move a key on disk away from the
    /// task that legitimately owns it — but the index still has to be
    /// duplicate-free, because `tasks_unique_key` refuses to store two live
    /// tasks under one key and `cadet show` has no answer for them either.
    /// Gating the whole rule on `complete` rather than just the write is what
    /// turned an unresolvable duplicate into a hard abort on every read.
    fn apply_renumbers(
        &self,
        now_ms: i64,
        complete: bool,
        resolved: &DuplicateResolution,
        parsed: &std::collections::BTreeMap<String, Task>,
        entries: &mut [IndexEntry],
        report: &mut ReconcileReport,
    ) -> Result<(), AppError> {
        let waiting = self.index.pending_renumbers(&self.project)?;
        for path in &resolved.settled {
            if waiting.contains_key(path) {
                self.index.clear_pending_renumber(&self.project, path)?;
            }
        }
        if resolved.changes.is_empty() {
            return Ok(());
        }
        for change in &resolved.changes {
            let Some(task) = parsed.get(&change.path) else {
                continue;
            };
            let ready = complete
                && waiting.get(&change.path).is_some_and(|(rev, since)| {
                    rev == &change.on_disk && now_ms - since >= GRACE_MS
                });
            if !ready {
                // Deliberately no `bump_high_water` here. The mark is what
                // `resolve_collisions` counts up from, so raising it for a key
                // that has not been written yet would hand the same file a
                // different key on every poll, and the index would show a key
                // that never settles.
                report.pending_renumber += 1;
                self.index.mark_pending_renumber(
                    &self.project,
                    &change.path,
                    &change.on_disk,
                    now_ms,
                )?;
                continue;
            }
            let written = self
                .backend
                .put(task.clone(), Some(change.on_disk.clone()))?;
            self.index
                .bump_high_water(&self.project, change.to.number)?;
            self.index
                .clear_pending_renumber(&self.project, &change.path)?;
            report.renumbered += 1;
            if let Some(e) = entries.iter_mut().find(|e| e.uid == task.uid) {
                e.revision = written;
            }
        }
        Ok(())
    }

    /// Everything that may enter `cache_tasks`, assembled the same way from
    /// both paths: the parsed tasks, plus the ones the scan didn't see but
    /// that are only pending deletion, with a final uniqueness sweep.
    ///
    /// `cache_tasks` keeps its hard `UNIQUE` constraints — they are what made
    /// this class of bug loud. This is where a row that would violate one is
    /// dropped and reported instead of aborting: `resolve_duplicates` is
    /// total over what the scan parsed, but a task carried in from the
    /// previous cache is not in that domain and can still collide. Losing one
    /// row from a display cache costs the user one line of output; letting it
    /// through kills every command in the project, `ls` included.
    pub(crate) fn assemble_cache<'a>(
        &self,
        complete: bool,
        parsed: std::collections::BTreeMap<String, Task>,
        observed_uids: &std::collections::BTreeSet<&str>,
        previously_cached: impl Iterator<Item = &'a TaskSummary>,
    ) -> Result<Vec<Task>, AppError> {
        let pending_deletions = self.index.view(&self.project)?.pending_deletions;
        let mut candidates: Vec<Task> = parsed.into_values().collect();
        candidates.extend(carry_absent_tasks(
            &pending_deletions,
            previously_cached,
            observed_uids,
            complete,
        ));

        let mut uids: std::collections::BTreeSet<TaskUid> = std::collections::BTreeSet::new();
        let mut keys: std::collections::BTreeSet<TaskKey> = std::collections::BTreeSet::new();
        let mut out = Vec::with_capacity(candidates.len());
        for task in candidates {
            if !uids.insert(task.uid.clone()) {
                self.warn(format!(
                    "two tasks share the identity {} — `{}` is hidden from the list until that is \
                     resolved",
                    task.uid.as_str(),
                    task.title
                ));
                continue;
            }
            if !keys.insert(task.key.clone()) {
                self.warn(format!(
                    "two tasks claim key {} — `{}` is hidden from the list until that is resolved",
                    task.key, task.title
                ));
                continue;
            }
            out.push(task);
        }
        Ok(out)
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
        duplicate_of: Option<&TaskUid>,
    ) -> Result<(), AppError> {
        let cfg = self.backend.load_project()?;
        let next = self.index.high_water(&self.project)? + 1;
        let uid = TaskUid::generate();
        let key = TaskKey::new(cfg.prefix.clone(), next);
        let now = jiff::Timestamp::from_millisecond(now_ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH);

        let mut task = self
            .backend
            .adopt(path.to_string(), uid.clone(), key, now)?;
        // `Backend::adopt` stamps uid, key and timestamps and nothing else,
        // so the breadcrumb needs its own write. Second `put` rather than a
        // wider `adopt` signature: `adopt` is a foreign-language trait
        // method (§3) and this is the only caller that has anything to
        // record.
        if let Some(source) = duplicate_of {
            task.possible_duplicate_of = Some(source.clone());
            self.backend.put(task.clone(), None)?;
        }

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

/// The §5 collision input: EVERY parsed task, whatever prefix its key
/// carries. Filtering to the project's own prefix left two classes the
/// resolver never saw — a key under a foreign prefix, and the `?-0`
/// placeholder a key that did not parse reads back as — while `cache_tasks`
/// still had to store both. Resolution has to be total over the same
/// `(key_prefix, key_num)` domain the index constrains, or ordinary user data
/// reaches a constraint nothing was taught to satisfy.
fn collision_candidates(parsed: &std::collections::BTreeMap<String, Task>) -> Vec<Candidate> {
    parsed
        .iter()
        .map(|(path, task)| Candidate {
            task: task.clone(),
            path: path.clone(),
        })
        .collect()
}

/// One file's key moving out of the way of a duplicate.
pub(crate) struct KeyChange {
    path: String,
    to: TaskKey,
    /// The file's revision as it is on disk, captured before the new key was
    /// written into the in-memory task. Both the grace-period comparison and
    /// `put`'s optimistic check are against the on-disk state, so it has to
    /// be taken before the rewrite, not after.
    on_disk: Revision,
}

#[derive(Default)]
pub(crate) struct DuplicateResolution {
    /// Paths that kept the key they came with. Any `pending_renumbers` row
    /// still held for one of them is obsolete.
    settled: Vec<String>,
    changes: Vec<KeyChange>,
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
/// `cache_tasks` persists uid/key/title/state/due/priority/tags/fields —
/// `tags` and `fields` are carried over from the summary so a task mid grace
/// period does not lose them on the next cache rebuild; `created`/`updated`/
/// `renumbered_from`/`possible_duplicate_of`/`body` are never persisted by
/// `cache_tasks` and stay placeholders.
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
        tags: s.tags.clone(),
        renumbered_from: None,
        possible_duplicate_of: None,
        fields: s.fields.clone(),
        body: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadet_backend_local_db::LocalDbBackend;
    use cadet_backend_markdown::MarkdownBackend;

    const CFG: &str = r#"
[project]
id = "p"
name = "P"
prefix = "P"
[workflow]
states = ["todo", "doing", "done"]
initial = "todo"
terminal = ["done"]
"#;

    fn local_db_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.toml"), CFG).unwrap();
        let backend = LocalDbBackend::open_in_memory(dir.path().join("p.toml")).unwrap();
        let index = SqliteIndex::open_in_memory().unwrap();
        (dir, App::new(Box::new(backend), index, None, "p".into()))
    }

    #[test]
    fn an_unchanged_markdown_snapshot_preserves_its_index_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.toml"), CFG).unwrap();
        let app = App::new(
            Box::new(MarkdownBackend::new(dir.path().to_path_buf())),
            SqliteIndex::open_in_memory().unwrap(),
            None,
            "p".into(),
        );
        app.add("stable").unwrap();
        app.reconcile(1_000).unwrap();
        let first_seen = app.index.view("p").unwrap().entries[0].first_seen_ms;

        app.reconcile(2_000).unwrap();

        assert_eq!(
            app.index.view("p").unwrap().entries[0].first_seen_ms,
            first_seen
        );
    }

    /// A task written by something other than `App` — the second writer the
    /// delta path exists for.
    fn foreign_task(number: u32, title: &str) -> Task {
        Task {
            uid: TaskUid::generate(),
            key: TaskKey::new("P", number),
            title: title.into(),
            state: "todo".into(),
            created: jiff::Timestamp::UNIX_EPOCH,
            updated: jiff::Timestamp::UNIX_EPOCH,
            due: None,
            priority: Priority::Normal,
            tags: vec![],
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: std::collections::BTreeMap::new(),
            body: String::new(),
        }
    }

    /// Review finding 1 (CRITICAL): `apply_delta` used to hand `self.index`
    /// only the delta's own upserts via `apply`, which — per its own doc
    /// comment — "replaces the project's entries wholesale". A one-task
    /// delta silently forgot every other cached task's identity, and an
    /// empty (no-op) delta forgot all of them. This has no clean public-API
    /// symptom for a delta-only backend (the snapshot-arm fallback that
    /// would surface it is itself unreachable under ordinary single-consumer
    /// operation, and reads are served from `tasks`, not `entries`), so this
    /// asserts directly on the index's own view — legitimate for an
    /// in-crate unit test, not a throwaway probe.
    #[test]
    fn a_one_task_delta_does_not_forget_another_tasks_identity() {
        let (_dir, app) = local_db_app();
        app.add("first").unwrap();
        let b = app.add("second").unwrap();
        app.reconcile(1_000).unwrap();
        assert_eq!(app.index.view("p").unwrap().entries.len(), 2);

        app.set_state(&b.key, "doing").unwrap();
        app.reconcile(2_000).unwrap();

        assert_eq!(
            app.index.view("p").unwrap().entries.len(),
            2,
            "a delta that only touches one task must not forget the other's identity"
        );

        // The sharper case: a delta with NOTHING to upsert (nothing changed)
        // still ran the old code's wholesale `apply([])`, wiping every uid.
        let r = app.reconcile(3_000).unwrap();
        assert_eq!(r.updated + r.adopted, 0, "nothing changed in this pass");
        assert_eq!(
            app.index.view("p").unwrap().entries.len(),
            2,
            "an empty delta must not wipe the project's entries"
        );
    }

    /// Review finding 1 (CRITICAL), the nineteenth instance of the signature
    /// defect. `apply_delta` minted a replacement key from `high_water + 1`
    /// while `resolve_collisions` — the codebase's other minting site — floors
    /// the same operation at `high_water.max(observed_max)` and has
    /// `renumbering_never_collides_with_a_key_already_in_the_batch` to say so.
    ///
    /// The cache holds `A: P-1` at `high_water = 1`, and one delta carries both
    /// `B` (duplicating `P-1`) and `C` (legitimately holding `P-2`, the number
    /// the unfloored counter is about to mint for `B`). Both arrive through
    /// `Backend::put`, the public trait — exactly the second writer this path
    /// exists for.
    #[test]
    fn a_delta_renumber_does_not_take_a_key_another_task_in_the_batch_holds() {
        let (_dir, app) = local_db_app();
        let a = app.add("A").unwrap();
        app.reconcile(1_000).unwrap();
        assert_eq!(a.key, TaskKey::new("P", 1));

        let b = foreign_task(1, "B duplicates A");
        let c = foreign_task(2, "C already holds P-2");
        app.backend.put(b.clone(), None).unwrap();
        app.backend.put(c.clone(), None).unwrap();

        app.reconcile(2_000).unwrap();

        let listed = app.list(true).unwrap();
        assert_eq!(listed.len(), 3, "all three tasks must be listed");
        let keys: std::collections::BTreeSet<String> =
            listed.iter().map(|s| s.key.to_string()).collect();
        assert_eq!(keys.len(), 3, "every listed task must hold its own key");
        let c_row = listed
            .iter()
            .find(|s| s.uid == c.uid.as_str())
            .expect("C must still be listed");
        assert_eq!(
            c_row.key,
            TaskKey::new("P", 2),
            "C never duplicated anything and must not be renumbered"
        );
    }

    /// Review finding 3. `clear_pending_deletion` is in the snapshot arm's
    /// `Update`/`Rename` outcomes with a comment giving the reason — "a task
    /// that vanishes, returns, then vanishes again is deleted immediately next
    /// scan" — and was missing from `apply_delta`. `reap_orphans` does not
    /// cover it: `apply_upsert` puts the uid back in `entries`, so the stale
    /// row has an owner and survives.
    ///
    /// A local-db project reaches the snapshot arm whenever its cursor is
    /// stale (or the index was rebuilt), which is where the record is written;
    /// the delta arm is where the task comes back. `mark_pending_deletion` is
    /// called directly here for the same reason `resolve_identity` cannot call
    /// `clear_pending_deletion` itself — arranging the two-arm sequence for
    /// real would test the arm-switching, not this.
    #[test]
    fn a_delta_upsert_clears_a_stale_pending_deletion_record() {
        let (_dir, app) = local_db_app();
        let a = app.add("A").unwrap();
        app.reconcile(1_000).unwrap();

        app.index.mark_pending_deletion("p", &a.uid, 1_000).unwrap();
        app.set_state(&a.key, "doing").unwrap();
        app.reconcile(2_000).unwrap();

        assert!(
            !app.index
                .view("p")
                .unwrap()
                .pending_deletions
                .contains_key(&a.uid),
            "a task the delta just upserted is present, so its deletion \
             grace-period record must be retired — leaving it makes the next \
             absence a same-scan deletion"
        );
    }

    /// Re-review finding 1, the twenty-first instance of the signature defect
    /// and the exact sibling of the one above. `resolve_duplicates` fills BOTH
    /// halves of a `DuplicateResolution`, and both arms call it — but only
    /// `apply_renumbers`, on the snapshot arm, clears the `pending_renumbers`
    /// row for a path that came back settled. `apply_delta` iterated
    /// `resolved.changes` and dropped `resolved.settled` on the floor, and a
    /// local-db project stops calling `apply_renumbers` at all the moment it
    /// has a cursor.
    ///
    /// `reap_orphans` cannot cover this: it returns early when `live_paths` is
    /// `None`, which is what the delta arm passes, and a settled task is live
    /// anyway so it would never be an orphan. The harm is the adoption side's
    /// verbatim: `cadet doctor` reads `pending: 1` forever, and because the
    /// grace check for that path is already long satisfied, the next duplicate
    /// to land there is renumbered with no grace period at all.
    #[test]
    fn a_delta_clears_the_pending_renumber_row_of_a_task_that_came_back_settled() {
        let (_dir, app) = local_db_app();
        let a = app.add("A").unwrap();
        app.reconcile(1_000).unwrap();

        // What the snapshot arm leaves behind for a duplicate still waiting
        // out §5's grace period. `apply_delta` keys `parsed` on the uid, so
        // that is the "path" `resolve_duplicates` reports as settled.
        app.index
            .mark_pending_renumber("p", a.uid.as_str(), &revision(&a), 1_000)
            .unwrap();
        assert_eq!(app.renumber_status().unwrap().pending, 1);

        app.set_state(&a.key, "doing").unwrap();
        app.reconcile(2_000).unwrap();

        assert_eq!(
            app.renumber_status().unwrap().pending,
            0,
            "a task the delta resolved with the key it already had is settled, \
             so its pending-renumber row must be retired — `cadet doctor` \
             reports it forever otherwise, and the next duplicate at that path \
             gets no grace period"
        );
    }
}
