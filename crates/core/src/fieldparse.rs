use crate::config::{FieldDef, FieldType, ProjectConfig};
use crate::error::CoreError;
use crate::model::FieldValue;

fn wrong(field: &str, expected: &str) -> CoreError {
    CoreError::FieldType {
        field: field.to_string(),
        expected: expected.to_string(),
    }
}

/// Rejects a line break in a value that has to occupy exactly one line.
///
/// Frontmatter is line-oriented: a value carrying a newline does not merely
/// render badly, it ends the entry and turns the remainder into its own
/// frontmatter line — an orphan at best, and an injected key (`estimate: 999`)
/// at worst, with everything after the break lost from the value forever.
///
/// The single copy of that rule. `parse_field_value` applies it to every
/// single-line field type, and the CLI applies it to a task's title, which
/// lands in the same frontmatter block through a different door.
pub fn reject_newlines(field: &str, value: &str) -> Result<(), CoreError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(wrong(field, "text without newlines"));
    }
    Ok(())
}

pub fn parse_field_value(def: &FieldDef, raw: &str) -> Result<FieldValue, CoreError> {
    let v = raw.trim();
    Ok(match &def.ty {
        FieldType::Str | FieldType::Text => {
            reject_newlines(&def.name, v)?;
            FieldValue::Str(v.to_string())
        }
        FieldType::Int => {
            FieldValue::Int(v.parse().map_err(|_| wrong(&def.name, "a whole number"))?)
        }
        FieldType::Float => {
            let f: f64 = v.parse().map_err(|_| wrong(&def.name, "a number"))?;
            if !f.is_finite() {
                return Err(wrong(&def.name, "a finite number"));
            }
            FieldValue::Float(f)
        }
        FieldType::Bool => match v.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => FieldValue::Bool(true),
            "false" | "no" | "n" | "0" => FieldValue::Bool(false),
            _ => return Err(wrong(&def.name, "true or false")),
        },
        FieldType::Date | FieldType::DateTime => {
            if !is_date_like(v) {
                return Err(wrong(&def.name, "a date such as 2026-08-10"));
            }
            FieldValue::Date(v.to_string())
        }
        FieldType::Enum(choices) => {
            reject_newlines(&def.name, v)?;
            if choices.iter().any(|c| c == v) {
                FieldValue::Str(v.to_string())
            } else {
                return Err(wrong(&def.name, &format!("one of: {}", choices.join(", "))));
            }
        }
        FieldType::ListStr => {
            reject_newlines(&def.name, v)?;
            if v.is_empty() {
                FieldValue::List(vec![])
            } else {
                FieldValue::List(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            }
        }
    })
}

/// Deliberately shallow: `YYYY-MM-DD` optionally followed by a time. Dates are
/// stored as the string the user typed (`Task::due` is an `Option<String>`),
/// so this rejects obvious nonsense without claiming to be a calendar.
///
/// This is the shared gate for every date-shaped value Cadet writes, `due`
/// included — not only declared custom fields of type `Date`/`DateTime`.
pub fn is_date_like(s: &str) -> bool {
    // Check for valid separator (T or space), and if present, ensure there's content after it.
    let parts: Vec<&str> = s.splitn(2, ['T', ' ']).collect();
    if parts.len() == 2 && parts[1].is_empty() {
        return false; // "2026-08-10T" or "2026-08-10 " are invalid
    }

    let d = parts[0];
    let mut date_parts = d.split('-');
    let (y, m, day) = (date_parts.next(), date_parts.next(), date_parts.next());
    if date_parts.next().is_some() {
        return false;
    }

    // Validate year is 4 digits, month is 01-12, day is 01-31
    if let (Some(y), Some(m), Some(d)) = (y, m, day) {
        if y.len() != 4 || !y.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if m.len() != 2 || !m.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if d.len() != 2 || !d.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }

        // Parse and validate month (01-12) and day (01-31)
        if let (Ok(month), Ok(day_num)) = (m.parse::<u32>(), d.parse::<u32>()) {
            return (1..=12).contains(&month) && (1..=31).contains(&day_num);
        }
        return false;
    }

    false
}

