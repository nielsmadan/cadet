use crate::config::{FieldType, ProjectConfig, Workflow};
use crate::error::CoreError;
use crate::model::{FieldValue, Task};

pub fn validate_task(task: &Task, cfg: &ProjectConfig) -> Result<(), CoreError> {
    if task.title.trim().is_empty() {
        return Err(CoreError::EmptyTitle);
    }
    if task.key.prefix != cfg.prefix {
        return Err(CoreError::ForeignKeyPrefix {
            expected: cfg.prefix.clone(),
            found: task.key.prefix.clone(),
        });
    }
    if !cfg.workflow.states.contains(&task.state) {
        return Err(CoreError::UnknownState(task.state.clone()));
    }
    for def in &cfg.fields {
        if def.required && !task.fields.contains_key(&def.name) {
            return Err(CoreError::FieldType {
                field: def.name.clone(),
                expected: "a value (field is required)".into(),
            });
        }
    }
    for (name, value) in &task.fields {
        let def = cfg
            .fields
            .iter()
            .find(|d| &d.name == name)
            .ok_or_else(|| CoreError::UnknownField(name.clone()))?;
        match (&def.ty, value) {
            (FieldType::Str | FieldType::Text, FieldValue::Str(_)) => {}
            (FieldType::Int, FieldValue::Int(_)) => {}
            (FieldType::Float, FieldValue::Float(_) | FieldValue::Int(_)) => {}
            (FieldType::Bool, FieldValue::Bool(_)) => {}
            (FieldType::Date, FieldValue::Date(s)) => {
                s.parse::<jiff::civil::Date>()
                    .map_err(|_| CoreError::InvalidDateValue {
                        field: name.clone(),
                        expected: "date".into(),
                        value: s.clone(),
                    })?;
            }
            (FieldType::DateTime, FieldValue::Date(s)) => {
                s.parse::<jiff::Timestamp>()
                    .map_err(|_| CoreError::InvalidDateValue {
                        field: name.clone(),
                        expected: "datetime".into(),
                        value: s.clone(),
                    })?;
            }
            (FieldType::ListStr, FieldValue::List(_)) => {}
            (FieldType::Enum(allowed), FieldValue::Str(s)) if allowed.contains(s) => {}
            _ => {
                return Err(CoreError::FieldType {
                    field: name.clone(),
                    expected: format!("{:?}", def.ty),
                });
            }
        }
    }
    Ok(())
}

