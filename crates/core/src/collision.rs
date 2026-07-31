use crate::canonical::canonical_projection;
use crate::model::{Task, TaskKey};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub task: Task,
    /// Backend-relative path, `/`-separated and NFC-normalised by the caller.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub path: String,
    /// `None` means the candidate keeps the key it already had.
    pub new_key: Option<TaskKey>,
    pub renumbered_from: Option<TaskKey>,
}

/// Order-independent resolution of duplicate keys across a complete scan.
/// `high_water` is the highest key number ever allocated for this project.
/// The counter's floor is the greater of `high_water` and the maximum key
/// number present in `candidates`, so renumbering can never collide with a
/// key already visible in this batch. Spec §5.
pub fn resolve_collisions(candidates: Vec<Candidate>, high_water: u32) -> Vec<Resolution> {
    let observed_max = candidates
        .iter()
        .map(|c| c.task.key.number)
        .max()
        .unwrap_or(0);
    let mut next = high_water.max(observed_max);

    let mut groups: BTreeMap<TaskKey, Vec<Candidate>> = BTreeMap::new();
    for c in candidates {
        groups.entry(c.task.key.clone()).or_default().push(c);
    }

    let mut out = Vec::new();

    for (key, mut group) in groups {
        if group.len() == 1 {
            out.push(Resolution {
                path: group.remove(0).path,
                new_key: None,
                renumbered_from: None,
            });
            continue;
        }
        group.sort_by(|a, b| {
            a.task
                .created
                .cmp(&b.task.created)
                .then_with(|| canonical_projection(&a.task).cmp(&canonical_projection(&b.task)))
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut it = group.into_iter();
        if let Some(keeper) = it.next() {
            out.push(Resolution {
                path: keeper.path,
                new_key: None,
                renumbered_from: None,
            });
        }
        for loser in it {
            next += 1;
            out.push(Resolution {
                path: loser.path,
                new_key: Some(TaskKey::new(key.prefix.clone(), next)),
                renumbered_from: Some(key.clone()),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use std::collections::BTreeMap;

    fn cand(key: u32, created_s: i64, path: &str, body: &str) -> Candidate {
        Candidate {
            task: Task {
                uid: TaskUid::generate(),
                key: TaskKey::new("P", key),
                title: "t".into(),
                state: "todo".into(),
                created: jiff::Timestamp::from_second(created_s).unwrap(),
                updated: jiff::Timestamp::UNIX_EPOCH,
                due: None,
                priority: Priority::Normal,
                tags: vec![],
                renumbered_from: None,
                possible_duplicate_of: None,
                fields: BTreeMap::new(),
                body: body.into(),
            },
            path: path.into(),
        }
    }

    #[test]
    fn no_collision_leaves_keys_untouched() {
        let out = resolve_collisions(vec![cand(1, 10, "a.md", "x"), cand(2, 20, "b.md", "y")], 2);
        assert!(out.iter().all(|r| r.new_key.is_none()));
    }

    #[test]
    fn earlier_created_keeps_the_key() {
        let out = resolve_collisions(
            vec![cand(4, 200, "b.md", "y"), cand(4, 100, "a.md", "x")],
            4,
        );
        let keeper = out.iter().find(|r| r.path == "a.md").unwrap();
        let loser = out.iter().find(|r| r.path == "b.md").unwrap();
        assert!(keeper.new_key.is_none());
        assert_eq!(loser.new_key, Some(TaskKey::new("P", 5)));
        assert_eq!(loser.renumbered_from, Some(TaskKey::new("P", 4)));
    }

    #[test]
    fn result_is_independent_of_input_order() {
        let a = cand(4, 100, "a.md", "x");
        let b = cand(4, 200, "b.md", "y");
        let c = cand(4, 300, "c.md", "z");
        let fwd = resolve_collisions(vec![a.clone(), b.clone(), c.clone()], 4);
        let rev = resolve_collisions(vec![c, b, a], 4);
        let norm = |mut v: Vec<Resolution>| {
            v.sort_by(|x, y| x.path.cmp(&y.path));
            v.into_iter()
                .map(|r| (r.path, r.new_key))
                .collect::<Vec<_>>()
        };
        assert_eq!(norm(fwd), norm(rev));
    }

    #[test]
    fn identical_timestamps_are_broken_by_content_then_path() {
        // Same created, same content: only `path` can decide. This models `cp -p`,
        // which produces byte-identical files — so both candidates share one uid,
        // not `cand()`'s freshly generated one, since `canonical_projection`
        // includes `uid` and two random uids would settle the tie before `path`
        // ever gets a say.
        let uid = TaskUid::generate();
        let mut z = cand(4, 100, "z.md", "same");
        z.task.uid = uid.clone();
        let mut a = cand(4, 100, "a.md", "same");
        a.task.uid = uid;
        let out = resolve_collisions(vec![z, a], 4);
        let keeper = out.iter().find(|r| r.new_key.is_none()).unwrap();
        assert_eq!(keeper.path, "a.md");
    }

    #[test]
    fn renumbering_starts_above_the_high_water_mark() {
        let out = resolve_collisions(
            vec![cand(4, 100, "a.md", "x"), cand(4, 200, "b.md", "y")],
            40,
        );
        let loser = out.iter().find(|r| r.path == "b.md").unwrap();
        assert_eq!(loser.new_key, Some(TaskKey::new("P", 41)));
    }

    #[test]
    fn result_is_independent_of_input_order_across_multiple_groups() {
        let a = cand(4, 100, "a.md", "x");
        let b = cand(4, 200, "b.md", "y");
        let c = cand(4, 300, "c.md", "z");
        let d = cand(9, 100, "d.md", "p");
        let e = cand(9, 200, "e.md", "q");

        let norm = |mut v: Vec<Resolution>| {
            v.sort_by(|x, y| x.path.cmp(&y.path));
            v.into_iter()
                .map(|r| (r.path, r.new_key))
                .collect::<Vec<_>>()
        };

        let order1 = resolve_collisions(
            vec![a.clone(), b.clone(), c.clone(), d.clone(), e.clone()],
            4,
        );
        let order2 = resolve_collisions(
            vec![e.clone(), d.clone(), c.clone(), b.clone(), a.clone()],
            4,
        );
        // Interleaved: alternates between the two groups.
        let order3 = resolve_collisions(vec![d, a, e, b, c], 4);

        let n1 = norm(order1);
        assert_eq!(n1, norm(order2));
        assert_eq!(n1, norm(order3));
    }

    #[test]
    fn losers_are_renumbered_in_sorted_order() {
        let out = resolve_collisions(
            vec![
                cand(4, 300, "c.md", "z"),
                cand(4, 100, "a.md", "x"),
                cand(4, 200, "b.md", "y"),
            ],
            4,
        );
        let b = out.iter().find(|r| r.path == "b.md").unwrap();
        let c = out.iter().find(|r| r.path == "c.md").unwrap();
        assert_eq!(b.new_key, Some(TaskKey::new("P", 5)));
        assert_eq!(c.new_key, Some(TaskKey::new("P", 6)));
    }

    #[test]
    fn empty_input_and_single_candidate_are_no_ops() {
        let out = resolve_collisions(vec![], 5);
        assert!(out.is_empty());

        let out = resolve_collisions(vec![cand(1, 10, "a.md", "x")], 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].new_key, None);
    }

    #[test]
    fn renumbering_never_collides_with_a_key_already_in_the_batch() {
        let out = resolve_collisions(
            vec![
                cand(4, 200, "b.md", "y"),
                cand(4, 100, "a.md", "x"),
                cand(5, 50, "live.md", "w"),
            ],
            4,
        );
        let loser = out.iter().find(|r| r.path == "b.md").unwrap();
        assert_ne!(loser.new_key, Some(TaskKey::new("P", 5)));
        let live = out.iter().find(|r| r.path == "live.md").unwrap();
        assert_eq!(live.new_key, None);
    }
}
