use crate::model::{FieldValue, Priority, Task};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(String);

impl Revision {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

fn normalise(s: &str) -> String {
    s.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render(v: &FieldValue) -> String {
    match v {
        FieldValue::Str(s) | FieldValue::Date(s) => s.clone(),
        FieldValue::Int(i) => i.to_string(),
        FieldValue::Float(f) => format!("{f}"),
        FieldValue::Bool(b) => b.to_string(),
        FieldValue::List(items) => {
            let mut out = String::new();
            let _ = write!(out, "{}", items.len());
            for it in items {
                let normalised = normalise(it);
                let _ = write!(out, ":{}:{normalised}", normalised.len());
            }
            out
        }
    }
}

fn put(out: &mut String, key: &str, value: &str) {
    // `value` must already be normalised
    let _ = writeln!(out, "{key}:{}:{value}", value.len());
}

fn put_opt(out: &mut String, key: &str, value: Option<&str>) {
    match value {
        None => {
            let _ = writeln!(out, "{key}:-");
        }
        Some(v) => {
            let _ = writeln!(out, "{key}:+{}:{v}", v.len());
        }
    }
}

fn put_list(out: &mut String, key: &str, items: &[String]) {
    let _ = write!(out, "{key}:{}", items.len());
    for it in items {
        let _ = write!(out, ":{}:{it}", it.len());
    }
    let _ = writeln!(out);
}

/// Deterministic text projection of a task, excluding every field that Cadet
/// itself rewrites (`key`, `updated`, `renumbered_from`, `possible_duplicate_of`).
/// See spec §5.
///
/// Every value is length-prefixed before being written so the projection is
/// injective: without a length prefix, `tags = ["a,b"]` and `tags = ["a", "b"]`
/// (or a field value containing `\n` and two separate fields) would render to
/// byte-identical output despite representing different tasks.
pub fn canonical_projection(t: &Task) -> String {
    let mut out = String::new();
    put(&mut out, "uid", t.uid.as_str());
    put(&mut out, "title", &normalise(&t.title));
    put(&mut out, "state", &normalise(&t.state));
    let _ = writeln!(out, "created:{}", t.created);
    let due = t.due.as_deref().map(normalise);
    put_opt(&mut out, "due", due.as_deref());
    put(
        &mut out,
        "priority",
        match t.priority {
            Priority::High => "high",
            Priority::Normal => "normal",
            Priority::Low => "low",
        },
    );
    let mut tags: Vec<String> = t.tags.iter().map(|s| normalise(s)).collect();
    tags.sort();
    put_list(&mut out, "tags", &tags);
    put(&mut out, "fields", &t.fields.len().to_string());
    for (k, v) in &t.fields {
        let value = normalise(&render(v));
        let _ = writeln!(out, "{}:{k}:{}:{value}", k.len(), value.len());
    }
    out.push('\n');
    out.push_str(&normalise(&t.body));
    out
}

pub fn revision(t: &Task) -> Revision {
    Revision(
        blake3::hash(canonical_projection(t).as_bytes())
            .to_hex()
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use std::collections::BTreeMap;

    fn task() -> Task {
        Task {
            uid: TaskUid::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            key: TaskKey::new("P", 1),
            title: "Buy milk".into(),
            state: "todo".into(),
            created: jiff::Timestamp::UNIX_EPOCH,
            updated: jiff::Timestamp::UNIX_EPOCH,
            due: None,
            priority: Priority::Normal,
            tags: vec!["home".into()],
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: BTreeMap::new(),
            body: "notes\n".into(),
        }
    }

    #[test]
    fn key_is_excluded_so_renumbering_does_not_change_the_hash() {
        let a = task();
        let mut b = task();
        b.key = TaskKey::new("P", 99);
        assert_eq!(revision(&a), revision(&b));
    }

    #[test]
    fn updated_is_excluded() {
        let a = task();
        let mut b = task();
        b.updated = jiff::Timestamp::from_second(1_000_000).unwrap();
        assert_eq!(revision(&a), revision(&b));
    }

    #[test]
    fn renumbered_from_is_excluded() {
        let a = task();
        let mut b = task();
        b.renumbered_from = Some(TaskKey::new("P", 4));
        assert_eq!(revision(&a), revision(&b));
    }

    #[test]
    fn crlf_and_lf_bodies_hash_identically() {
        let a = task();
        let mut b = task();
        b.body = "notes\r\n".into();
        assert_eq!(revision(&a), revision(&b));
    }

    #[test]
    fn trailing_whitespace_is_ignored() {
        let a = task();
        let mut b = task();
        b.body = "notes   \n".into();
        assert_eq!(revision(&a), revision(&b));
    }

    #[test]
    fn title_change_does_change_the_hash() {
        let a = task();
        let mut b = task();
        b.title = "Buy oat milk".into();
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn field_order_does_not_affect_the_hash() {
        let mut a = task();
        let mut b = task();
        a.fields.insert("x".into(), FieldValue::Int(1));
        a.fields.insert("y".into(), FieldValue::Int(2));
        b.fields.insert("y".into(), FieldValue::Int(2));
        b.fields.insert("x".into(), FieldValue::Int(1));
        assert_eq!(revision(&a), revision(&b));
    }

    #[test]
    fn tags_with_a_comma_do_not_collide_with_two_tags() {
        let mut a = task();
        let mut b = task();
        a.tags = vec!["a,b".into()];
        b.tags = vec!["a".into(), "b".into()];
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn a_field_value_containing_a_newline_cannot_forge_a_second_field() {
        let mut a = task();
        let mut b = task();
        a.fields
            .insert("a".into(), FieldValue::Str("x\nb:y".into()));
        b.fields.insert("a".into(), FieldValue::Str("x".into()));
        b.fields.insert("b".into(), FieldValue::Str("y".into()));
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn absent_due_differs_from_empty_due() {
        let mut a = task();
        let mut b = task();
        a.due = None;
        b.due = Some("".into());
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn possible_duplicate_of_is_excluded() {
        let a = task();
        let mut b = task();
        b.possible_duplicate_of = TaskUid::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW");
        assert_eq!(revision(&a), revision(&b));
    }

    #[test]
    fn state_change_does_change_the_hash() {
        let a = task();
        let mut b = task();
        b.state = "doing".into();
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn due_change_does_change_the_hash() {
        let a = task();
        let mut b = task();
        b.due = Some("2026-08-01".into());
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn priority_change_does_change_the_hash() {
        let a = task();
        let mut b = task();
        b.priority = Priority::High;
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn tags_change_does_change_the_hash() {
        let a = task();
        let mut b = task();
        b.tags = vec!["work".into()];
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn field_value_change_does_change_the_hash() {
        let mut a = task();
        let mut b = task();
        a.fields.insert("x".into(), FieldValue::Int(1));
        b.fields.insert("x".into(), FieldValue::Int(2));
        assert_ne!(revision(&a), revision(&b));
    }

    #[test]
    fn body_change_does_change_the_hash() {
        let a = task();
        let mut b = task();
        b.body = "different notes\n".into();
        assert_ne!(revision(&a), revision(&b));
    }
}
