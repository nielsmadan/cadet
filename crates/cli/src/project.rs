use crate::TEMPLATE;
use crate::config::{Project, Registry};
use crate::prompt;
use cadet_core::ProjectConfig;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Register a project and create its folder
    Add {
        id: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Overwrite an existing project.toml at this path
        #[arg(long)]
        force: bool,
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
            path,
            prefix,
            name,
            force,
        } => add(&mut reg, id, path, prefix, name, force),
    }
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
        println!("{marker} {:<12} {}", p.id, p.path.display());
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
fn add(
    reg: &mut Registry,
    id: String,
    path: Option<String>,
    prefix: Option<String>,
    name: Option<String>,
    force: bool,
) -> Result<(), String> {
    let interactive = prompt::is_interactive();

    let default_path = reg
        .project_root()
        .map(|r| r.join(&id).join("tasks").display().to_string());

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
    let project_toml = root.join("project.toml");

    // Read the config being overwritten (if any) before it's gone, so a
    // `--force` re-add without explicit --prefix/--name re-derives from the
    // id's default rather than silently swapping the project's real prefix
    // out from under it. Under the old `init` this was impossible — prefix
    // and name were required flags, so an overwrite could only ever write
    // what the user typed. Making them optional here turned a bare `--force`
    // into "silently re-derive", which splits one project's tasks across two
    // key namespaces (`ALFA-*` and `ALPH-*`) that `doctor` has no way to see
    // are the same project.
    let existing = if project_toml.exists() {
        if !force {
            return Err(format!(
                "{} already exists — pass --force to overwrite it",
                project_toml.display()
            ));
        }
        std::fs::read_to_string(&project_toml)
            .ok()
            .and_then(|body| ProjectConfig::parse(&body).ok())
    } else {
        None
    };
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

    // Moved (mostly) verbatim from the old `Init` handler: creates the
    // directory, writes TEMPLATE with id/name/prefix substituted, and
    // validates the result by parsing it (the check that catches a
    // whitespace-only prefix typed explicitly, since the empty-derived case
    // is now caught above).
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let body = TEMPLATE
        .replace("{id}", &id)
        .replace("{name}", &name)
        .replace("{prefix}", &prefix);
    ProjectConfig::parse(&body)
        .map_err(|e| format!("generated project.toml would not parse: {e}"))?;
    std::fs::write(&project_toml, body).map_err(|e| e.to_string())?;
    reg.upsert_project(Project {
        id: id.clone(),
        path: root.clone(),
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