pub fn parse_assignment(
    cfg: &ProjectConfig,
    pair: &str,
) -> Result<(String, FieldValue), CoreError> {
    let (name, raw) = pair
        .split_once('=')
        .ok_or_else(|| CoreError::ConfigParse(format!("expected name=value, got `{pair}`")))?;
    let name = name.trim();
    let def = cfg
        .fields
        .iter()
        .find(|f| f.name == name)
        .ok_or_else(|| CoreError::UnknownField(name.to_string()))?;
    Ok((name.to_string(), parse_field_value(def, raw)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FieldDef, FieldType};

    fn def(name: &str, ty: FieldType) -> FieldDef {
        FieldDef {
            name: name.into(),
            ty,
            required: false,
        }
    }

    #[test]
    fn parses_each_scalar_type() {
        assert_eq!(
            parse_field_value(&def("a", FieldType::Int), "3").unwrap(),
            FieldValue::Int(3)
        );
        assert_eq!(
            parse_field_value(&def("a", FieldType::Float), "1.5").unwrap(),
            FieldValue::Float(1.5)
        );
        assert_eq!(
            parse_field_value(&def("a", FieldType::Bool), "true").unwrap(),
            FieldValue::Bool(true)
        );
        assert_eq!(
            parse_field_value(&def("a", FieldType::Str), "hi").unwrap(),
            FieldValue::Str("hi".into())
        );
        assert_eq!(
            parse_field_value(&def("a", FieldType::Date), "2026-08-10").unwrap(),
            FieldValue::Date("2026-08-10".into())
        );
    }

    #[test]
    fn rejects_a_value_that_does_not_match_its_declared_type() {
        let e = parse_field_value(&def("estimate", FieldType::Int), "soon").unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("estimate"),
            "error should name the field: {msg}"
        );
        assert!(
            msg.contains("whole number"),
            "error should say what was expected: {msg}"
        );
    }

    #[test]
    fn bool_accepts_the_usual_spellings_and_rejects_the_rest() {
        for yes in ["true", "yes", "1"] {
            assert_eq!(
                parse_field_value(&def("a", FieldType::Bool), yes).unwrap(),
                FieldValue::Bool(true)
            );
        }
        for no in ["false", "no", "0"] {
            assert_eq!(
                parse_field_value(&def("a", FieldType::Bool), no).unwrap(),
                FieldValue::Bool(false)
            );
        }
        assert!(parse_field_value(&def("a", FieldType::Bool), "maybe").is_err());
    }

    #[test]
    fn enum_accepts_only_declared_variants() {
        let d = def("size", FieldType::Enum(vec!["s".into(), "m".into()]));
        assert_eq!(
            parse_field_value(&d, "m").unwrap(),
            FieldValue::Str("m".into())
        );
        let msg = parse_field_value(&d, "xl").unwrap_err().to_string();
        assert!(
            msg.contains("s") && msg.contains("m"),
            "error should list the choices: {msg}"
        );
    }

    #[test]
    fn list_splits_on_commas_and_trims() {
        assert_eq!(
            parse_field_value(&def("a", FieldType::ListStr), "x, y ,z").unwrap(),
            FieldValue::List(vec!["x".into(), "y".into(), "z".into()])
        );
        assert_eq!(
            parse_field_value(&def("a", FieldType::ListStr), "").unwrap(),
            FieldValue::List(vec![])
        );
    }

    #[test]
    fn date_rejects_a_non_date() {
        assert!(parse_field_value(&def("when", FieldType::Date), "tomorrow").is_err());
    }

    #[test]
    fn assignment_splits_on_the_first_equals_only() {
        let cfg = ProjectConfig {
            id: "p".into(),
            name: "P".into(),
            prefix: "P".into(),
            match_mode: crate::config::MatchMode::Frontmatter,
            include: vec![],
            exclude: vec![],
            workflow: crate::config::Workflow {
                states: vec!["todo".into()],
                initial: "todo".into(),
                terminal: vec![],
                transitions: Default::default(),
            },
            defaults: Default::default(),
            fields: vec![def("note", FieldType::Str)],
        };
        let (k, v) = parse_assignment(&cfg, "note=a=b").unwrap();
        assert_eq!(k, "note");
        assert_eq!(v, FieldValue::Str("a=b".into()));
    }

    #[test]
    fn assignment_rejects_an_undeclared_field() {
        let cfg = ProjectConfig {
            id: "p".into(),
            name: "P".into(),
            prefix: "P".into(),
            match_mode: crate::config::MatchMode::Frontmatter,
            include: vec![],
            exclude: vec![],
            workflow: crate::config::Workflow {
                states: vec!["todo".into()],
                initial: "todo".into(),
                terminal: vec![],
                transitions: Default::default(),
            },
            defaults: Default::default(),
            fields: vec![],
        };
        assert!(parse_assignment(&cfg, "nope=1").is_err());
        assert!(parse_assignment(&cfg, "novalue").is_err());
    }

    #[test]
    fn str_rejects_newlines() {
        let e = parse_field_value(&def("note", FieldType::Str), "line1\nline2").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("note"), "error should name the field: {msg}");
    }

    #[test]
    fn text_rejects_newlines() {
        assert!(parse_field_value(&def("body", FieldType::Text), "line1\nline2").is_err());
    }

    #[test]
    fn str_rejects_carriage_returns() {
        assert!(parse_field_value(&def("note", FieldType::Str), "line1\rline2").is_err());
    }

    #[test]
    fn enum_rejects_newlines() {
        let d = def("size", FieldType::Enum(vec!["s".into(), "m".into()]));
        assert!(parse_field_value(&d, "s\ninjection").is_err());
    }

    #[test]
    fn list_rejects_newlines() {
        let e = parse_field_value(&def("tags", FieldType::ListStr), "a\nb").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("tags"), "error should name the field: {msg}");
    }

    #[test]
    fn float_rejects_infinity() {
        assert!(parse_field_value(&def("score", FieldType::Float), "inf").is_err());
        assert!(parse_field_value(&def("score", FieldType::Float), "-inf").is_err());
    }

    #[test]
    fn float_rejects_nan() {
        assert!(parse_field_value(&def("score", FieldType::Float), "NaN").is_err());
    }

    #[test]
    fn float_rejects_silent_overflow() {
        assert!(parse_field_value(&def("score", FieldType::Float), "1e400").is_err());
    }

    #[test]
    fn float_accepts_plus_sign() {
        assert_eq!(
            parse_field_value(&def("a", FieldType::Float), "+1.5").unwrap(),
            FieldValue::Float(1.5)
        );
    }

    #[test]
    fn int_accepts_plus_sign() {
        assert_eq!(
            parse_field_value(&def("a", FieldType::Int), "+3").unwrap(),
            FieldValue::Int(3)
        );
    }

    #[test]
    fn date_rejects_invalid_month() {
        assert!(parse_field_value(&def("when", FieldType::Date), "2026-13-10").is_err());
        assert!(parse_field_value(&def("when", FieldType::Date), "2026-00-10").is_err());
    }

    #[test]
    fn date_rejects_invalid_day() {
        assert!(parse_field_value(&def("when", FieldType::Date), "2026-08-40").is_err());
        assert!(parse_field_value(&def("when", FieldType::Date), "2026-08-00").is_err());
    }

    #[test]
    fn date_rejects_calendar_nonsense() {
        assert!(parse_field_value(&def("when", FieldType::Date), "9999-99-99").is_err());
    }

    #[test]
    fn date_rejects_trailing_garbage() {
        assert!(parse_field_value(&def("when", FieldType::Date), "2026-08-10garbage").is_err());
    }

    #[test]
    fn date_rejects_incomplete_time() {
        assert!(parse_field_value(&def("when", FieldType::Date), "2026-08-10T").is_err());
    }

    #[test]
    fn date_accepts_time_separator() {
        assert_eq!(
            parse_field_value(&def("when", FieldType::DateTime), "2026-08-10T14:30:00").unwrap(),
            FieldValue::Date("2026-08-10T14:30:00".into())
        );
        assert_eq!(
            parse_field_value(&def("when", FieldType::DateTime), "2026-08-10 14:30:00").unwrap(),
            FieldValue::Date("2026-08-10 14:30:00".into())
        );
    }
}
