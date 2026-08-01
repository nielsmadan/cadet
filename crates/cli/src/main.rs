mod config;

use cadet_app::{App, GitNet};
use cadet_backend_fs::FsBackend;
use cadet_core::{ProjectConfig, TaskKey};
use cadet_store_sqlite::SqliteIndex;
use clap::{Parser, Subcommand};
use config::{Project, Registry};

#[derive(Parser)]
#[command(name = "cadet", version, about = "Tasks that live in your files")]
struct Cli {
    #[arg(long, global = true)]
    project: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a project in an existing folder
    Init {
        path: String,
        #[arg(long)]
        prefix: String,
        #[arg(long)]
        name: String,
        /// Overwrite an existing project.toml at this path
        #[arg(long)]
        force: bool,
    },
    /// Add a task
    Add { title: Vec<String> },
    /// List tasks
    Ls {
        #[arg(long)]
        all: bool,
    },
    /// Show one task
    Show { key: String },
    /// Mark a task done
    Done { key: String },
    /// Move a task to a state
    Mv { key: String, state: String },
    /// Remove a task
    Rm { key: String },
    /// Adopt every pending hand-written note immediately
    Adopt,
    /// Report quarantined tasks
    Doctor,
    /// Revert the last change
    Undo,
}

const TEMPLATE: &str = r#"[project]
id = "{id}"
name = "{name}"
prefix = "{prefix}"

[tasks]
match = "frontmatter"

