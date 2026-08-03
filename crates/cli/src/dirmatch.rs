use crate::config::{Project, Registry};
use std::path::{Component, Path, PathBuf};

/// Why a project was selected. The CLI prints a note only for `Dir`, so
/// everyone not using directory patterns sees exactly what they saw before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    Dir(PathBuf),
    Default,
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Flag => "--project".into(),
            Source::Env => "CADET_PROJECT".into(),
            Source::Dir(p) => format!("cwd matches `{}`", p.display()),
            Source::Default => "the default project".into(),
        }
    }
}

pub struct Selection {
    pub project: Project,
    pub source: Source,
}

/// Does `pattern` cover `cwd`?
///
/// `*` matches within one path component, `**` matches zero or more whole
/// components. A pattern that does not already end in `**` gets one appended,
/// so configuring a repository root also covers everything inside it — the
/// git mental model, and what makes this usable rather than fiddly.
///
/// Symlinks are resolved on both sides where possible, because ignoring them
/// breaks the feature outright: on macOS `/tmp` and `/var` are symlinks into
/// `/private`, so `current_dir()` reports `/private/tmp/x` for a pattern the
/// user wrote as `/tmp/x`. A wildcard pattern cannot be canonicalised, and a
/// path that no longer exists cannot either, so each side falls back to its
/// literal form and a match against any combination counts.
pub fn matches(pattern: &Path, cwd: &Path) -> bool {
    let pats = variants(pattern);
    let dirs = variants(cwd);
    pats.iter()
        .any(|p| dirs.iter().any(|d| matches_lexically(p, d)))
}

/// A path and its canonical form, deduplicated. Canonicalising is skipped for
/// anything containing a wildcard — there is no file to resolve.
fn variants(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf()];
    if !p.to_string_lossy().contains('*')
        && let Ok(c) = p.canonicalize()
        && c != *p
    {
        out.push(c);
    }
    out
}

fn matches_lexically(pattern: &Path, cwd: &Path) -> bool {
    let pat: Vec<String> = components(pattern);
    let dir: Vec<String> = components(cwd);
    let implicit_tail = pat.last().map(|s| s != "**").unwrap_or(true);
    match_from(&pat, &dir, implicit_tail)
}

fn components(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            Component::RootDir => Some("/".to_string()),
            // `.` and `..` are dropped rather than compared: a trailing slash
            // or a `./` prefix must never cause a false mismatch.
            _ => None,
        })
        .collect()
}

/// Backtracking match of `pat` against `dir`. `implicit_tail` means the
/// pattern is treated as if it ended in `**`.
fn match_from(pat: &[String], dir: &[String], implicit_tail: bool) -> bool {
    match pat.split_first() {
        None => implicit_tail || dir.is_empty(),
        Some((head, rest)) if head == "**" => {
            // `**` consumes any number of components, including none.
            (0..=dir.len()).any(|skip| match_from(rest, &dir[skip..], implicit_tail))
        }
        Some((head, rest)) => match dir.split_first() {
            None => false,
            Some((d, drest)) if segment_matches(head, d) => match_from(rest, drest, implicit_tail),
            _ => false,
        },
    }
}

/// `*` inside one component. Anchored at both ends, so a pattern for `cadet`
/// never matches a sibling `cadet-old`.
fn segment_matches(pat: &str, seg: &str) -> bool {
    if !pat.contains('*') {
        return pat == seg;
    }
    let parts: Vec<&str> = pat.split('*').collect();
    let mut rest = seg;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match i {
            0 => match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            },
            _ if i == parts.len() - 1 => return rest.ends_with(part),
            _ => match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            },
        }
    }
    true
}

/// How specific a pattern is, for ranking two projects that both match:
/// literal components before the first wildcard, then total components.
/// A deeper, more literal pattern wins.
fn specificity(pattern: &Path) -> (usize, usize) {
    let parts = components(pattern);
    let literal = parts.iter().take_while(|c| !c.contains('*')).count();
    (literal, parts.len())
}

