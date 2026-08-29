use crate::TEMPLATE;
use crate::config::{BackendKind, Project, Registry};
use crate::prompt;
use cadet_backend_local_db::LocalDbBackend;
use cadet_backend_markdown::markdown_files_under;
use cadet_core::{BackendError, ProjectConfig, TaskFilter, TaskKey, Workflow};
use clap::Subcommand;
use std::path::{Path, PathBuf};

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
        /// A directory whose contents belong to this project. Running cadet
        /// from inside it selects this project automatically. Repeatable;
        /// `*` matches within one path segment, `**` across segments.
        #[arg(long = "dir")]
        dirs: Vec<String>,
    },
    /// Show or change the directories that select a project
    Dirs {
        id: String,
        /// Add a directory pattern. Repeatable.
        #[arg(long = "add")]
        add: Vec<String>,
        /// Remove a directory pattern by its exact stored value. Repeatable.
        #[arg(long = "rm")]
        rm: Vec<String>,
    },
    /// Report which project the current directory selects, and why
    Which,
    /// List configured projects
    Ls,
    /// Set the default project
    Use { id: String },
    /// Forget a project. Never deletes its files.
    Rm { id: String },
    /// Show or set the folder new projects are offered under
    Root { path: Option<String> },
    /// Show or change the states a task can be in
    State {
        #[command(subcommand)]
        cmd: StateCmd,
    },
}

