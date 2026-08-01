use crate::canonical::Revision;
use crate::model::TaskUid;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub uid: Option<TaskUid>,
    pub path: String,
    pub revision: Revision,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub complete: bool,
    pub observed: Vec<Observed>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    pub uid: TaskUid,
    pub path: String,
    pub revision: Revision,
    pub first_seen_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct IndexView {
    pub entries: Vec<IndexEntry>,
    /// path -> (revision when first seen, timestamp ms).
    ///
    /// Contract: the caller MUST clear a path's entry here whenever an
    /// `Outcome::Adopt` claims it. If the caller does not, and the path is
    /// later deleted and recreated — paths, unlike uids, are reused — the
    /// newcomer inherits the stale first-seen timestamp, trivially satisfies
    /// the grace-period check, and is adopted immediately with no grace
    /// period at all, defeating the "never mutate on first observation" rule
    /// that exists to stop a rename delivered mid-sync from being written to.
    pub pending: BTreeMap<String, (Revision, i64)>,
    /// uid -> timestamp ms when absence was first observed.
    ///
    /// Contract: the caller MUST clear a uid's entry here whenever an
    /// `Outcome` reclaims it — i.e. on `Update` or `Rename` for that uid.
    /// If the caller does not, a task that vanishes, reappears, and then
    /// vanishes again is deleted immediately on the second disappearance,
    /// with no grace period, because the stale first-absence timestamp is
    /// still on record and already satisfies the grace-period check.
    pub pending_deletions: BTreeMap<TaskUid, i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct ScanClock {
    pub now_ms: i64,
    pub grace_ms: i64,
    /// True only for the single reconcile pass that immediately follows an
    /// explicit `undo`: a `git reset --hard` makes an absence intentional,
    /// not a possible sync artefact, so a uid observed absent for the FIRST
    /// time in this pass — no `pending_deletions` record yet — is deleted
    /// at once instead of entering the normal grace period. A uid that
    /// already had a `pending_deletions` record before this pass (mid grace
    /// period for a reason unrelated to the undo) is left exactly as it
    /// was: this flag must never shorten or reset an unrelated task's
    /// countdown.
    pub immediate_deletion: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    Incomplete,
    SuspectedIncompleteScan,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    PendingAdoption {
        path: String,
    },
    Adopt {
        path: String,
    },
    Update {
        uid: TaskUid,
        path: String,
    },
    Rename {
        uid: TaskUid,
        to: String,
    },
    Copy {
        source: TaskUid,
        path: String,
    },
    PendingDeletion {
        uid: TaskUid,
    },
    Delete {
        uid: TaskUid,
    },
    /// Rejects the whole scan: the caller must discard every other `Outcome`
    /// returned alongside this one (this is always the sole element of the
    /// vec), including unrelated `Update`s for tasks that did not change.
    /// A `Rename` or `Delete` conclusion depends on an absence being real;
    /// under an incomplete snapshot, or a drop too large to trust, absence
    /// is not reliable evidence, so applying only some of the computed
    /// outcomes is more dangerous than applying none of them. Reads are
    /// served from the index and are unaffected — the only cost is that
    /// legitimate edits are picked up on the next complete, trusted scan.
    ScanRejected {
        reason: RejectReason,
    },
}

const DROP_FRACTION: f64 = 0.10;
const DROP_ABSOLUTE: usize = 5;

/// Applies the §5 resolution table to one scan.
///
/// Two guarantees: it is pure — all time enters via `clock`, never a clock
/// read — and its result is independent of the order of `snap.observed`.
/// When one uid appears at multiple live paths in the same scan, the
/// lexicographically smallest path wins the `Update`/`Rename` slot and every
/// other path becomes a `Copy`, regardless of the order the scanner happened
/// to walk the directory in (which is `readdir` order — unstable across
/// filesystems and across scans of the same directory).
pub fn resolve_identity(snap: &Snapshot, idx: &IndexView, clock: ScanClock) -> Vec<Outcome> {
    let known = idx.entries.len();
    // Distinct known uids covered by the snapshot, not observed rows — a
    // duplicated path (an ordinary copy) must not be able to mask real
    // absences by inflating the count of "things we saw".
    let known_uids: BTreeSet<&TaskUid> = idx.entries.iter().map(|e| &e.uid).collect();
    let covered: BTreeSet<&TaskUid> = snap
        .observed
        .iter()
        .filter_map(|o| o.uid.as_ref())
        .filter(|u| known_uids.contains(u))
        .collect();
    let observed_known = covered.len();

    // Guard 2: a proportionally large *and* absolutely significant drop is not evidence.
    if known > 0 && observed_known < known {
        let dropped = known - observed_known;
        if dropped > DROP_ABSOLUTE && (dropped as f64 / known as f64) > DROP_FRACTION {
            return vec![Outcome::ScanRejected {
                reason: RejectReason::SuspectedIncompleteScan,
            }];
        }
    }

    let mut out = Vec::new();
    let live_paths: BTreeSet<&str> = snap.observed.iter().map(|o| o.path.as_str()).collect();
    let mut claimed: BTreeSet<TaskUid> = BTreeSet::new();

    // Classify in path order, not scan (readdir) order, so that when one uid
    // appears at multiple live paths the same path always wins the
    // Update/Rename slot on every machine, regardless of directory walk
    // order. This also makes the emitted `Outcome` order deterministic.
    let mut ordered: Vec<&Observed> = snap.observed.iter().collect();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));

    for o in ordered {
        match &o.uid {
            None => {
                let ready = idx
                    .pending
                    .get(&o.path)
                    .is_some_and(|(r, t)| r == &o.revision && clock.now_ms - t >= clock.grace_ms);
                out.push(if ready {
                    Outcome::Adopt {
                        path: o.path.clone(),
                    }
                } else {
                    Outcome::PendingAdoption {
                        path: o.path.clone(),
                    }
                });
            }
            Some(u) => match idx.entries.iter().find(|e| &e.uid == u) {
                None => out.push(Outcome::Adopt {
                    path: o.path.clone(),
                }),
                // A uid already claimed by an earlier row in this same scan
                // cannot be claimed again: two live paths can't both be the
                // one task that uid identifies, so every occurrence after
                // the first is a Copy.
                Some(_) if claimed.contains(u) => {
                    out.push(Outcome::Copy {
                        source: u.clone(),
                        path: o.path.clone(),
                    });
                }
                Some(entry) if entry.path == o.path => {
                    claimed.insert(u.clone());
                    out.push(Outcome::Update {
                        uid: u.clone(),
                        path: o.path.clone(),
                    });
                }
                Some(entry) if live_paths.contains(entry.path.as_str()) => {
                    out.push(Outcome::Copy {
                        source: u.clone(),
                        path: o.path.clone(),
                    });
                }
                Some(_) => {
                    claimed.insert(u.clone());
                    out.push(Outcome::Rename {
                        uid: u.clone(),
                        to: o.path.clone(),
                    });
                }
            },
        }
    }

    // Guard 1: deletion is only inferable from a complete snapshot.
    for e in &idx.entries {
        // Redundant with `claimed`: every claimed uid necessarily came from a
        // row in `snap.observed`, so this single check covers both claimed
        // and copied-but-unclaimed occurrences.
        if snap.observed.iter().any(|o| o.uid.as_ref() == Some(&e.uid)) {
            continue;
        }
        if !snap.complete {
            // Wholesale rejection: every other `Outcome` already pushed into
            // `out` for this call — including unrelated, legitimate
            // `Update`s — is discarded, not just the ones touching this
            // uid. See the doc comment on `Outcome::ScanRejected`.
            return vec![Outcome::ScanRejected {
                reason: RejectReason::Incomplete,
            }];
        }
        // Guard 3: absence must persist across the grace period — except a
        // fresh (never-before-seen) absence during the post-undo pass,
        // which is confirmed on the spot. See `ScanClock::immediate_deletion`.
        let confirmed = if clock.immediate_deletion && !idx.pending_deletions.contains_key(&e.uid) {
            true
        } else {
            idx.pending_deletions
                .get(&e.uid)
                .is_some_and(|t| clock.now_ms - t >= clock.grace_ms)
        };
        out.push(if confirmed {
            Outcome::Delete { uid: e.uid.clone() }
        } else {
            Outcome::PendingDeletion { uid: e.uid.clone() }
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::Revision;
    use crate::model::TaskUid;

    fn uid(n: u8) -> TaskUid {
        TaskUid::parse(&format!("01ARZ3NDEKTSV4RRFFQ69G5F{:02}", n)).unwrap()
    }
    fn rev(s: &str) -> Revision {
        Revision::from_raw(s)
    }
    fn seen(u: TaskUid, path: &str) -> IndexEntry {
        IndexEntry {
            uid: u,
            path: path.into(),
            revision: rev("r1"),
            first_seen_ms: 0,
        }
    }
    fn clock(now_ms: i64) -> ScanClock {
        ScanClock {
            now_ms,
            grace_ms: 60_000,
            immediate_deletion: false,
        }
    }

    fn undo_clock(now_ms: i64) -> ScanClock {
        ScanClock {
            immediate_deletion: true,
            ..clock(now_ms)
        }
    }

    #[test]
    fn missing_uid_is_pending_adoption_not_an_immediate_write() {
        let snap = Snapshot {
            complete: true,
            observed: vec![Observed {
                uid: None,
                path: "new.md".into(),
                revision: rev("r1"),
            }],
        };
        let out = resolve_identity(&snap, &IndexView::default(), clock(0));
        assert_eq!(
            out,
            vec![Outcome::PendingAdoption {
                path: "new.md".into()
            }]
        );
    }

    #[test]
    fn pending_adoption_becomes_adopt_after_the_grace_period() {
        let snap = Snapshot {
            complete: true,
            observed: vec![Observed {
                uid: None,
                path: "new.md".into(),
                revision: rev("r1"),
            }],
        };
        let mut idx = IndexView::default();
        idx.pending.insert("new.md".into(), (rev("r1"), 0));
        let out = resolve_identity(&snap, &idx, clock(60_001));
        assert_eq!(
            out,
            vec![Outcome::Adopt {
                path: "new.md".into()
            }]
        );
    }

    #[test]
    fn changed_content_restarts_the_grace_period() {
        let snap = Snapshot {
            complete: true,
            observed: vec![Observed {
                uid: None,
                path: "new.md".into(),
                revision: rev("r2"),
            }],
        };
        let mut idx = IndexView::default();
        idx.pending.insert("new.md".into(), (rev("r1"), 0));
        let out = resolve_identity(&snap, &idx, clock(60_001));
        assert_eq!(
            out,
            vec![Outcome::PendingAdoption {
                path: "new.md".into()
            }]
        );
    }

    #[test]
    fn known_uid_at_same_path_is_an_update() {
        let snap = Snapshot {
            complete: true,
            observed: vec![Observed {
                uid: Some(uid(1)),
                path: "a.md".into(),
                revision: rev("r2"),
            }],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        let out = resolve_identity(&snap, &idx, clock(0));
        assert_eq!(
            out,
            vec![Outcome::Update {
                uid: uid(1),
                path: "a.md".into()
            }]
        );
    }

    #[test]
    fn known_uid_at_new_path_with_old_path_gone_is_a_rename() {
        let snap = Snapshot {
            complete: true,
            observed: vec![Observed {
                uid: Some(uid(1)),
                path: "b.md".into(),
                revision: rev("r1"),
            }],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        let out = resolve_identity(&snap, &idx, clock(0));
        assert_eq!(
            out,
            vec![Outcome::Rename {
                uid: uid(1),
                to: "b.md".into()
            }]
        );
    }

    #[test]
    fn same_uid_at_two_live_paths_is_a_copy() {
        let snap = Snapshot {
            complete: true,
            observed: vec![
                Observed {
                    uid: Some(uid(1)),
                    path: "a.md".into(),
                    revision: rev("r1"),
                },
                Observed {
                    uid: Some(uid(1)),
                    path: "b.md".into(),
                    revision: rev("r1"),
                },
            ],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        let out = resolve_identity(&snap, &idx, clock(0));
        assert!(out.contains(&Outcome::Update {
            uid: uid(1),
            path: "a.md".into()
        }));
        assert!(out.contains(&Outcome::Copy {
            source: uid(1),
            path: "b.md".into()
        }));
    }

    #[test]
    fn one_uid_at_two_new_paths_is_a_rename_plus_a_copy() {
        // Both destinations are new; the old path is gone entirely. This
        // must not become two `Rename`s for the same uid.
        let snap = Snapshot {
            complete: true,
            observed: vec![
                Observed {
                    uid: Some(uid(1)),
                    path: "b.md".into(),
                    revision: rev("r1"),
                },
                Observed {
                    uid: Some(uid(1)),
                    path: "c.md".into(),
                    revision: rev("r1"),
                },
            ],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        let out = resolve_identity(&snap, &idx, clock(0));
        assert_eq!(
            out.iter()
                .filter(|o| matches!(o, Outcome::Rename { .. }))
                .count(),
            1
        );
        assert_eq!(
            out.iter()
                .filter(|o| matches!(o, Outcome::Copy { .. }))
                .count(),
            1
        );
        // Pin *which* path wins: the lexicographically smaller one, not
        // whichever happened to be observed first.
        assert!(out.contains(&Outcome::Rename {
            uid: uid(1),
            to: "b.md".into()
        }));
        assert!(out.contains(&Outcome::Copy {
            source: uid(1),
            path: "c.md".into()
        }));
    }

    #[test]
    fn rename_destination_is_independent_of_observed_order() {
        // Same scenario as `one_uid_at_two_new_paths_is_a_rename_plus_a_copy`,
        // run with the two observations in both orders. Two machines walking
        // the same directory in different (readdir) orders must compute the
        // same verdict: b.md always wins the Rename slot.
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));

        let forward = Snapshot {
            complete: true,
            observed: vec![
                Observed {
                    uid: Some(uid(1)),
                    path: "b.md".into(),
                    revision: rev("r1"),
                },
                Observed {
                    uid: Some(uid(1)),
                    path: "c.md".into(),
                    revision: rev("r1"),
                },
            ],
        };
        let reversed = Snapshot {
            complete: true,
            observed: vec![
                Observed {
                    uid: Some(uid(1)),
                    path: "c.md".into(),
                    revision: rev("r1"),
                },
                Observed {
                    uid: Some(uid(1)),
                    path: "b.md".into(),
                    revision: rev("r1"),
                },
            ],
        };

        let mut out_forward = resolve_identity(&forward, &idx, clock(0));
        let mut out_reversed = resolve_identity(&reversed, &idx, clock(0));
        out_forward.sort_by_key(|o| format!("{o:?}"));
        out_reversed.sort_by_key(|o| format!("{o:?}"));

        assert_eq!(out_forward, out_reversed);
        assert!(out_forward.contains(&Outcome::Rename {
            uid: uid(1),
            to: "b.md".into()
        }));
    }

    #[test]
    fn absent_uid_in_a_complete_snapshot_is_pending_deletion_first() {
        let snap = Snapshot {
            complete: true,
            observed: vec![],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        let out = resolve_identity(&snap, &idx, clock(0));
        assert_eq!(out, vec![Outcome::PendingDeletion { uid: uid(1) }]);
    }

    #[test]
    fn deletion_confirms_only_after_the_grace_period() {
        let snap = Snapshot {
            complete: true,
            observed: vec![],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        idx.pending_deletions.insert(uid(1), 0);
        let out = resolve_identity(&snap, &idx, clock(60_001));
        assert_eq!(out, vec![Outcome::Delete { uid: uid(1) }]);
    }

    #[test]
    fn immediate_deletion_deletes_a_freshly_absent_uid_at_once() {
        let snap = Snapshot {
            complete: true,
            observed: vec![],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        // No `pending_deletions` record yet — this is the first time the
        // uid is observed absent.
        let out = resolve_identity(&snap, &idx, undo_clock(0));
        assert_eq!(out, vec![Outcome::Delete { uid: uid(1) }]);
    }

    #[test]
    fn immediate_deletion_does_not_disturb_an_already_pending_uid() {
        let snap = Snapshot {
            complete: true,
            observed: vec![],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        // Already mid grace period for a reason unrelated to this pass.
        idx.pending_deletions.insert(uid(1), 0);
        let out = resolve_identity(&snap, &idx, undo_clock(1_000));
        assert_eq!(
            out,
            vec![Outcome::PendingDeletion { uid: uid(1) }],
            "an existing pending-deletion record must not be short-circuited by an unrelated undo"
        );
    }

    #[test]
    fn incomplete_snapshot_never_deletes() {
        let snap = Snapshot {
            complete: false,
            observed: vec![],
        };
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        idx.pending_deletions.insert(uid(1), 0);
        let out = resolve_identity(&snap, &idx, clock(60_001));
        assert_eq!(
            out,
            vec![Outcome::ScanRejected {
                reason: RejectReason::Incomplete
            }]
        );
    }

    #[test]
    fn incomplete_snapshot_discards_even_valid_updates() {
        // uid(1) is present with a changed revision — an ordinary, valid
        // Update on its own. uid(2) is absent from an incomplete snapshot.
        // Rejection must be wholesale: the valid Update is discarded too.
        let mut idx = IndexView::default();
        idx.entries.push(seen(uid(1), "a.md"));
        idx.entries.push(seen(uid(2), "b.md"));
        let snap = Snapshot {
            complete: false,
            observed: vec![Observed {
                uid: Some(uid(1)),
                path: "a.md".into(),
                revision: rev("r2"),
            }],
        };
        let out = resolve_identity(&snap, &idx, clock(0));
        assert_eq!(
            out,
            vec![Outcome::ScanRejected {
                reason: RejectReason::Incomplete
            }]
        );
        assert!(!out.iter().any(|o| matches!(o, Outcome::Update { .. })));
    }

    #[test]
    fn large_proportional_drop_rejects_the_whole_scan() {
        // 40 known, 4 observed: 90% drop, well over both thresholds.
        let mut idx = IndexView::default();
        for n in 0..40 {
            idx.entries.push(seen(uid(n), &format!("t{n}.md")));
        }
        let snap = Snapshot {
            complete: true,
            observed: (0..4)
                .map(|n| Observed {
                    uid: Some(uid(n)),
                    path: format!("t{n}.md"),
                    revision: rev("r1"),
                })
                .collect(),
        };
        let out = resolve_identity(&snap, &idx, clock(0));
        assert_eq!(
            out,
            vec![Outcome::ScanRejected {
                reason: RejectReason::SuspectedIncompleteScan
            }]
        );
    }

    #[test]
    fn duplicate_paths_do_not_mask_a_mass_deletion() {
        // 40 known, only 7 genuinely survive (33 gone — an 82.5% real drop).
        // One survivor, uid(0), is additionally duplicated to 28 extra live
        // paths — ordinary copying, not an attack. The guard must count
        // distinct known uids covered by the snapshot, not observed rows:
        // 35 rows cover only 7 distinct known uids.
        let mut idx = IndexView::default();
        for n in 0..40 {
            idx.entries.push(seen(uid(n), &format!("t{n}.md")));
        }
        let mut observed: Vec<Observed> = (0..7)
            .map(|n| Observed {
                uid: Some(uid(n)),
                path: format!("t{n}.md"),
                revision: rev("r1"),
            })
            .collect();
        observed.extend((0..28).map(|n| Observed {
            uid: Some(uid(0)),
            path: format!("dup{n}.md"),
            revision: rev("r1"),
        }));
        let snap = Snapshot {
            complete: true,
            observed,
        };
        let out = resolve_identity(&snap, &idx, clock(0));
        assert_eq!(
            out,
            vec![Outcome::ScanRejected {
                reason: RejectReason::SuspectedIncompleteScan
            }]
        );
    }

    #[test]
    fn small_deletions_always_pass_the_threshold() {
        // 40 known, 37 observed: 7.5% and 3 tasks — under both limits.
        let mut idx = IndexView::default();
        for n in 0..40 {
            idx.entries.push(seen(uid(n), &format!("t{n}.md")));
        }
        let snap = Snapshot {
            complete: true,
            observed: (0..37)
                .map(|n| Observed {
                    uid: Some(uid(n)),
                    path: format!("t{n}.md"),
                    revision: rev("r1"),
                })
                .collect(),
        };
        let out = resolve_identity(&snap, &idx, clock(0));
        assert_eq!(
            out.iter()
                .filter(|o| matches!(o, Outcome::PendingDeletion { .. }))
                .count(),
            3
        );
    }
}
