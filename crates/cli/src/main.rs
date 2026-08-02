mod config;
mod project;
mod prompt;

use cadet_app::{App, GitNet, RejectReason, TaskChanges, TaskDraft};
use cadet_backend_fs::FsBackend;
use cadet_core::{Priority, ProjectConfig, TaskFilter, TaskKey, is_date_like};
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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum PriorityArg {
    High,
    Normal,
    Low,
}

impl From<PriorityArg> for Priority {
    fn from(p: PriorityArg) -> Self {
        match p {
            PriorityArg::High => Priority::High,
            PriorityArg::Normal => Priority::Normal,
            PriorityArg::Low => Priority::Low,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage projects
    Project {
        #[command(subcommand)]
        cmd: Option<project::ProjectCmd>,
    },
    /// Add a task
    Add {
        title: Vec<String>,
        /// Due date, e.g. 2026-08-10
        #[arg(long)]
        due: Option<String>,
        /// Tag to attach, repeatable
        #[arg(long = "tag", value_parser = parse_tag)]
        tags: Vec<String>,
        /// Priority
        #[arg(long, value_enum, ignore_case = true)]
        priority: Option<PriorityArg>,
        /// Initial state, defaults to the workflow's initial state
        #[arg(long)]
        state: Option<String>,
        /// name=value, repeatable. Only declared fields (or the reserved
        /// names handled by their own flags) are accepted.
        #[arg(long = "set")]
        set: Vec<String>,
    },
    /// Set fields on a task
    Set {
        /// Task key, e.g. T-1
        key: String,
        /// name=value, repeatable. An empty value clears the field.
        assignments: Vec<String>,
    },
    /// List tasks
    Ls {
        /// Include terminal states (e.g. done)
        #[arg(long)]
        all: bool,
        /// Only tasks in this state, repeatable (OR'd)
        #[arg(long = "state")]
        states: Vec<String>,
        /// Only tasks with this tag, repeatable (AND'd)
        #[arg(long = "tag", value_parser = parse_tag)]
        tags: Vec<String>,
        /// Only tasks at this priority
        #[arg(long, value_enum, ignore_case = true)]
        priority: Option<PriorityArg>,
        /// Only tasks due before this date
        #[arg(long)]
        due_before: Option<String>,
        /// Only tasks due after this date
        #[arg(long)]
        due_after: Option<String>,
        /// name=value against a declared field, repeatable (AND'd)
        #[arg(long = "field")]
        fields: Vec<String>,
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

pub(crate) const TEMPLATE: &str = r#"[project]
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

/// `App` never exposes its backend outside the crate (see `write.rs`), so the
/// CLI reads and parses `project.toml` itself. This is also what gives
/// `apply_assignment` and the `ls --field` parser the exact path to name in
/// an "unknown field" error.
fn load_config(
    project: &Project,
) -> Result<(ProjectConfig, std::path::PathBuf), Box<dyn std::error::Error>> {
    let config_path = project.path.join("project.toml");
    let src = std::fs::read_to_string(&config_path)?;
    let cfg = ProjectConfig::parse(&src)?;
    Ok((cfg, config_path))
}

fn parse_priority(s: &str) -> Result<Priority, String> {
    match s.to_ascii_lowercase().as_str() {
        "high" => Ok(Priority::High),
        "normal" => Ok(Priority::Normal),
        "low" => Ok(Priority::Low),
        other => Err(format!(
            "unknown priority `{other}` — expected high, normal or low"
        )),
    }
}

/// Kept next to `parse_priority`: they are the two halves of one spelling,
/// and this codebase's signature defect is a symmetric pair drifting apart.
fn priority_label(p: Priority) -> &'static str {
    match p {
        Priority::High => "high",
        Priority::Normal => "normal",
        Priority::Low => "low",
    }
}

/// A value the user typed, read back the way they typed it. `List` reads
/// like the `tags` line rather than as `List(["a", "b"])` — `show` is the
/// only way to see a custom field at all, so debug formatting there is not
/// a cosmetic problem.
fn render_field_value(v: &cadet_core::FieldValue) -> String {
    use cadet_core::FieldValue as V;
    match v {
        V::Str(s) | V::Date(s) => s.clone(),
        V::Int(i) => i.to_string(),
        V::Float(f) => f.to_string(),
        V::Bool(b) => b.to_string(),
        V::List(items) => items.join(", "),
    }
}

/// `label: value` lines, every value aligned to one column past the longest
/// label. With only `state`/`due`/`tags` present this reproduces the fixed
/// two-space layout those lines have always had, so a task carrying no
/// priority and no custom fields prints exactly what it printed before.
fn print_labelled(rows: &[(String, String)]) {
    let width = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0) + 2;
    for (label, value) in rows {
        println!("{:<width$}{value}", format!("{label}:"));
    }
}

/// Shared by `apply_assignment`'s undeclared-field branch and `ls --field`,
/// so the two ways of naming an undeclared field never drift into two
/// different error messages.
fn unknown_field_error(name: &str, config_path: &std::path::Path) -> String {
    format!(
        "unknown field `{name}` — declare it in {} under [[fields]]",
        config_path.display()
    )
}

/// `--tag` used to write its value into the task verbatim, while `--set
/// tags=a,b` comma-splits and trims — so `--tag "  home  "` and `--tag
/// "home,urgent"` silently disagreed with the equivalent `--set` spelling,
/// with no error anywhere and the only symptom being a task quietly
/// missing from `ls --tag`. Trim here, and reject an embedded comma
/// outright: repeating `--tag` is the one unambiguous way to give more
/// than one.
fn parse_tag(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.contains(',') {
        return Err(format!(
            "tag `{raw}` contains a comma — repeat --tag instead of joining tags with a comma"
        ));
    }
    Ok(t.to_string())
}

/// `ls --field` on a reserved name would otherwise fall through to
/// `cfg.fields` and print "declare it in project.toml under [[fields]]" —
/// advice that, followed literally, corrupts the config (a `[[fields]]`
/// entry whose name shadows a reserved one fails to parse) and then bricks
/// every command against the project until the edit is undone. Point at
/// the flag that already covers the reserved name instead of advising a
/// declaration that can never succeed.
fn reserved_field_redirect(name: &str) -> Option<&'static str> {
    match name {
        "tags" => Some("`tags` is not filterable via --field — use --tag instead"),
        "due" => Some("`due` is not filterable via --field — use --due-before/--due-after instead"),
        "state" => Some("`state` is not filterable via --field — use --state instead"),
        "priority" => Some("`priority` is not filterable via --field — use --priority instead"),
        "title" => Some("`title` has no ls filter"),
        _ => None,
    }
}

/// The `name` half of a `name=value` pair, split out on its own so a
/// repeated name can be detected before any of a batch is applied.
fn assignment_name(pair: &str) -> Result<String, String> {
    let (name, _) = pair
        .split_once('=')
        .ok_or_else(|| format!("expected name=value, got `{pair}`"))?;
    Ok(name.trim().to_string())
}

/// `set K priority=low priority=high` silently taking the last one, and
/// `ls --field x=1 --field x=2` silently building a filter that can never
/// match, are both easier to get right by rejecting the repeat up front
/// than by documenting last-wins.
fn reject_duplicate_names(pairs: &[String]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for pair in pairs {
        let name = assignment_name(pair)?;
        if !seen.insert(name.clone()) {
            return Err(format!("`{name}` given more than once"));
        }
    }
    Ok(())
}

/// `cadet_core::parse_assignment` plus the CLI's error enrichment — the
/// core error can't know which `project.toml` to name. Used by both
/// `apply_assignment`'s undeclared-field branch and `ls --field`, so there
/// is exactly one place that resolves a name against `cfg.fields` and
/// parses its value, not three.
fn declared_assignment(
    cfg: &ProjectConfig,
    pair: &str,
    config_path: &std::path::Path,
) -> Result<(String, cadet_core::FieldValue), String> {
    cadet_core::parse_assignment(cfg, pair).map_err(|e| match e {
        cadet_core::CoreError::UnknownField(name) => unknown_field_error(&name, config_path),
        other => other.to_string(),
    })
}

/// A bound that is not a date produces a silent, asymmetric wrong answer
/// from `TaskFilter` (a plain string comparison), not an error — `'2' <
/// 'b'` in ASCII means `--due-before banana` matches nearly everything and
/// `--due-after banana` matches nothing. Reject it up front instead, naming
/// the flag.
fn check_date_bound(flag: &str, raw: &str) -> Result<(), String> {
    if is_date_like(raw) {
        Ok(())
    } else {
        Err(format!(
            "`--{flag} {raw}` is not a date — expected a date such as 2026-08-10"
        ))
    }
}

/// Turns `name=value` into a change. Reserved names are handled directly;
/// everything else must be declared in project.toml, and the error says so.
/// Shared by `add --set` and `set` — the single most common defect in this
/// codebase's history is two copies of this rule drifting apart.
fn apply_assignment(
    cfg: &ProjectConfig,
    changes: &mut TaskChanges,
    pair: &str,
    config_path: &std::path::Path,
) -> Result<(), String> {
    let (name, raw) = pair
        .split_once('=')
        .ok_or_else(|| format!("expected name=value, got `{pair}`"))?;
    let raw = raw.trim();
    match name.trim() {
        "title" => changes.title = Some(raw.to_string()),
        "state" => changes.state = Some(raw.to_string()),
        "priority" => changes.priority = Some(parse_priority(raw)?),
        "due" => {
            if raw.is_empty() {
                changes.due = Some(None);
            } else {
                check_date_bound("due", raw)?;
                changes.due = Some(Some(raw.to_string()));
            }
        }
        "tags" => {
            changes.tags = Some(if raw.is_empty() {
                vec![]
            } else {
                raw.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
        }
        other => {
            if raw.is_empty() {
                // A declaration check, not a no-op: the backend never put an
                // undeclared key into `task.fields` (see `App::update`), so
                // falling through without checking here would let this
                // "clear" request reach `update` and lose the CLI's
                // enriched "declare it in project.toml" message in favour
                // of the bare core one.
                if !cfg.fields.iter().any(|f| f.name == other) {
                    return Err(unknown_field_error(other, config_path));
                }
                changes.fields.insert(other.to_string(), None);
            } else {
                let (name, v) = declared_assignment(cfg, pair, config_path)?;
                changes.fields.insert(name, Some(v));
            }
        }
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut cli = Cli::parse();
    let reg = Registry::load()?;

    // Handled before a project is resolved, since none may exist yet.
    if let Some(Cmd::Project { cmd }) = &mut cli.cmd {
        let cmd = cmd.take().unwrap_or(project::ProjectCmd::Ls);
        return project::run(cmd, reg).map_err(Into::into);
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
                    reg.known_projects()
                )
            })?,
        None => reg.active(None).cloned().ok_or_else(|| {
            if reg.projects.is_empty() {
                "no project configured — run `cadet project add <id>`".to_string()
            } else {
                format!(
                    "no default project set — pass --project or set one, configured project(s): {}",
                    reg.known_projects()
                )
            }
        })?,
    };
    let app = open_app(&reg, &project)?;

    let now = jiff_now_ms();
    let report = app.reconcile(now)?;
    match &report.scan_rejected {
        Some(RejectReason::SuspectedIncompleteScan) => {
            eprintln!("⚠ scan rejected: an unexpectedly large number of tasks disappeared.");
            eprintln!("  Nothing was deleted. Check that your sync tool has finished.");
        }
        Some(RejectReason::Incomplete) => {
            eprintln!("⚠ scan rejected: some files could not be read.");
            eprintln!(
                "  Nothing was deleted. Check file permissions, or wait for a cloud-synced folder to finish downloading."
            );
        }
        None => {}
    }
    if report.pending_adoption > 0 {
        eprintln!(
            "⚠ {} note(s) ready to adopt — run `cadet adopt`",
            report.pending_adoption
        );
    }
    // Reconcile runs ahead of every command, reads included, and it is where
    // a duplicate the resolver could not settle gets held back out of the
    // task list. That has to be visible on `cadet ls` too, not only after a
    // write — a silently shorter list is how the user finds out otherwise.
    print_warnings(&app);

    match cli.cmd.unwrap_or(Cmd::Ls {
        all: false,
        states: vec![],
        tags: vec![],
        priority: None,
        due_before: None,
        due_after: None,
        fields: vec![],
    }) {
        Cmd::Project { .. } => unreachable!("handled above"),
        Cmd::Add {
            title,
            due,
            tags,
            priority,
            state,
            set,
        } => {
            if let Some(d) = &due {
                check_date_bound("due", d)?;
            }
            reject_duplicate_names(&set)?;
            let (cfg, config_path) = load_config(&project)?;
            let mut scratch = TaskChanges::default();
            for pair in &set {
                apply_assignment(&cfg, &mut scratch, pair, &config_path)?;
            }
            let mut draft = TaskDraft {
                title: title.join(" "),
                due,
                priority: priority.map(Priority::from),
                tags,
                state,
                ..Default::default()
            };
            if let Some(t) = scratch.title {
                draft.title = t;
            }
            if let Some(s) = scratch.state {
                draft.state = Some(s);
            }
            match scratch.due {
                Some(Some(d)) => draft.due = Some(d),
                Some(None) => {
                    return Err(
                        "cannot clear `due` — there is nothing to clear on a new task".into(),
                    );
                }
                None => {}
            }
            if let Some(p) = scratch.priority {
                draft.priority = Some(p);
            }
            if let Some(t) = scratch.tags {
                draft.tags = t;
            }
            for (name, value) in scratch.fields {
                match value {
                    Some(v) => {
                        draft.fields.insert(name, v);
                    }
                    None => {
                        return Err(format!(
                            "cannot clear `{name}` — there is nothing to clear on a new task"
                        )
                        .into());
                    }
                }
            }
            let t = app.add_with(draft)?;
            println!("{}  {}", t.key, t.title);
            print_warnings(&app);
        }
        Cmd::Set { key, assignments } => {
            let k = parse_key(&app, &key)?;
            reject_duplicate_names(&assignments)?;
            let (cfg, config_path) = load_config(&project)?;
            let mut changes = TaskChanges::default();
            for pair in &assignments {
                apply_assignment(&cfg, &mut changes, pair, &config_path)?;
            }
            let t = app.update(&k, changes)?;
            println!("{}  {}", t.key, t.title);
            print_warnings(&app);
        }
        Cmd::Ls {
            all,
            states,
            tags,
            priority,
            due_before,
            due_after,
            fields,
        } => {
            if let Some(d) = &due_before {
                check_date_bound("due-before", d)?;
            }
            if let Some(d) = &due_after {
                check_date_bound("due-after", d)?;
            }
            reject_duplicate_names(&fields)?;
            let mut filter = TaskFilter {
                states,
                tags,
                priority: priority.map(Priority::from),
                due_before,
                due_after,
                fields: Vec::new(),
            };
            if !fields.is_empty() {
                let (cfg, config_path) = load_config(&project)?;
                for pair in &fields {
                    let name = assignment_name(pair)?;
                    if let Some(msg) = reserved_field_redirect(&name) {
                        return Err(msg.into());
                    }
                    filter
                        .fields
                        .push(declared_assignment(&cfg, pair, &config_path)?);
                }
            }
            let tasks = app.list_filtered(all, &filter)?;
            if tasks.is_empty() {
                println!("no tasks");
            }
            // A list that can be sorted and filtered by priority but never
            // shows it is the same read/write asymmetry as `show`. The column
            // only appears when some task in the list has one, so an ordinary
            // list keeps the row it always had.
            let show_priority = tasks.iter().any(|t| t.priority != Priority::Normal);
            for t in tasks {
                let key = t.key.to_string();
                if show_priority {
                    let p = if t.priority == Priority::Normal {
                        ""
                    } else {
                        priority_label(t.priority)
                    };
                    println!("{:<10} {:<8} {:<5} {}", key, t.state, p, t.title);
                } else {
                    println!("{:<10} {:<8} {}", key, t.state, t.title);
                }
            }
        }
        Cmd::Show { key } => {
            let k = parse_key(&app, &key)?;
            let t = app.get_by_key(&k)?;
            println!("{}  {}", t.key, t.title);
            let mut rows = vec![("state".to_string(), t.state.clone())];
            // `normal` is every task's default, so a line for it would appear
            // on every task and say nothing.
            if t.priority != Priority::Normal {
                rows.push((
                    "priority".to_string(),
                    priority_label(t.priority).to_string(),
                ));
            }
            if let Some(d) = &t.due {
                rows.push(("due".to_string(), d.clone()));
            }
            if !t.tags.is_empty() {
                rows.push(("tags".to_string(), t.tags.join(", ")));
            }
            // `fields` is a `BTreeMap`, so this order is already stable
            // across runs without a sort.
            for (name, value) in &t.fields {
                rows.push((name.clone(), render_field_value(value)));
            }
            print_labelled(&rows);
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
            // A forced `PendingCopy` lands in `copies`, not `adopted` — both
            // are notes the user just asked to be given an identity.
            println!("adopted {} note(s)", r.adopted + r.copies);
            print_warnings(&app);
        }
        Cmd::Doctor => {
            println!(
                "adopted: {}  pending: {}",
                report.adopted, report.pending_adoption
            );
            println!("pending deletions: {}", report.pending_deletion);
            let renumbers = app.renumber_status()?;
            println!(
                "renumbered: {}  pending renumber: {}",
                renumbers.recorded, renumbers.pending
            );
            if report.scan_rejected.is_some() {
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
