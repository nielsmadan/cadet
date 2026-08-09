use crate::error::CoreError;
const EXPECTED: &str =
    "a date such as 2026-08-10, `today`, `tomorrow`, `10d`, `+10d`, `1w`, or `aug10`";

fn bad() -> CoreError {
    CoreError::FieldType {
        field: "due".to_string(),
        expected: EXPECTED.to_string(),
    }
}

/// Resolve a due specification against a calendar day.
///
/// A literal date passes through unchanged. Words, day or week offsets, and
/// month/day forms resolve relative to `today`.
pub fn resolve_due(spec: &str, today: jiff::civil::Date) -> Result<String, CoreError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(bad());
    }
    if let Ok(date) = spec.parse::<jiff::civil::Date>() {
        return Ok(date.to_string());
    }
    let lower = spec.to_ascii_lowercase();
    let date = match lower.as_str() {
        "today" => today,
        "tomorrow" => today.tomorrow().map_err(|_| bad())?,
        rest => {
            if let Some((n, unit)) = parse_offset(rest) {
                let span = match unit {
                    'd' => jiff::Span::new().days(n),
                    'w' => jiff::Span::new().weeks(n),
                    _ => return Err(bad()),
                };
                today.checked_add(span).map_err(|_| bad())?
            } else if let Some((month, day)) = parse_month_day(rest) {
                next_month_day(today, month, day).ok_or_else(bad)?
            } else {
                return Err(bad());
            }
        }
    };
    Ok(date.to_string())
}

pub fn canonical_due_date(spec: &str) -> Result<String, CoreError> {
    spec.trim()
        .parse::<jiff::civil::Date>()
        .map(|date| date.to_string())
        .map_err(|_| bad())
}

pub fn select_due_for_new_task<'a>(
    explicit: Option<&'a str>,
    no_due: bool,
    project_default: Option<&'a str>,
    global_default: Option<&'a str>,
) -> Option<&'a str> {
    if no_due {
        None
    } else {
        explicit.or(project_default).or(global_default)
    }
}

fn parse_offset(spec: &str) -> Option<(i64, char)> {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (number, unit) = spec.split_at(spec.len().checked_sub(1)?);
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let unit = unit.chars().next()?;
    if !matches!(unit, 'd' | 'w') {
        return None;
    }
    Some((number.parse().ok()?, unit))
}

fn parse_month_day(spec: &str) -> Option<(i8, i8)> {
    const MONTHS: [(&str, i8); 12] = [
        ("jan", 1),
        ("feb", 2),
        ("mar", 3),
        ("apr", 4),
        ("may", 5),
        ("jun", 6),
        ("jul", 7),
        ("aug", 8),
        ("sep", 9),
        ("oct", 10),
        ("nov", 11),
        ("dec", 12),
    ];
    let (prefix, month) = MONTHS.iter().find(|(prefix, _)| spec.starts_with(prefix))?;
    let day = spec[prefix.len()..].trim_start();
    if day.is_empty() || !day.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((*month, day.parse().ok()?))
}

fn next_month_day(today: jiff::civil::Date, month: i8, day: i8) -> Option<jiff::civil::Date> {
    let mut year = today.year();
    loop {
        if let Ok(candidate) = jiff::civil::Date::new(year, month, day)
            && candidate >= today
        {
            return Some(candidate);
        }
        if year == 9999 {
            return None;
        }
        year += 1;
    }
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
    match select_due_for_new_task(explicit, no_due, project_default, global_default) {
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
    fn canonical_dates_normalize_other_jiff_spellings() {
        assert_eq!(canonical_due_date("20260810").unwrap(), "2026-08-10");
        assert_eq!(canonical_due_date("+002026-08-10").unwrap(), "2026-08-10");
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
        assert_eq!(resolve_due("7d", d("2026-08-05")).unwrap(), "2026-08-12");
        assert_eq!(resolve_due("2W", d("2026-08-05")).unwrap(), "2026-08-19");
        assert_eq!(resolve_due("+0d", d("2026-08-05")).unwrap(), "2026-08-05");
    }

    #[test]
    fn named_month_days_resolve_to_the_next_occurrence() {
        assert_eq!(resolve_due("aug10", d("2026-08-05")).unwrap(), "2026-08-10");
        assert_eq!(
            resolve_due("AUG 10", d("2026-08-10")).unwrap(),
            "2026-08-10"
        );
        assert_eq!(resolve_due("aug10", d("2026-08-11")).unwrap(), "2027-08-10");
        assert_eq!(resolve_due("feb29", d("2026-01-01")).unwrap(), "2028-02-29");
    }

    #[test]
    fn offsets_cross_month_and_year_boundaries() {
        assert_eq!(resolve_due("+1d", d("2026-12-31")).unwrap(), "2027-01-01");
        assert_eq!(resolve_due("+1d", d("2024-02-28")).unwrap(), "2024-02-29");
    }

    #[test]
    fn a_resolved_value_is_always_fixed_width() {
        for spec in ["today", "tomorrow", "+1d", "40w", "aug10"] {
            let got = resolve_due(spec, d("2026-01-01")).unwrap();
            assert_eq!(got.len(), 10, "{spec} -> {got}");
            assert!(got.parse::<jiff::civil::Date>().is_ok(), "{spec} -> {got}");
        }
    }

    #[test]
    fn nonsense_is_rejected() {
        for spec in [
            "",
            "   ",
            "banana",
            "+d",
            "+7",
            "+7y",
            "-7d",
            "+7 d",
            "2026-02-31",
            "10/8",
            "aug 1 0",
        ] {
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
