use crate::TEMPLATE;
use crate::config::{BackendKind, Project, Registry};
use crate::prompt;
use cadet_backend_local_db::LocalDbBackend;
use cadet_core::ProjectConfig;
use clap::Subcommand;
use std::path::PathBuf;

/// The `--backend` flag's own type, kept separate from `config::BackendKind`
/// the same way `PriorityArg` is kept separate from `cadet_core::Priority` in
/// `main.rs`: clap derives `--help`'s choice list and the "invalid value"
/// rejection straight off this enum, so the type clap renders from must be
/// the one the flag is declared with, not one converted into later.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum BackendArg {
    Markdown,
    LocalDb,
}

impl From<BackendArg> for BackendKind {
    fn from(b: BackendArg) -> Self {
        match b {
            BackendArg::Markdown => BackendKind::Markdown,
            BackendArg::LocalDb => BackendKind::LocalDb,
        }
    }
}

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Register a project and create its store: a folder of notes for
    /// `markdown`, a single database file for `local-db`
    Add {
        id: String,
        /// Where this project's tasks live: `markdown` (a folder of files)
        /// or `local-db` (a single SQLite file). Defaults to `markdown`.
        #[arg(long, value_enum, ignore_case = true)]
        backend: Option<BackendArg>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Overwrite an existing project.toml at this path
        #[arg(long)]
        force: bool,
        /// Use the path even when it already holds many notes
        #[arg(long)]
        yes: bool,
    },
    /// List configured projects
    Ls,
    /// Set the default project
    Use { id: String },
    /// Forget a project. Never deletes its files.
    Rm { id: String },
    /// Show or set the folder new projects are offered under
    Root { path: Option<String> },
}

pub fn derive_prefix(id: &str) -> String {
    let cleaned: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    cleaned.to_ascii_uppercase().chars().take(4).collect()
}

pub fn derive_name(id: &str) -> String {
    let mut c = id.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Expands a leading `~` or `~/…`. Deliberately does not resolve `~user/…` —
/// that names a different account's home directory, which cadet has no way
/// to look up, and silently creating a directory literally named `~user`
/// would be the same bug this function exists to fix, one character wider.
pub fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home_dir().map(|h| h.join(rest));
    }
    if raw.starts_with('~') {
        return Err(format!(
            "`{raw}` looks like another user's home directory — cadet can't expand `~user`; pass an absolute path instead"
        ));
    }
    Ok(PathBuf::from(raw))
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set — cannot expand `~`".to_string())
}

/// Tilde-expands, then makes the result absolute against the current
/// directory. A relative path stored in the registry only works from the
/// directory it was typed in — the registry is the one piece of local state
/// that is not disposable (see `config.rs`), so an entry that stops
/// resolving the moment `cadet` runs from elsewhere defeats the point of it.
pub fn resolve_path(raw: &str) -> Result<PathBuf, String> {
    let expanded = expand_tilde(raw)?;
    std::path::absolute(&expanded).map_err(|e| format!("could not resolve `{raw}`: {e}"))
}

/// Above this many existing notes, a folder is almost certainly somebody's
/// whole document collection rather than an empty place for tasks.
const MANY_NOTES: usize = 50;

/// Counts markdown files under `root`, stopping as soon as `limit` is
/// exceeded — the folder this guard exists to catch is `$HOME`, and a full
/// walk of it is not something to do before printing a warning.
///
/// Mirrors `MarkdownBackend::markdown_files` exactly, dot-entry skip included: a
/// count that disagreed with what adoption actually sees would be worse
/// than no count at all.
fn count_markdown(root: &std::path::Path, limit: usize) -> usize {
    let mut found = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                found += 1;
                if found > limit {
                    return found;
                }
            }
        }
    }
    found
}