pub fn check_transition(wf: &Workflow, from: &str, to: &str) -> Result<(), CoreError> {
    if !wf.states.iter().any(|s| s == from) {
        return Err(CoreError::UnknownState(from.to_string()));
    }
    if !wf.states.iter().any(|s| s == to) {
        return Err(CoreError::UnknownState(to.to_string()));
    }
    if wf.transitions.is_empty() || from == to {
        return Ok(());
    }
    match wf.transitions.get(from) {
        Some(allowed) if allowed.iter().any(|s| s == to) => Ok(()),
        _ => Err(CoreError::IllegalTransition {
            from: from.to_string(),
            to: to.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use std::collections::BTreeMap;

    fn cfg() -> ProjectConfig {
        ProjectConfig::parse(
            r#"
[project]
id = "p"
name = "P"
prefix = "P"
[workflow]
states = ["todo", "doing", "done"]
initial = "todo"
terminal = ["done"]
[workflow.transitions]
todo = ["doing"]
doing = ["done"]
done = []
[[fields]]
name = "category"
type = "enum"
values = ["a", "b"]
"#,
        )
        .unwrap()
    }

    fn cfg_with_dates_and_required() -> ProjectConfig {
        ProjectConfig::parse(
            r#"
[project]
id = "p"
name = "P"
prefix = "P"
[workflow]
states = ["todo", "doing", "done"]
initial = "todo"
terminal = ["done"]
[[fields]]
name = "due_date"
type = "date"
[[fields]]
name = "starts_at"
type = "datetime"
[[fields]]
name = "owner"
type = "str"
required = true
"#,
        )
        .unwrap()
    }

    fn task(state: &str) -> Task {
        Task {
            uid: TaskUid::generate(),
            key: TaskKey::new("P", 1),
            title: "t".into(),
            state: state.into(),
            created: jiff::Timestamp::UNIX_EPOCH,
            updated: jiff::Timestamp::UNIX_EPOCH,
            due: None,
            priority: Priority::Normal,
            tags: vec![],
            renumbered_from: None,
            possible_duplicate_of: None,
            fields: BTreeMap::new(),
            body: String::new(),
        }
    }

    #[test]
    fn accepts_a_valid_task() {
        assert!(validate_task(&task("todo"), &cfg()).is_ok());
    }

    #[test]
    fn rejects_unknown_state() {
        let err = validate_task(&task("banana"), &cfg()).unwrap_err();
        assert!(matches!(err, CoreError::UnknownState(_)));
    }

    #[test]
    fn rejects_unknown_field() {
        let mut t = task("todo");
        t.fields.insert("nope".into(), FieldValue::Str("x".into()));
        assert!(matches!(
            validate_task(&t, &cfg()).unwrap_err(),
            CoreError::UnknownField(_)
        ));
    }

    #[test]
    fn rejects_enum_value_outside_declared_set() {
        let mut t = task("todo");
        t.fields
            .insert("category".into(), FieldValue::Str("zzz".into()));
        assert!(matches!(
            validate_task(&t, &cfg()).unwrap_err(),
            CoreError::FieldType { .. }
        ));
    }

    #[test]
    fn rejects_illegal_transition() {
        let c = cfg();
        assert!(check_transition(&c.workflow, "todo", "done").is_err());
        assert!(check_transition(&c.workflow, "todo", "doing").is_ok());
    }

    #[test]
    fn empty_transition_map_allows_anything() {
        let mut c = cfg();
        c.workflow.transitions.clear();
        assert!(check_transition(&c.workflow, "todo", "done").is_ok());
    }

    #[test]
    fn rejects_unknown_from_state_with_empty_transition_map() {
        let mut c = cfg();
        c.workflow.transitions.clear();
        assert!(matches!(
            check_transition(&c.workflow, "garbage", "todo").unwrap_err(),
            CoreError::UnknownState(ref s) if s == "garbage"
        ));
    }

    #[test]
    fn rejects_unknown_from_state_when_from_equals_to() {
        let c = cfg();
        assert!(matches!(
            check_transition(&c.workflow, "garbage", "garbage").unwrap_err(),
            CoreError::UnknownState(ref s) if s == "garbage"
        ));
    }

    #[test]
    fn rejects_malformed_date_value() {
        let mut t = task("todo");
        t.fields.insert("owner".into(), FieldValue::Str("x".into()));
        t.fields
            .insert("due_date".into(), FieldValue::Date("banana".into()));
        assert!(matches!(
            validate_task(&t, &cfg_with_dates_and_required()).unwrap_err(),
            CoreError::InvalidDateValue { .. }
        ));
    }

    #[test]
    fn accepts_a_well_formed_date_value() {
        let mut t = task("todo");
        t.fields.insert("owner".into(), FieldValue::Str("x".into()));
        t.fields
            .insert("due_date".into(), FieldValue::Date("2026-08-01".into()));
        assert!(validate_task(&t, &cfg_with_dates_and_required()).is_ok());
    }

    #[test]
    fn rejects_datetime_value_that_is_only_a_date() {
        let mut t = task("todo");
        t.fields.insert("owner".into(), FieldValue::Str("x".into()));
        t.fields
            .insert("starts_at".into(), FieldValue::Date("2026-08-01".into()));
        assert!(matches!(
            validate_task(&t, &cfg_with_dates_and_required()).unwrap_err(),
            CoreError::InvalidDateValue { .. }
        ));
    }

    #[test]
    fn rejects_missing_required_field() {
        let t = task("todo");
        assert!(matches!(
            validate_task(&t, &cfg_with_dates_and_required()).unwrap_err(),
            CoreError::FieldType { .. }
        ));
    }

    #[test]
    fn rejects_foreign_key_prefix() {
        let mut t = task("todo");
        t.key = TaskKey::new("OTHER", 1);
        assert!(matches!(
            validate_task(&t, &cfg()).unwrap_err(),
            CoreError::ForeignKeyPrefix { .. }
        ));
    }

    #[test]
    fn rejects_empty_title() {
        let mut t = task("todo");
        t.title = "".into();
        assert!(matches!(
            validate_task(&t, &cfg()).unwrap_err(),
            CoreError::EmptyTitle
        ));
    }
}
