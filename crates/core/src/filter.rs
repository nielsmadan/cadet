use crate::model::{FieldValue, Priority};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskFilter {
    pub states: Vec<String>,
    pub tags: Vec<String>,
    pub priority: Option<Priority>,
    pub due_before: Option<String>,
    pub due_after: Option<String>,
    pub fields: Vec<(String, FieldValue)>,
}

/// A symbolic answer to "when is it due", resolved against a calendar day the
/// caller supplies. Produces the same `due_before`/`due_after` bounds a user
/// could have typed, so there is one comparison rule rather than two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueBucket {
    Today,
    Week,
    Overdue,
}

impl DueBucket {
    /// `(due_after, due_before)`, both exclusive, matching `TaskFilter`.
    /// `today` is due on exactly this day, `overdue` is strictly before it,
    /// and `week` is a rolling today-through-six-days-out window.
    pub fn bounds(
        self,
        today: jiff::civil::Date,
    ) -> Result<(Option<String>, Option<String>), jiff::Error> {
        let day = |d: jiff::civil::Date| d.to_string();
        Ok(match self {
            DueBucket::Today => (Some(day(today.yesterday()?)), Some(day(today.tomorrow()?))),
            DueBucket::Week => (
                Some(day(today.yesterday()?)),
                Some(day(today.checked_add(jiff::Span::new().days(7))?)),
            ),
            DueBucket::Overdue => (None, Some(day(today))),
        })
    }
}

pub struct FilterTarget<'a> {
    pub state: &'a str,
    pub due: Option<&'a str>,
    pub priority: Priority,
    pub tags: &'a [String],
    pub fields: &'a BTreeMap<String, FieldValue>,
}

impl TaskFilter {
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
            && self.tags.is_empty()
            && self.priority.is_none()
            && self.due_before.is_none()
            && self.due_after.is_none()
            && self.fields.is_empty()
    }

    pub fn matches(&self, m: &FilterTarget<'_>) -> bool {
        if !self.states.is_empty() && !self.states.iter().any(|s| s == m.state) {
            return false;
        }
        // Every requested tag must be present. Repeating `--tag` narrows.
        if !self
            .tags
            .iter()
            .all(|want| m.tags.iter().any(|t| t == want))
        {
            return false;
        }
        if let Some(p) = self.priority
            && p != m.priority
        {
            return false;
        }
        // ISO-8601 dates sort lexicographically in date order, so a string
        // comparison is a date comparison. Undated tasks match neither bound.
        if let Some(b) = &self.due_before {
            match m.due {
                Some(d) if d < b.as_str() => {}
                _ => return false,
            }
        }
        if let Some(a) = &self.due_after {
            match m.due {
                Some(d) if d > a.as_str() => {}
                _ => return false,
            }
        }
        for (name, want) in &self.fields {
            if m.fields.get(name) != Some(want) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn target<'a>(
        state: &'a str,
        due: Option<&'a str>,
        tags: &'a [String],
        fields: &'a BTreeMap<String, FieldValue>,
    ) -> FilterTarget<'a> {
        FilterTarget {
            state,
            due,
            priority: Priority::Normal,
            tags,
            fields,
        }
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        let f = TaskFilter::default();
        let tags: Vec<String> = vec![];
        let fields = BTreeMap::new();
        assert!(f.is_empty());
        assert!(f.matches(&target("todo", None, &tags, &fields)));
    }

    #[test]
    fn states_are_or_ed() {
        let f = TaskFilter {
            states: vec!["doing".into(), "blocked".into()],
            ..Default::default()
        };
        let tags: Vec<String> = vec![];
        let fields = BTreeMap::new();
        assert!(f.matches(&target("doing", None, &tags, &fields)));
        assert!(f.matches(&target("blocked", None, &tags, &fields)));
        assert!(!f.matches(&target("todo", None, &tags, &fields)));
    }

    #[test]
    fn tags_are_and_ed() {
        let f = TaskFilter {
            tags: vec!["home".into(), "urgent".into()],
            ..Default::default()
        };
        let fields = BTreeMap::new();
        let both = vec![
            "urgent".to_string(),
            "home".to_string(),
            "extra".to_string(),
        ];
        let one = vec!["home".to_string()];
        assert!(f.matches(&target("todo", None, &both, &fields)));
        assert!(!f.matches(&target("todo", None, &one, &fields)));
    }

    #[test]
    fn due_bounds_are_exclusive_and_skip_undated_tasks() {
        let tags: Vec<String> = vec![];
        let fields = BTreeMap::new();
        let before = TaskFilter {
            due_before: Some("2026-08-10".into()),
            ..Default::default()
        };
        assert!(before.matches(&target("todo", Some("2026-08-09"), &tags, &fields)));
        assert!(!before.matches(&target("todo", Some("2026-08-10"), &tags, &fields)));
        assert!(!before.matches(&target("todo", None, &tags, &fields)));

        let after = TaskFilter {
            due_after: Some("2026-08-10".into()),
            ..Default::default()
        };
        assert!(after.matches(&target("todo", Some("2026-08-11"), &tags, &fields)));
        assert!(!after.matches(&target("todo", Some("2026-08-10"), &tags, &fields)));
        assert!(!after.matches(&target("todo", None, &tags, &fields)));
    }

    #[test]
    fn custom_fields_must_match_exactly() {
        let mut fields = BTreeMap::new();
        fields.insert("estimate".to_string(), FieldValue::Int(3));
        let tags: Vec<String> = vec![];
        let hit = TaskFilter {
            fields: vec![("estimate".into(), FieldValue::Int(3))],
            ..Default::default()
        };
        let miss = TaskFilter {
            fields: vec![("estimate".into(), FieldValue::Int(5))],
            ..Default::default()
        };
        let absent = TaskFilter {
            fields: vec![("other".into(), FieldValue::Int(3))],
            ..Default::default()
        };
        assert!(hit.matches(&target("todo", None, &tags, &fields)));
        assert!(!miss.matches(&target("todo", None, &tags, &fields)));
        assert!(!absent.matches(&target("todo", None, &tags, &fields)));
    }

    #[test]
    fn criteria_of_different_kinds_are_and_ed() {
        let tags_v = vec!["home".to_string()];
        let fields = BTreeMap::new();
        let f = TaskFilter {
            states: vec!["todo".into()],
            tags: vec!["home".into()],
            ..Default::default()
        };
        assert!(f.matches(&target("todo", None, &tags_v, &fields)));
        assert!(!f.matches(&target("doing", None, &tags_v, &fields)));
    }
}

