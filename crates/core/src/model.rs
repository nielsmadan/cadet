use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskUid(String);

impl TaskUid {
    pub fn generate() -> Self {
        // ulid 3.0 exposes `generate()`, not `new()`.
        Self(ulid::Ulid::generate().to_string())
    }
    pub fn parse(s: &str) -> Option<Self> {
        ulid::Ulid::from_string(s).ok().map(|u| Self(u.to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskKey {
    pub prefix: String,
    pub number: u32,
}

impl TaskKey {
    pub fn new(prefix: impl Into<String>, number: u32) -> Self {
        Self {
            prefix: prefix.into(),
            number,
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        let (prefix, num) = s.rsplit_once('-')?;
        if prefix.is_empty() {
            return None;
        }
        Some(Self::new(prefix, num.parse().ok()?))
    }
}

impl fmt::Display for TaskKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.prefix, self.number)
    }
}

pub type State = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    High,
    #[default]
    Normal,
    Low,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Date(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub uid: TaskUid,
    pub key: TaskKey,
    pub title: String,
    pub state: State,
    pub created: jiff::Timestamp,
    pub updated: jiff::Timestamp,
    pub due: Option<String>,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub renumbered_from: Option<TaskKey>,
    pub possible_duplicate_of: Option<TaskUid>,
    pub fields: BTreeMap<String, FieldValue>,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_key_round_trips() {
        let k = TaskKey::new("PERS", 4);
        assert_eq!(k.to_string(), "PERS-4");
        assert_eq!(TaskKey::parse("PERS-4").unwrap(), k);
    }

    #[test]
    fn task_key_rejects_malformed() {
        assert!(TaskKey::parse("PERS").is_none());
        assert!(TaskKey::parse("PERS-").is_none());
        assert!(TaskKey::parse("PERS-x").is_none());
    }

    // ULIDs are time-ordered only at millisecond granularity — two generated in
    // the same millisecond have random suffixes and sort arbitrarily. Uniqueness
    // is what Cadet depends on: `uid` is an identity, never a sort key.
    #[test]
    fn uid_is_unique_per_call() {
        let a = TaskUid::generate();
        let b = TaskUid::generate();
        assert_ne!(a.as_str(), b.as_str());
    }
}