[workflow]
states = ["todo", "doing", "blocked", "done"]
initial = "todo"
terminal = ["done"]
"#;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn known_projects(reg: &Registry) -> String {
    if reg.projects.is_empty() {
        return "(none)".to_string();
    }
    reg.projects
        .iter()
        .map(|p| p.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn open_app(reg: &Registry, p: &Project) -> Result<App, Box<dyn std::error::Error>> {
    let backend = FsBackend::new(p.path.clone());
    let index = SqliteIndex::open(&reg.index_path())?;
    let git = GitNet::new(reg.repo_dir(&p.id), p.path.clone());
    git.ensure_init()?;
    Ok(App::new(Box::new(backend), index, git, p.id.clone()))
}

fn parse_key(app: &App, raw: &str) -> Result<TaskKey, Box<dyn std::error::Error>> {
    if let Some(k) = TaskKey::parse(raw) {
        return Ok(k);
    }
    let tasks = app.list(true)?;
    let lower = raw.to_lowercase();
    let title_matches: Vec<_> = tasks
        .iter()
        .filter(|t| t.title.to_lowercase().starts_with(&lower))
        .collect();
    let numeric_match = raw
        .parse::<u32>()
        .ok()
        .and_then(|n| tasks.iter().find(|t| t.key.number == n));

    // A bare number is ambiguous when it also matches a title prefix — e.g.
    // `show 1` could mean key number 1 or a task literally titled `1`.
    // Silently preferring the numeric match would make the title-matching
    // task permanently unreachable by this path.
    if let Some(t) = numeric_match {
        if title_matches.is_empty() {
            return Ok(t.key.clone());
        }
        return Err(format!(
            "`{raw}` is ambiguous — it matches key {} and {} task title(s) starting with `{raw}`; use the full key (e.g. `{}`) to disambiguate",
            t.key,
            title_matches.len(),
            t.key
        )
        .into());
    }

    match title_matches.len() {
        1 => Ok(title_matches[0].key.clone()),
        0 => Err(format!("no task matching `{raw}`").into()),
        n => Err(format!("`{raw}` matches {n} tasks — be more specific").into()),
    }
}

/// The library deliberately never prints — a caller (here, the CLI) drains
/// whatever a mutating command queued and decides how to show it.
fn print_warnings(app: &App) {
    for w in app.drain_warnings() {
        eprintln!("⚠ {w}");
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut reg = Registry::load()?;

    if let Some(Cmd::Init {
        path,
        prefix,
        name,
        force,
    }) = &cli.cmd
    {
        let root = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&root)?;
        let project_toml = root.join("project.toml");
        // Re-running `init` on an already-initialised folder must not
        // silently clobber it: a hand-edited `project.toml` (custom fields,
        // a tweaked workflow) would be destroyed with no warning. Require
        // an explicit `--force` to overwrite.
        if project_toml.exists() && !force {
            return Err(format!(
                "{} already exists — pass --force to overwrite it",
                project_toml.display()
            )
            .into());
        }
        let id = name.to_lowercase().replace(' ', "-");
        let body = TEMPLATE
            .replace("{id}", &id)
            .replace("{name}", name)
            .replace("{prefix}", prefix);
        // `init` writes this file directly from a template rather than
        // round-tripping through `ProjectConfig`, so it doesn't get that
        // type's validation for free. Parse what we're about to write and
        // fail loudly instead of leaving behind a `project.toml` that looks
        // fine but can't be read back — an empty prefix, for instance,
        // renders keys as `-1` and breaks task identity silently.
        ProjectConfig::parse(&body)
            .map_err(|e| format!("generated project.toml would not parse: {e}"))?;
        std::fs::write(&project_toml, body)?;
        // Upsert, not append: re-running `init` (with --force) for a project
        // that's already registered must replace its entry, not add a
        // second `[projects.<id>]` table — two tables with the same key is
        // invalid TOML and corrupts the registry for every project in it.
        reg.upsert_project(Project {
            id: id.clone(),
            path: root.clone(),
        });
        if reg.default.is_none() {
            reg.default = Some(id.clone());
        }
        reg.save()?;
        println!("created project `{id}` at {}", root.display());
        return Ok(());
    }

    let project = match cli.project.as_deref() {
        Some(id) => reg
            .projects
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "unknown project `{id}` — configured project(s): {}",
                    known_projects(&reg)
                )
            })?,
        None => reg.active(None).cloned().ok_or_else(|| {
            if reg.projects.is_empty() {
                "no project configured — run `cadet init <path> --prefix X --name Y`".to_string()
            } else {
                format!(
                    "no default project set — pass --project or set one, configured project(s): {}",
                    known_projects(&reg)
                )
            }
        })?,
    };
    let app = open_app(&reg, &project)?;

    let now = jiff_now_ms();
    let report = app.reconcile(now)?;
    if report.scan_rejected {
        eprintln!("⚠ scan rejected: an unexpectedly large number of tasks disappeared.");
        eprintln!("  Nothing was deleted. Check that your sync tool has finished.");
    }
    if report.pending_adoption > 0 {
        eprintln!(
            "⚠ {} note(s) ready to adopt — run `cadet adopt`",
            report.pending_adoption
        );
    }

    match cli.cmd.unwrap_or(Cmd::Ls { all: false }) {
        Cmd::Init { .. } => unreachable!("handled above"),
        Cmd::Add { title } => {
            let t = app.add(&title.join(" "))?;
            println!("{}  {}", t.key, t.title);
            print_warnings(&app);
        }
        Cmd::Ls { all } => {
            let tasks = app.list(all)?;
            if tasks.is_empty() {
                println!("no tasks");
            }
            for t in tasks {
                println!("{:<10} {:<8} {}", t.key.to_string(), t.state, t.title);
            }
        }
        Cmd::Show { key } => {
            let k = parse_key(&app, &key)?;
            let t = app.get_by_key(&k)?;
            println!("{}  {}", t.key, t.title);
            println!("state: {}", t.state);
            if let Some(d) = &t.due {
                println!("due:   {d}");
            }
            if !t.tags.is_empty() {
                println!("tags:  {}", t.tags.join(", "));
            }
            if !t.body.trim().is_empty() {
                println!("\n{}", t.body.trim());
            }
        }
        Cmd::Done { key } => {
            let k = parse_key(&app, &key)?;
            app.set_state(&k, "done")?;
            println!("{k} done");
            print_warnings(&app);
        }
        Cmd::Mv { key, state } => {
            let k = parse_key(&app, &key)?;
            app.set_state(&k, &state)?;
            println!("{k} -> {state}");
            print_warnings(&app);
        }
        Cmd::Rm { key } => {
            let k = parse_key(&app, &key)?;
            app.delete(&k)?;
            println!("{k} removed");
            print_warnings(&app);
        }
        Cmd::Adopt => {
            let r = app.adopt_pending(now)?;
            println!("adopted {} note(s)", r.adopted);
            print_warnings(&app);
        }
        Cmd::Doctor => {
            println!(
                "adopted: {}  pending: {}",
                report.adopted, report.pending_adoption
            );
            println!("pending deletions: {}", report.pending_deletion);
            if report.scan_rejected {
                println!("scan rejected — see the warning above");
            }
        }
        Cmd::Undo => {
            app.undo()?;
            // `undo` rewrites the work tree wholesale via `git reset --hard`,
            // not the kind of transient absence the reconcile grace period
            // exists to protect against. Reconcile immediately, treating any
            // uid observed absent for the first time as deleted at once
            // rather than pending — but ONLY a fresh absence: a task that
            // was already mid pending-deletion for an unrelated reason keeps
            // its own grace period untouched. `clear_index` would wipe that
            // unrelated task's tracking wholesale, dropping it from `list()`
            // without ever formally deleting it.
            app.reconcile_after_undo(now)?;
            println!("reverted");
            print_warnings(&app);
        }
    }
    Ok(())
}

fn jiff_now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}