/// The single place project selection is decided. `main.rs` and
/// `cadet project which` both call this rather than each implementing the
/// precedence — two copies of one rule is the defect this codebase has
/// produced twenty-one times.
///
/// Precedence: `--project`, then `CADET_PROJECT`, then a directory match,
/// then the registry default. A flag the user typed always beats a directory
/// they merely happen to be standing in.
pub fn resolve(
    reg: &Registry,
    project_flag: Option<&str>,
    cwd: &Path,
) -> Result<Selection, String> {
    let requested = match project_flag {
        Some(id) => Some((id.to_string(), "", Source::Flag)),
        None => crate::config::env_project().map(|id| (id, " (from CADET_PROJECT)", Source::Env)),
    };
    if let Some((id, note, source)) = requested {
        let project = reg.find(&id).cloned().ok_or_else(|| {
            format!(
                "unknown project `{id}`{note} — configured project(s): {}",
                reg.known_projects()
            )
        })?;
        return Ok(Selection { project, source });
    }

    let mut hits: Vec<(&Project, &PathBuf, (usize, usize))> = reg
        .projects
        .iter()
        .flat_map(|p| p.dirs.iter().map(move |d| (p, d)))
        .filter(|(_, d)| matches(d, cwd))
        .map(|(p, d)| (p, d, specificity(d)))
        .collect();
    hits.sort_by(|a, b| b.2.cmp(&a.2));
    if let Some((best, pattern, rank)) = hits.first() {
        // A genuine tie between two different projects is ambiguous, and
        // guessing would silently write into the wrong one.
        let tied: Vec<&str> = hits
            .iter()
            .filter(|(p, _, r)| r == rank && p.id != best.id)
            .map(|(p, _, _)| p.id.as_str())
            .collect();
        if !tied.is_empty() {
            return Err(format!(
                "the current directory matches more than one project — `{}` and `{}`. \
                 Pass --project to choose, or make one pattern more specific.",
                best.id,
                tied.join("`, `")
            ));
        }
        return Ok(Selection {
            project: (*best).clone(),
            source: Source::Dir((*pattern).clone()),
        });
    }

    let project = reg.default_project().cloned().ok_or_else(|| {
        if reg.projects.is_empty() {
            "no project configured — run `cadet project add <id>`".to_string()
        } else {
            format!(
                "no default project set — pass --project or set one, configured project(s): {}",
                reg.known_projects()
            )
        }
    })?;
    Ok(Selection {
        project,
        source: Source::Default,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn an_exact_directory_matches() {
        assert!(matches(&p("/w/cadet"), &p("/w/cadet")));
    }

    #[test]
    fn a_descendant_matches_without_writing_a_wildcard() {
        assert!(matches(&p("/w/cadet"), &p("/w/cadet/crates/cli")));
    }

    #[test]
    fn a_parent_does_not_match() {
        assert!(!matches(&p("/w/cadet"), &p("/w")));
    }

    #[test]
    fn a_sibling_sharing_a_prefix_does_not_match() {
        assert!(!matches(&p("/w/cadet"), &p("/w/cadet-old")));
        assert!(!matches(&p("/w/cadet"), &p("/w/cadet-old/src")));
    }

    #[test]
    fn a_star_matches_within_one_component_only() {
        assert!(matches(&p("/w/*/tasks"), &p("/w/juggler/tasks")));
        assert!(!matches(&p("/w/*/tasks"), &p("/w/a/b/tasks")));
        assert!(matches(&p("/w/cad*"), &p("/w/cadet")));
        assert!(!matches(&p("/w/cad*"), &p("/w/other")));
    }

    #[test]
    fn a_double_star_crosses_components() {
        assert!(matches(&p("/w/**/tasks"), &p("/w/a/b/tasks")));
        assert!(matches(&p("/w/**/tasks"), &p("/w/tasks")));
        assert!(matches(&p("/w/**"), &p("/w/anything/at/all")));
    }

    #[test]
    fn trailing_slashes_and_dot_segments_do_not_break_a_match() {
        assert!(matches(&p("/w/cadet/"), &p("/w/cadet")));
        assert!(matches(&p("/w/cadet"), &p("/w/./cadet")));
    }

    #[test]
    fn specificity_prefers_the_deeper_more_literal_pattern() {
        assert!(specificity(&p("/w/cadet/crates")) > specificity(&p("/w/cadet")));
        assert!(specificity(&p("/w/cadet")) > specificity(&p("/w/*")));
    }
}