#[cfg(test)]
mod due_bucket_tests {
    use super::*;

    fn d(s: &str) -> jiff::civil::Date {
        s.parse().unwrap()
    }

    fn matches(bucket: DueBucket, today: &str, due: &str) -> bool {
        let (after, before) = bucket.bounds(d(today)).unwrap();
        let f = TaskFilter {
            due_after: after,
            due_before: before,
            ..Default::default()
        };
        f.matches(&FilterTarget {
            state: "todo",
            due: Some(due),
            priority: Priority::default(),
            tags: &[],
            fields: &BTreeMap::new(),
        })
    }

    #[test]
    fn today_is_exactly_that_day() {
        assert!(matches(DueBucket::Today, "2026-08-05", "2026-08-05"));
        assert!(!matches(DueBucket::Today, "2026-08-05", "2026-08-04"));
        assert!(!matches(DueBucket::Today, "2026-08-05", "2026-08-06"));
    }

    #[test]
    fn overdue_is_strictly_before_today() {
        assert!(matches(DueBucket::Overdue, "2026-08-05", "2026-08-04"));
        assert!(!matches(DueBucket::Overdue, "2026-08-05", "2026-08-05"));
        assert!(!matches(DueBucket::Overdue, "2026-08-05", "2026-08-06"));
    }

    #[test]
    fn week_is_today_through_six_days_out() {
        assert!(matches(DueBucket::Week, "2026-08-05", "2026-08-05"));
        assert!(matches(DueBucket::Week, "2026-08-05", "2026-08-11"));
        assert!(!matches(DueBucket::Week, "2026-08-05", "2026-08-12"));
        assert!(!matches(DueBucket::Week, "2026-08-05", "2026-08-04"));
    }

    #[test]
    fn an_undated_task_is_in_no_bucket() {
        for b in [DueBucket::Today, DueBucket::Week, DueBucket::Overdue] {
            let (after, before) = b.bounds(d("2026-08-05")).unwrap();
            let f = TaskFilter {
                due_after: after,
                due_before: before,
                ..Default::default()
            };
            assert!(!f.matches(&FilterTarget {
                state: "todo",
                due: None,
                priority: Priority::default(),
                tags: &[],
                fields: &BTreeMap::new(),
            }));
        }
    }

    #[test]
    fn bounds_stay_fixed_width_across_month_and_year_rollovers() {
        for (today, bucket) in [
            ("2026-01-01", DueBucket::Today),
            ("2026-12-31", DueBucket::Week),
            ("2024-02-29", DueBucket::Today),
            ("2026-03-01", DueBucket::Today),
        ] {
            let (after, before) = bucket.bounds(d(today)).unwrap();
            for b in [after, before].into_iter().flatten() {
                assert_eq!(b.len(), 10, "{b} from {today}");
                assert!(crate::is_date_like(&b), "{b} from {today}");
            }
        }
    }

    #[test]
    fn a_leap_day_rolls_over_correctly() {
        assert!(matches(DueBucket::Week, "2024-02-26", "2024-02-29"));
        assert!(matches(DueBucket::Today, "2024-02-28", "2024-02-28"));
        assert!(matches(DueBucket::Overdue, "2024-03-01", "2024-02-29"));
    }
}