/// The workflow's states are edited here rather than by hand so that removing
/// one can see the tasks still in it. A hand edit strands them: every write
/// path refuses a task whose state the workflow no longer declares, so it can
/// no longer be moved, finished, or edited at all. `cadet doctor` is the way
/// back from a hand edit that already happened.
#[derive(Subcommand)]
pub enum StateCmd {
    /// List the states, in order
    Ls { id: Option<String> },
    /// Add a state
    Add {
        state: String,
        id: Option<String>,
        /// Place it after this state. Defaults to last.
        #[arg(long, conflicts_with = "before")]
        after: Option<String>,
        /// Place it before this state
        #[arg(long)]
        before: Option<String>,
        /// Tasks in this state are complete, and hidden from `cadet ls`
        #[arg(long)]
        terminal: bool,
    },
    /// Remove a state
    Rm {
        state: String,
        id: Option<String>,
        /// Move tasks still in this state here first
        #[arg(long = "move-to")]
        move_to: Option<String>,
    },
    /// Rename a state, moving every task that holds it
    Rename {
        from: String,
        to: String,
        id: Option<String>,
    },
    /// Use this project's workflow for every project created from now on
    SetDefault { id: Option<String> },
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

/// Counts markdown files with the same walk `MarkdownBackend::markdown_files`
/// uses, so adoption's count cannot silently disagree with adoption's scan.
/// A missing root still counts as zero because that is the ordinary
/// `project add` case before the tasks directory exists.
fn count_markdown(root: &Path, limit: Option<usize>) -> Result<usize, String> {
    if !root.is_dir() {
        return Ok(0);
    }
    markdown_files_under(root, limit)
        .map(|v| v.len())
        .map_err(|e| unscannable(root, e))
}

/// Walks `root` to completion for its errors alone — a directory the scan
/// cannot read is the whole point here, not the count it returns.
fn ensure_scannable(root: &Path) -> Result<(), String> {
    count_markdown(root, None).map(|_| ())
}

fn unscannable(root: &Path, e: BackendError) -> String {
    let detail = match e {
        BackendError::Io(m) => m,
        other => other.to_string(),
    };
    format!(
        "cannot scan {}: {detail} — every `.md` under a project root becomes a task, \
         so a folder that cannot be counted cannot be adopted. \
         Check file permissions, or point --path elsewhere.",
        root.display()
    )
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
///
/// `default_workflow` is the registry's, applied only when there is no
/// existing config: on `--force` the file's own `[workflow]` is what the
/// project's tasks are already sitting in, and overwriting it is exactly the
/// "every task in a dropped state becomes unmovable" bug above.
pub fn render_project_toml(
    existing: Option<&str>,
    id: &str,
    name: &str,
    prefix: &str,
    default_workflow: Option<&Workflow>,
) -> String {
    let parsed: Option<toml_edit::DocumentMut> = existing.and_then(|src| src.parse().ok());
    let is_new = parsed.is_none();
    let mut doc = parsed.unwrap_or_else(|| TEMPLATE.parse().expect("TEMPLATE must be valid TOML"));
    if !doc.get("project").is_some_and(|p| p.is_table()) {
        doc["project"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["project"]["id"] = toml_edit::value(id);
    doc["project"]["name"] = toml_edit::value(name);
    doc["project"]["prefix"] = toml_edit::value(prefix);
    if let Some(wf) = default_workflow.filter(|_| is_new) {
        if !doc.get("workflow").is_some_and(|w| w.is_table()) {
            doc["workflow"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        wf.write_into(
            doc["workflow"]
                .as_table_mut()
                .expect("just ensured a table"),
        );
    }
    doc.to_string()
}

/// A project's `project.toml`, parsed, with its workflow lifted out. Every
/// `state` subcommand goes through this: read, mutate the `Workflow` value,
/// hand it back to `write_workflow`. Nothing formats a `[workflow]` table by
/// hand, and nothing re-derives the state rules.
struct WorkflowFile {
    project: Project,
    path: PathBuf,
    doc: toml_edit::DocumentMut,
    workflow: Workflow,
}

fn read_workflow(reg: &Registry, id: Option<&str>) -> Result<WorkflowFile, String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let project = crate::dirmatch::resolve(reg, id, &cwd)?.project;
    let path = project.config_path();
    let src = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let doc: toml_edit::DocumentMut = src
        .parse()
        .map_err(|e| format!("{} is not valid TOML: {e}", path.display()))?;
    let item = doc
        .get("workflow")
        .ok_or_else(|| format!("{} has no [workflow] table", path.display()))?;
    let workflow = Workflow::from_toml_item(item).map_err(|e| e.to_string())?;
    Ok(WorkflowFile {
        project,
        path,
        doc,
        workflow,
    })
}

/// Validates twice on purpose: once as a workflow, then once as a whole
/// config by reparsing the rendered document. The second catches anything the
/// first cannot see — and it runs before the write, so a rejected edit leaves
/// the file exactly as it was.
fn write_workflow(f: &mut WorkflowFile, wf: &Workflow) -> Result<(), String> {
    wf.validate().map_err(|e| e.to_string())?;
    if !f.doc.get("workflow").is_some_and(|w| w.is_table()) {
        f.doc["workflow"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    wf.write_into(
        f.doc["workflow"]
            .as_table_mut()
            .expect("just ensured a table"),
    );
    let body = f.doc.to_string();
    ProjectConfig::parse(&body)
        .map_err(|e| format!("that edit would make {} invalid: {e}", f.path.display()))?;
    std::fs::write(&f.path, body).map_err(|e| e.to_string())?;
    f.workflow = wf.clone();
    Ok(())
}

fn known_states(wf: &Workflow) -> String {
    wf.states.join(", ")
}

/// The tasks currently in `state`, with a fresh index behind them.
fn tasks_in(reg: &Registry, project: &Project, state: &str) -> Result<Vec<TaskKey>, String> {
    let app = crate::open_app(reg, project).map_err(|e| e.to_string())?;
    app.reconcile(crate::jiff_now_ms())
        .map_err(|e| e.to_string())?;
    let filter = TaskFilter {
        states: vec![state.to_string()],
        ..Default::default()
    };
    Ok(app
        .list_filtered(true, &filter)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|s| s.key)
        .collect())
}

fn move_them(reg: &Registry, project: &Project, keys: &[TaskKey], to: &str) -> Result<(), String> {
    let app = crate::open_app(reg, project).map_err(|e| e.to_string())?;
    let moved = app.move_tasks(keys, to).map_err(|e| e.to_string())?;
    println!("moved {moved} task(s) to `{to}`");
    Ok(())
}

fn state_cmd(reg: &mut Registry, cmd: StateCmd) -> Result<(), String> {
    match cmd {
        StateCmd::Ls { id } => {
            let f = read_workflow(reg, id.as_deref())?;
            for s in &f.workflow.states {
                let mut notes = vec![];
                if *s == f.workflow.initial {
                    notes.push("initial");
                }
                if f.workflow.terminal.contains(s) {
                    notes.push("terminal");
                }
                match notes.is_empty() {
                    true => println!("{s}"),
                    false => println!("{s}  ({})", notes.join(", ")),
                }
            }
            Ok(())
        }
        StateCmd::SetDefault { id } => {
            let f = read_workflow(reg, id.as_deref())?;
            reg.workflow = Some(f.workflow.clone());
            reg.save().map_err(|e| e.to_string())?;
            println!(
                "new projects will use `{}`'s workflow: {}",
                f.project.id,
                known_states(&f.workflow)
            );
            Ok(())
        }
        StateCmd::Add {
            state,
            id,
            after,
            before,
            terminal,
        } => {
            let mut f = read_workflow(reg, id.as_deref())?;
            let mut wf = f.workflow.clone();
            if wf.states.contains(&state) {
                return Err(format!("`{state}` is already a state"));
            }
            let anchor = after.as_ref().or(before.as_ref());
            let at = match anchor {
                None => wf.states.len(),
                Some(a) => {
                    let i = wf.states.iter().position(|s| s == a).ok_or_else(|| {
                        format!(
                            "unknown state `{a}` — declared state(s): {}",
                            known_states(&wf)
                        )
                    })?;
                    if after.is_some() { i + 1 } else { i }
                }
            };
            wf.states.insert(at, state.clone());
            if terminal {
                wf.terminal.push(state.clone());
            }
            write_workflow(&mut f, &wf)?;
            println!("added `{state}` — states are now: {}", known_states(&wf));
            Ok(())
        }
        StateCmd::Rm { state, id, move_to } => {
            let mut f = read_workflow(reg, id.as_deref())?;
            let mut wf = f.workflow.clone();
            if !wf.states.contains(&state) {
                return Err(format!(
                    "unknown state `{state}` — declared state(s): {}",
                    known_states(&wf)
                ));
            }
            if wf.initial == state {
                return Err(format!(
                    "`{state}` is where new tasks start — set another `initial` in {} first",
                    f.path.display()
                ));
            }
            if wf.states.len() == 1 {
                return Err("a workflow needs at least one state".to_string());
            }
            let holders = tasks_in(reg, &f.project, &state)?;
            if !holders.is_empty() {
                let dest = resolve_destination(&wf, &state, move_to, holders.len())?;
                move_them(reg, &f.project, &holders, &dest)?;
            }
            wf.states.retain(|s| *s != state);
            wf.terminal.retain(|s| *s != state);
            wf.transitions.remove(&state);
            for allowed in wf.transitions.values_mut() {
                allowed.retain(|s| *s != state);
            }
            write_workflow(&mut f, &wf)?;
            println!("removed `{state}` — states are now: {}", known_states(&wf));
            Ok(())
        }
        StateCmd::Rename { from, to, id } => {
            let mut f = read_workflow(reg, id.as_deref())?;
            if !f.workflow.states.contains(&from) {
                return Err(format!(
                    "unknown state `{from}` — declared state(s): {}",
                    known_states(&f.workflow)
                ));
            }
            if f.workflow.states.contains(&to) {
                return Err(format!(
                    "`{to}` already exists — use `cadet project state rm {from} --move-to {to}` to merge them"
                ));
            }
            // The new name is declared before any task moves into it, because
            // a move into an undeclared state is exactly what strands tasks.
            // That leaves both names declared for the duration of the move; if
            // the move fails, the extra state is visible and removable rather
            // than a set of tasks nothing can touch.
            let mut widened = f.workflow.clone();
            let at = widened
                .states
                .iter()
                .position(|s| *s == from)
                .expect("checked above");
            widened.states.insert(at + 1, to.clone());
            if widened.terminal.contains(&from) {
                widened.terminal.push(to.clone());
            }
            write_workflow(&mut f, &widened)?;

            let holders = tasks_in(reg, &f.project, &from)?;
            if !holders.is_empty() {
                move_them(reg, &f.project, &holders, &to)?;
            }

            let mut wf = widened;
            wf.states.retain(|s| *s != from);
            wf.terminal.retain(|s| *s != from);
            if wf.initial == from {
                wf.initial = to.clone();
            }
            if let Some(allowed) = wf.transitions.remove(&from) {
                wf.transitions.insert(to.clone(), allowed);
            }
            for allowed in wf.transitions.values_mut() {
                for s in allowed.iter_mut() {
                    if *s == from {
                        *s = to.clone();
                    }
                }
            }
            write_workflow(&mut f, &wf)?;
            println!("renamed `{from}` to `{to}`");
            Ok(())
        }
    }
}

/// Where tasks go when their state is removed. `--move-to` answers it
/// outright; otherwise a terminal prompts and a script gets a message naming
/// the flag. Never guesses — picking a state for someone silently rewrites
/// their tasks.
fn resolve_destination(
    wf: &Workflow,
    removing: &str,
    move_to: Option<String>,
    count: usize,
) -> Result<String, String> {
    let candidates: Vec<&str> = wf
        .states
        .iter()
        .map(String::as_str)
        .filter(|s| *s != removing)
        .collect();
    let dest = match move_to {
        Some(d) => d,
        None if prompt::is_interactive() => {
            println!(
                "{count} task(s) are in `{removing}`. Move them to which state? ({})",
                candidates.join(", ")
            );
            prompt::ask("state", candidates.first().copied()).map_err(|e| e.to_string())?
        }
        None => {
            return Err(format!(
                "{count} task(s) are still in `{removing}` — pass --move-to <state> ({})",
                candidates.join(", ")
            ));
        }
    };
    if !candidates.contains(&dest.as_str()) {
        return Err(format!(
            "unknown state `{dest}` — declared state(s): {}",
            candidates.join(", ")
        ));
    }
    Ok(dest)
}

pub fn run(cmd: ProjectCmd, mut reg: Registry) -> Result<(), String> {
    match cmd {
        ProjectCmd::State { cmd } => state_cmd(&mut reg, cmd),
        ProjectCmd::Ls => list(&reg),
        ProjectCmd::Dirs { id, add, rm } => dirs_cmd(&mut reg, &id, &add, &rm),
        ProjectCmd::Which => which(&reg),
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
            dirs,
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
                dirs,
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
    dirs: Vec<String>,
}

/// Two new subcommands and a listing line, all reading the same `dirs`
/// field — `which` deliberately calls `dirmatch::resolve` rather than
/// re-deriving precedence, so what it reports is what actually happens.
fn dirs_cmd(reg: &mut Registry, id: &str, add: &[String], rm: &[String]) -> Result<(), String> {
    let mut project = reg.find(id).cloned().ok_or_else(|| {
        format!(
            "unknown project `{id}` — configured project(s): {}",
            reg.known_projects()
        )
    })?;

    if add.is_empty() && rm.is_empty() {
        if project.dirs.is_empty() {
            println!("(none)");
        }
        for d in &project.dirs {
            println!("{}", d.display());
        }
        return Ok(());
    }

    for pattern in rm {
        let want = resolve_path(pattern)?;
        let before = project.dirs.len();
        project.dirs.retain(|d| d != &want);
        if project.dirs.len() == before {
            return Err(format!(
                "`{}` is not a directory of `{id}` — run `cadet project dirs {id}` to see them",
                want.display()
            ));
        }
    }
    for pattern in add {
        let want = resolve_path(pattern)?;
        if !project.dirs.contains(&want) {
            project.dirs.push(want);
        }
    }

    let n = project.dirs.len();
    reg.upsert_project(project);
    reg.save().map_err(|e| e.to_string())?;
    println!(
        "`{id}` now has {n} director{}",
        if n == 1 { "y" } else { "ies" }
    );
    Ok(())
}

fn which(reg: &Registry) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    // The same resolver the real commands use, so this cannot report one
    // thing while `cadet add` does another.
    let sel = crate::dirmatch::resolve(reg, None, &cwd)?;
    println!("{}  ({})", sel.project.id, sel.source.describe());
    Ok(())
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
        for d in &p.dirs {
            println!("{:<24}dir: {}", "", d.display());
        }
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
        dirs,
    } = new;
    // Stored absolute and tilde-expanded, exactly like `path` — one
    // expansion rule, applied once at write time rather than on every match.
    let resolved_dirs: Vec<std::path::PathBuf> = dirs
        .iter()
        .map(|d| resolve_path(d))
        .collect::<Result<_, _>>()?;
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
        dirs: vec![],
    }
    .config_path();

    // Bound once: `project.toml` appearing or vanishing between here and the
    // adoption guard below would otherwise let a fresh root skip the gate.
    let is_readd = project_toml.exists();

    // Read the config being overwritten (if any) before it's gone, so a
    // `--force` re-add without explicit --prefix/--name re-derives from the
    // id's default rather than silently swapping the project's real prefix
    // out from under it. Under the old `init` this was impossible — prefix
    // and name were required flags, so an overwrite could only ever write
    // what the user typed. Making them optional here turned a bare `--force`
    // into "silently re-derive", which splits one project's tasks across two
    // key namespaces (`ALFA-*` and `ALPH-*`) that `doctor` has no way to see
    // are the same project.
    let existing_src = if is_readd {
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
    // consequence gets a gate. A re-add skips the gate — a project's own
    // task files are exactly what would be counted — but no path adopts a
    // root on a walk that stopped early, so a folder that cannot be read
    // is refused rather than adopted on an undercount. `local-db` skips
    // this outright: `root` there is a single database file, not a folder
    // of notes.
    if backend == BackendKind::Markdown {
        if is_readd {
            ensure_scannable(&root)?;
        } else {
            let found = count_markdown(&root, Some(MANY_NOTES))?;
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
                ensure_scannable(&root)?;
            }
            // At or under the limit the wire never tripped — see `markdown_files_under`.
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
    let body = render_project_toml(
        existing_src.as_deref(),
        &id,
        &name,
        &prefix,
        reg.workflow.as_ref(),
    );
    ProjectConfig::parse(&body)
        .map_err(|e| format!("generated project.toml would not parse: {e}"))?;
    std::fs::write(&project_toml, body).map_err(|e| e.to_string())?;
    reg.upsert_project(Project {
        id: id.clone(),
        path: root.clone(),
        backend,
        dirs: resolved_dirs.clone(),
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

    #[cfg(unix)]
    use cadet_backend_markdown::MarkdownBackend;

    #[cfg(unix)]
    struct LockedDir {
        root: std::path::PathBuf,
        locked: std::path::PathBuf,
        _tmp: Option<tempfile::TempDir>,
    }

    #[cfg(unix)]
    impl LockedDir {
        fn new() -> Option<Self> {
            let tmp = tempfile::tempdir().unwrap();
            let mut locked = Self::inside(tmp.path())?;
            locked._tmp = Some(tmp);
            Some(locked)
        }

        fn inside(root: &std::path::Path) -> Option<Self> {
            use std::os::unix::fs::PermissionsExt;

            let locked = root.join("sub");
            std::fs::create_dir_all(&locked).unwrap();
            std::fs::write(locked.join("hidden.md"), "x").unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read_dir(&locked).is_ok() {
                let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
                eprintln!(
                    "skipped: this user reads a 0o000 directory, so the unreadable-directory guard was never exercised"
                );
                return None;
            }
            Some(Self {
                root: root.to_path_buf(),
                locked,
                _tmp: None,
            })
        }

        fn root(&self) -> &std::path::Path {
            &self.root
        }

        fn locked(&self) -> &std::path::Path {
            &self.locked
        }
    }

    #[cfg(unix)]
    impl Drop for LockedDir {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;

            let _ = std::fs::set_permissions(&self.locked, std::fs::Permissions::from_mode(0o755));
        }
    }

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
        let got = render_project_toml(Some(CUSTOMISED), "new", "New", "NEW", None);
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
        let got = render_project_toml(None, "fresh", "Fresh", "FR", None);
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
        let got = render_project_toml(Some("[workflow]\nstates = [\"a\"]\n"), "x", "X", "XX", None);
        assert!(got.contains(r#"prefix = "XX""#), "{got}");
    }

    #[test]
    fn count_markdown_stops_once_the_limit_is_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("n{i}.md")), "x").unwrap();
        }
        assert_eq!(count_markdown(dir.path(), Some(3)).unwrap(), 4);
        assert_eq!(count_markdown(dir.path(), Some(100)).unwrap(), 10);
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
        assert_eq!(count_markdown(dir.path(), Some(100)).unwrap(), 2);
    }

    #[test]
    fn count_markdown_of_a_missing_folder_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            count_markdown(&dir.path().join("nope"), Some(100)).unwrap(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn count_markdown_and_markdown_files_agree_on_unreadable_directories() {
        let Some(locked) = LockedDir::new() else {
            return;
        };
        let want = locked.locked().display().to_string();

        let count_err = count_markdown(locked.root(), Some(100)).unwrap_err();
        let backend_err = MarkdownBackend::new(locked.root().to_path_buf())
            .markdown_files()
            .unwrap_err()
            .to_string();

        assert!(
            count_err.contains(&want),
            "count_markdown said: {count_err}"
        );
        assert!(
            backend_err.contains(&want),
            "markdown_files said: {backend_err}"
        );
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
