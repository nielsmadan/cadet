use crate::error::CoreError;
use crate::fieldparse::is_date_like;

const EXPECTED: &str = "a date such as 2026-08-10, or `today`, `tomorrow`, `+7d`, `+2w`";

fn bad() -> CoreError {
    CoreError::FieldType {
        field: "due".to_string(),
        expected: EXPECTED.to_string(),
    }
}

/// Resolve a due specification against a calendar day.
///
/// A literal date passes through unchanged; `today`, `tomorrow` and `+Nd` /
/// `+Nw` resolve relative to `today`. Relative forms exist so a configured
/// default is not stale the day after it is written.
pub fn resolve_due(spec: &str, today: jiff::civil::Date) -> Result<String, CoreError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(bad());
    }
    if is_date_like(spec) {
        return Ok(spec.to_string());
    }
    let date = match spec {
        "today" => today,
        "tomorrow" => today.tomorrow().map_err(|_| bad())?,
        rest => {
            let rest = rest.strip_prefix('+').ok_or_else(bad)?;
            let (n, unit) = rest.split_at(rest.len().saturating_sub(1));
            let n: i64 = n.parse().map_err(|_| bad())?;
            let span = match unit {
                "d" => jiff::Span::new().days(n),
                "w" => jiff::Span::new().weeks(n),
                _ => return Err(bad()),
            };
            today.checked_add(span).map_err(|_| bad())?
        }
    };
    Ok(date.to_string())
}

/// Which due date a new task gets: an explicit value beats the project's
/// default, which beats the global one, and `--no-due` beats all of them.
///
/// One function so the precedence exists in exactly one place — the CLI holds
/// the global default and the backend holds the project's, and a rule split
/// across those two is the divergence this codebase keeps producing.
pub fn resolve_due_for_new_task(
    explicit: Option<&str>,
    no_due: bool,
    project_default: Option<&str>,
    global_default: Option<&str>,
    today: jiff::civil::Date,
) -> Result<Option<String>, CoreError> {
    if no_due {
        return Ok(None);
    }
    match explicit.or(project_default).or(global_default) {
        None => Ok(None),
        Some(spec) => resolve_due(spec, today).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> jiff::civil::Date {
        s.parse().unwrap()
    }

    #[test]
    fn a_literal_date_passes_through() {
        assert_eq!(
            resolve_due("2026-08-10", d("2026-08-05")).unwrap(),
            "2026-08-10"
        );
    }

    #[test]
    fn the_relative_words_resolve() {
        assert_eq!(resolve_due("today", d("2026-08-05")).unwrap(), "2026-08-05");
        assert_eq!(
            resolve_due("tomorrow", d("2026-08-05")).unwrap(),
            "2026-08-06"
        );
    }

    #[test]
    fn day_and_week_offsets_resolve() {
        assert_eq!(resolve_due("+7d", d("2026-08-05")).unwrap(), "2026-08-12");
        assert_eq!(resolve_due("+2w", d("2026-08-05")).unwrap(), "2026-08-19");
        assert_eq!(resolve_due("+0d", d("2026-08-05")).unwrap(), "2026-08-05");
    }

    #[test]
    fn offsets_cross_month_and_year_boundaries() {
        assert_eq!(resolve_due("+1d", d("2026-12-31")).unwrap(), "2027-01-01");
        assert_eq!(resolve_due("+1d", d("2024-02-28")).unwrap(), "2024-02-29");
    }

    #[test]
    fn a_resolved_value_is_always_fixed_width() {
        for spec in ["today", "tomorrow", "+1d", "+40w"] {
            let got = resolve_due(spec, d("2026-01-01")).unwrap();
            assert_eq!(got.len(), 10, "{spec} -> {got}");
            assert!(is_date_like(&got), "{spec} -> {got}");
        }
    }

    #[test]
    fn nonsense_is_rejected() {
        for spec in ["", "   ", "banana", "7d", "+d", "+7", "+7y", "-7d", "+7 d"] {
            assert!(resolve_due(spec, d("2026-08-05")).is_err(), "{spec:?}");
        }
    }

    #[test]
    fn precedence_is_explicit_then_project_then_global() {
        let today = d("2026-08-05");
        let r = |e, p, g| resolve_due_for_new_task(e, false, p, g, today).unwrap();
        assert_eq!(
            r(Some("+1d"), Some("+2d"), Some("+3d")).as_deref(),
            Some("2026-08-06")
        );
        assert_eq!(
            r(None, Some("+2d"), Some("+3d")).as_deref(),
            Some("2026-08-07")
        );
        assert_eq!(r(None, None, Some("+3d")).as_deref(), Some("2026-08-08"));
        assert_eq!(r(None, None, None), None);
    }

    #[test]
    fn no_due_beats_every_default() {
        let today = d("2026-08-05");
        assert_eq!(
            resolve_due_for_new_task(Some("+1d"), true, Some("+2d"), Some("+3d"), today).unwrap(),
            None
        );
    }
}