/// The `project.toml` body to write for `id`/`name`/`prefix`.
///
/// When `existing` parses as TOML — which is all this needs, and
/// deliberately weaker than "is a valid config" — its *parsed document* is
/// mutated and only those three keys are rewritten: `[[fields]]`, a
/// customised `[workflow]`, `[tasks]` include/exclude, comments and unknown
/// keys all survive. Re-emitting `TEMPLATE` instead silently redefines what a task
/// is: every value written under a dropped declaration becomes unreachable,
/// and every task sitting in a dropped state becomes unmovable.
///
/// This is deliberately the same treatment `Registry::save` gives the
/// registry — parse, mutate, write — because it is the same bug shape, and
/// this codebase keeps growing new instances of it.
///
/// `None` — no file at all, or one that is not even TOML — falls back to the
/// template, since there is then nothing to preserve.
pub fn render_project_toml(existing: Option<&str>, id: &str, name: &str, prefix: &str) -> String {
    let mut doc: toml_edit::DocumentMut = existing
        .and_then(|src| src.parse().ok())
        .unwrap_or_else(|| TEMPLATE.parse().expect("TEMPLATE must be valid TOML"));
    if !doc.get("project").is_some_and(|p| p.is_table()) {
        doc["project"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["project"]["id"] = toml_edit::value(id);
    doc["project"]["name"] = toml_edit::value(name);
    doc["project"]["prefix"] = toml_edit::value(prefix);
    doc.to_string()
}

pub fn run(cmd: ProjectCmd, mut reg: Registry) -> Result<(), String> {
    match cmd {
        ProjectCmd::Ls => list(&reg),
        ProjectCmd::Root { path } => match path {
            None => {
                match reg.project_root() {
                    Some(p) => println!("{}", p.display()),
                    None => println!("(not set) — pass a path to set it"),
                }
                Ok(())
            }
            Some(p) => {
                let resolved = resolve_path(&p)?;
                reg.set_project_root(resolved.clone());
                reg.save().map_err(|e| e.to_string())?;
                println!("project root set to {}", resolved.display());
                Ok(())
            }
        },
        ProjectCmd::Use { id } => {
            reg.set_default(&id)?;
            reg.save().map_err(|e| e.to_string())?;
            println!("default project is now `{id}`");
            Ok(())
        }
        ProjectCmd::Rm { id } => {
            let was_default = reg.default.as_deref() == Some(id.as_str());
            if !reg.remove_project(&id) {
                return Err(format!(
                    "unknown project `{id}` — configured project(s): {}",
                    reg.known_projects()
                ));
            }
            reg.save().map_err(|e| e.to_string())?;
            if !was_default {
                println!("forgot project `{id}` — its files were not touched");
            } else {
                match &reg.default {
                    Some(new_default) => println!(
                        "forgot project `{id}` — its files were not touched; default project is now `{new_default}`"
                    ),
                    None => println!(
                        "forgot project `{id}` — its files were not touched; no default project is set"
                    ),
                }
            }
            Ok(())
        }
        ProjectCmd::Add {
            id,
            backend,
            path,
            prefix,
            name,
            force,
            yes,
        } => add(
            &mut reg,
            NewProject {
                id,
                backend,
                path,
                prefix,
                name,
                force,
                yes,
            },
        ),
    }
}

/// `ProjectCmd::Add`'s fields, bundled for `add` — one struct rather than
/// seven positional arguments, which is also what clippy's
/// `too_many_arguments` lint is for.
struct NewProject {
    id: String,
    backend: Option<BackendArg>,
    path: Option<String>,
    prefix: Option<String>,
    name: Option<String>,
    force: bool,
    yes: bool,
}

fn list(reg: &Registry) -> Result<(), String> {
    if reg.projects.is_empty() {
        println!("no projects — run `cadet project add <id>`");
        return Ok(());
    }
    for p in &reg.projects {
        let marker = if reg.default.as_deref() == Some(p.id.as_str()) {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {:<12} {:<9} {}",
            p.id,
            p.backend.as_str(),
            p.path.display()
        );
    }
    Ok(())
}

/// Resolves each value in this order: the flag, then a prompt when
/// interactive, then a hard error naming the flag — the clig.dev rule that a
/// missing argument may prompt but must never *require* a prompt. `path` is
/// the one exception that isn't an error outside a TTY: with a project root
/// configured, `<root>/<id>/tasks` is exactly as good a default there as it
/// is inside a prompt, and treating the two modes differently would make
/// `project_root` mean two different things depending on `isatty(0)`.
fn add(reg: &mut Registry, new: NewProject) -> Result<(), String> {
    let NewProject {
        id,
        backend,
        path,
        prefix,
        name,
        force,
        yes,
    } = new;
    let interactive = prompt::is_interactive();

    // Resolved before anything else that follows: it changes what the path
    // default means (a folder of notes vs. a single database file) and
    // where the generated config is written, so every later default has to
    // be computed after this, not before it.
    let backend: BackendKind = match backend {
        Some(b) => b.into(),
        None if interactive => {
            let got = prompt::ask("backend", Some(BackendKind::Markdown.as_str()))
                .map_err(|e| e.to_string())?;
            BackendKind::parse(&got).ok_or_else(|| {
                format!("unknown backend `{got}` — expected `markdown` or `local-db`")
            })?
        }
        None => BackendKind::Markdown,
    };

    let default_path = match backend {
        BackendKind::Markdown => reg
            .project_root()
            .map(|r| r.join(&id).join("tasks").display().to_string()),
        // No project root involved: a local-db project's storage is a file
        // under the registry's own home, not under the (markdown-only)
        // notes root a user may have configured with `cadet project root`.
        BackendKind::LocalDb => Some(
            reg.root
                .join("projects")
                .join(format!("{id}.db"))
                .display()
                .to_string(),
        ),
    };

    let path = match (path, interactive) {
        (Some(p), _) => p,
        (None, true) => {
            let got = prompt::ask("path", default_path.as_deref()).map_err(|e| e.to_string())?;
            if got.is_empty() {
                return Err("a path is required — pass --path".into());
            }
            got
        }
        (None, false) => default_path.ok_or_else(|| {
            "a path is required — pass --path, or set a root with `cadet project root <dir>`"
                .to_string()
        })?,
    };

    let root = resolve_path(&path)?;
    // `Project::config_path` is the single place that knows where a
    // project's config lives for each backend — shared with `load_config`
    // in `main.rs` so the two can't drift apart. `id` doesn't matter for
    // this lookup, only `path` and `backend` do, so it's fine to derive it
    // from a throwaway value ahead of the real `Project` built below.
    let project_toml = Project {
        id: id.clone(),
        path: root.clone(),
        backend,
    }
    .config_path();

    // Read the config being overwritten (if any) before it's gone, so a
    // `--force` re-add without explicit --prefix/--name re-derives from the
    // id's default rather than silently swapping the project's real prefix
    // out from under it. Under the old `init` this was impossible — prefix
    // and name were required flags, so an overwrite could only ever write
    // what the user typed. Making them optional here turned a bare `--force`
    // into "silently re-derive", which splits one project's tasks across two
    // key namespaces (`ALFA-*` and `ALPH-*`) that `doctor` has no way to see
    // are the same project.
    let existing_src = if project_toml.exists() {
        if !force {
            return Err(format!(
                "{} already exists — pass --force to overwrite it",
                project_toml.display()
            ));
        }
        std::fs::read_to_string(&project_toml).ok()
    } else {
        None
    };
    // Only used for the prefix/name defaults below, which need the *values*
    // a config carries. Whether the document is preserved is a separate
    // question with a wider answer — see the `render_project_toml` call.
    let existing = existing_src
        .as_deref()
        .and_then(|body| ProjectConfig::parse(body).ok());
    // `--path '~'` expands correctly and then roots a project at the user's
    // home directory, where every `.md` underneath becomes an adoption
    // candidate. `~` and `~/` are plausible things to type, so the
    // consequence gets a gate. Skipped when the folder is already a cadet
    // project: its own task files are exactly what we would be counting.
    // Also skipped outright for `local-db`: `root` there is a single
    // database file, not a folder of notes, so there is nothing for this
    // guard to count.
    if backend == BackendKind::Markdown && existing_src.is_none() {
        let found = count_markdown(&root, MANY_NOTES);
        if found > MANY_NOTES {
            let question = format!(
                "{} already holds more than {MANY_NOTES} markdown files — every `.md` under a project root becomes a task. Use it anyway?",
                root.display()
            );
            let approved = if yes {
                true
            } else if interactive {
                prompt::confirm(&question).map_err(|e| e.to_string())?
            } else {
                false
            };
            if !approved {
                return Err(format!(
                    "{} already holds more than {MANY_NOTES} markdown files — every `.md` under a project root becomes a task. Point --path at a subfolder, or pass --yes to use it anyway.",
                    root.display()
                ));
            }
        }
    }

    let default_prefix = existing
        .as_ref()
        .map(|c| c.prefix.clone())
        .unwrap_or_else(|| derive_prefix(&id));
    let default_name = existing
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| derive_name(&id));

    let prefix = match prefix {
        Some(p) => p,
        None if interactive => {
            prompt::ask("prefix", Some(&default_prefix)).map_err(|e| e.to_string())?
        }
        None => default_prefix,
    };

    let name = match name {
        Some(n) => n,
        None if interactive => {
            prompt::ask("name", Some(&default_name)).map_err(|e| e.to_string())?
        }
        None => default_name,
    };

    // A degenerate id (no ASCII alphanumerics, e.g. `日本` or `---`) derives
    // an empty prefix. Catching it here, by name, beats letting it fall
    // through to `ProjectConfig::parse`'s generic "project prefix must not
    // be empty" — that message doesn't say whose input caused it or how to
    // fix it.
    if prefix.trim().is_empty() {
        return Err(format!("id `{id}` yields no usable prefix — pass --prefix"));
    }

    // Creates the directory, renders the body (preserving an existing
    // config's declarations — see `render_project_toml`), and validates the
    // result by parsing it (the check that catches a whitespace-only prefix
    // typed explicitly, since the empty-derived case is caught above).
    //
    // The source is handed over whenever it is TOML at all, NOT only when it
    // is a valid config: those are different sets, and the second is much
    // larger — an enum spelling its options key wrong is valid TOML and an
    // invalid config. Withholding the source there would make `--force`, the
    // repair command, eat exactly the files that need repairing. Nothing
    // unsafe follows: the validation on the next line runs before the write,
    // so a config that is still invalid after preservation errors and
    // touches nothing.
    match backend {
        BackendKind::Markdown => {
            std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        }
        BackendKind::LocalDb => {
            if let Some(parent) = root.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            // Opens (creating the file if absent) and runs its schema
            // migration now, so a bad `--path` is caught here rather than on
            // the first `cadet ls` against this project.
            LocalDbBackend::open(&root).map_err(|e| e.to_string())?;
        }
    }
    let body = render_project_toml(existing_src.as_deref(), &id, &name, &prefix);
    ProjectConfig::parse(&body)
        .map_err(|e| format!("generated project.toml would not parse: {e}"))?;
    std::fs::write(&project_toml, body).map_err(|e| e.to_string())?;
    reg.upsert_project(Project {
        id: id.clone(),
        path: root.clone(),
        backend,
    });
    if reg.default.is_none() {
        reg.default = Some(id.clone());
    }
    reg.save().map_err(|e| e.to_string())?;
    println!("created project `{id}` at {}", root.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_prefix_uppercases_and_truncates_to_four() {
        assert_eq!(derive_prefix("juggler"), "JUGG");
    }

    #[test]
    fn derive_prefix_drops_non_alphanumerics() {
        assert_eq!(derive_prefix("my-app"), "MYAP");
    }

    #[test]
    fn derive_prefix_of_a_degenerate_id_is_empty() {
        assert_eq!(derive_prefix("---"), "");
        assert_eq!(derive_prefix("日本"), "");
    }

    #[test]
    fn derive_name_capitalizes_the_first_character() {
        assert_eq!(derive_name("juggler"), "Juggler");
    }

    #[test]
    fn derive_name_of_an_empty_id_is_empty() {
        assert_eq!(derive_name(""), "");
    }

    #[test]
    fn expand_tilde_expands_a_leading_tilde_slash() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/x").unwrap(), PathBuf::from(home).join("x"));
    }

    #[test]
    fn expand_tilde_expands_a_bare_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~").unwrap(), PathBuf::from(home));
    }

    #[test]
    fn expand_tilde_rejects_another_users_home() {
        assert!(expand_tilde("~other/x").is_err());
        assert!(expand_tilde("~other").is_err());
    }

    #[test]
    fn expand_tilde_leaves_an_ordinary_path_alone() {
        assert_eq!(expand_tilde("/tmp/x").unwrap(), PathBuf::from("/tmp/x"));
    }

    #[test]
    fn expand_tilde_leaves_a_relative_path_alone() {
        assert_eq!(
            expand_tilde("relative/x").unwrap(),
            PathBuf::from("relative/x")
        );
    }

    const CUSTOMISED: &str = r#"# hand-written
[project]
id = "old"
name = "Old"
prefix = "OLD"

[tasks]
match = "frontmatter"
exclude = ["archive/**"]

[workflow]
states = ["todo", "review", "done"]
initial = "todo"
terminal = ["done"]

[[fields]]
name = "estimate"
type = "int"
"#;

    #[test]
    fn render_rewrites_only_id_name_and_prefix() {
        let got = render_project_toml(Some(CUSTOMISED), "new", "New", "NEW");
        assert!(got.contains(r#"id = "new""#), "{got}");
        assert!(got.contains(r#"name = "New""#), "{got}");
        assert!(got.contains(r#"prefix = "NEW""#), "{got}");
        assert!(got.contains("estimate"), "{got}");
        assert!(got.contains("review"), "{got}");
        assert!(got.contains("archive/**"), "{got}");
        assert!(got.contains("# hand-written"), "{got}");
    }

    #[test]
    fn render_without_an_existing_config_uses_the_template() {
        let got = render_project_toml(None, "fresh", "Fresh", "FR");
        let cfg = ProjectConfig::parse(&got).unwrap();
        assert_eq!(cfg.id, "fresh");
        assert_eq!(cfg.prefix, "FR");
        assert!(cfg.fields.is_empty());
        assert!(
            !got.contains("{id}"),
            "placeholders must be replaced: {got}"
        );
    }

    #[test]
    fn render_supplies_a_project_table_when_the_existing_one_has_none() {
        let got = render_project_toml(Some("[workflow]\nstates = [\"a\"]\n"), "x", "X", "XX");
        assert!(got.contains(r#"prefix = "XX""#), "{got}");
    }

    #[test]
    fn count_markdown_stops_once_the_limit_is_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("n{i}.md")), "x").unwrap();
        }
        assert_eq!(count_markdown(dir.path(), 3), 4);
        assert_eq!(count_markdown(dir.path(), 100), 10);
    }

    #[test]
    fn count_markdown_recurses_but_skips_dot_entries_and_other_extensions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("a.md"), "x").unwrap();
        std::fs::write(dir.path().join("sub/b.md"), "x").unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), "x").unwrap();
        std::fs::write(dir.path().join(".hidden/d.md"), "x").unwrap();
        std::fs::write(dir.path().join(".e.md"), "x").unwrap();
        assert_eq!(count_markdown(dir.path(), 100), 2);
    }

    #[test]
    fn count_markdown_of_a_missing_folder_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_markdown(&dir.path().join("nope"), 100), 0);
    }

    #[test]
    fn the_template_is_valid_toml() {
        assert!(TEMPLATE.parse::<toml_edit::DocumentMut>().is_ok());
    }

    #[test]
    fn resolve_path_makes_a_relative_path_absolute() {
        let got = resolve_path("relative/x").unwrap();
        assert!(got.is_absolute(), "{got:?}");
    }

    #[test]
    fn resolve_path_leaves_an_absolute_path_absolute() {
        let got = resolve_path("/tmp/x").unwrap();
        assert_eq!(got, PathBuf::from("/tmp/x"));
    }
}
