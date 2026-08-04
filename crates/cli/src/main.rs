mod config;
mod dirmatch;
mod project;
mod prompt;

use cadet_app::{App, GitNet, RejectReason, TaskChanges, TaskDraft};
use cadet_backend_local_db::LocalDbBackend;
use cadet_backend_markdown::MarkdownBackend;
use cadet_core::{
    Backend, DueBucket, Priority, ProjectConfig, TaskFilter, TaskKey, is_date_like,
    resolve_due_for_new_task,
};
use cadet_store_sqlite::SqliteIndex;
use clap::{Parser, Subcommand};
use config::{BackendKind, Project, Registry};

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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum DueArg {
    Today,
    Week,
    Overdue,
}

impl From<DueArg> for DueBucket {
    fn from(d: DueArg) -> Self {
        match d {
            DueArg::Today => DueBucket::Today,
            DueArg::Week => DueBucket::Week,
            DueArg::Overdue => DueBucket::Overdue,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage projects
    Project {
        /// Declared only to keep clap's global `--project` out of this
        /// subtree: a global arg is propagated to every subcommand that does
        /// not already define one by that name, and `cadet project add
        /// --help` listing a flag for selecting the project to act on is
        /// nonsense in a command group that creates and lists them. Noted on
        /// stderr rather than ignored if it is actually passed, and never
        /// fatal: clap fills this field from the root spelling too, so
        /// refusing it would break `cadet --project work project ls`.
        #[arg(long, hide = true)]
        project: Option<String>,
        #[command(subcommand)]
        cmd: Option<project::ProjectCmd>,
    },
    /// Add a task
    Add {
        title: Vec<String>,
        /// Due date: 2026-08-10, or `today`, `tomorrow`, `+7d`, `+2w`
        #[arg(long)]
        due: Option<String>,
        /// Create the task with no due date, ignoring any configured default
        #[arg(long, conflicts_with = "due")]
        no_due: bool,
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
        /// Only tasks due today, in the next week, or already overdue
        #[arg(long, value_enum, ignore_case = true)]
        due: Option<DueArg>,
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
    /// Open a task in $EDITOR
    Edit { key: String },
    /// Mark one or more tasks done
    Done {
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Move a task to a state
    Mv { key: String, state: String },
    /// Remove one or more tasks
    Rm {
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Adopt every pending hand-written note immediately
    Adopt,
    /// Report quarantined tasks
    Doctor {
        #[command(subcommand)]
        cmd: Option<DoctorCmd>,
    },
    /// Revert the last change
    Undo,
}

#[derive(Subcommand)]
enum DoctorCmd {
    /// Move every task stuck in an undeclared state into a declared one.
    ///
    /// The way back from a `[workflow]` edited by hand, pulled in from
    /// another machine, or otherwise changed out from under tasks that were
    /// already in the removed state — every ordinary write refuses those
    /// tasks, including the move that would fix them.
    RepairState { from: String, to: String },
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

# Custom fields — uncomment to declare your own, then set them with
# `cadet add --set estimate=3` or `cadet set <KEY> size=m`.
# [[fields]]
# name = "estimate"
# type = "int"
#
# [[fields]]
# name = "size"
# type = "enum"
# values = ["s", "m", "l"]
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

pub(crate) fn open_app(reg: &Registry, p: &Project) -> Result<App, Box<dyn std::error::Error>> {
    let index = SqliteIndex::open(&reg.index_path())?;
    let (backend, git): (Box<dyn Backend>, Option<GitNet>) = match p.backend {
        BackendKind::Markdown => {
            let git = GitNet::new(reg.repo_dir(&p.id), p.path.clone());
            git.ensure_init()?;
            (Box::new(MarkdownBackend::new(p.path.clone())), Some(git))
        }
        // No work tree, so nothing for git to hold.
        BackendKind::LocalDb => (Box::new(LocalDbBackend::open(&p.path)?), None),
    };
    Ok(App::new(backend, index, git, p.id.clone()))
}

fn report_reconcile(report: &cadet_app::ReconcileReport) {
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
}

/// `$EDITOR`, split on whitespace so `EDITOR="code --wait"` works. Falls back
/// to `vi`, the same fallback the Justfile's `conf` recipe uses.
fn spawn_editor(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let editor = match std::env::var("EDITOR") {
        Ok(e) if !e.trim().is_empty() => e,
        _ => "vi".to_string(),
    };
    let mut parts = editor.split_whitespace();
    let program = parts.next().ok_or("EDITOR is blank")?;
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
        .map_err(|e| format!("could not launch editor `{editor}`: {e}"))?;
    // Advisory: the file on disk is the truth either way, and a commit with
    // no diff is already a no-op.
    if !status.success() {
        eprintln!("⚠ {editor} exited with {status}");
    }
    Ok(())
}

fn resolve_keys(app: &App, raw: &[String]) -> Result<Vec<TaskKey>, Box<dyn std::error::Error>> {
    raw.iter().map(|k| parse_key(app, k)).collect()
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
    let config_path = project.config_path();
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
    // `--set tags=a,,b` drops an empty item; `--tag ""` used to keep one, and
    // the backends then disagreed about it — markdown drops an empty tag on
    // the way back out of frontmatter, local-db stores it. Say so rather than
    // dropping it silently: an explicit `--tag ""` is a mistake, not a stray
    // separator in a list.
    if t.is_empty() {
        return Err("tag is empty — a tag needs a name".to_string());
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

/// The same rule as `reject_duplicate_names`, one level out. `--priority
/// high --set priority=low` silently yielded `low`, and `--tag home --set
/// tags=work` silently yielded `[work]` — last-wins across two spellings of
/// one field, with no word to the user. `given` pairs each reserved name
/// with how the dedicated flag is spelled and whether it was actually
/// passed.
fn reject_flag_collisions(set: &[String], given: &[(&str, &str, bool)]) -> Result<(), String> {
    for pair in set {
        let name = assignment_name(pair)?;
        if let Some((n, flag, _)) = given
            .iter()
            .find(|(n, _, was_given)| *was_given && *n == name)
        {
            return Err(format!(
                "`{n}` given twice — as {flag} and as `--set {n}=…`; use one or the other"
            ));
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

/// A title lands in the same line-oriented frontmatter block a custom field
/// value does, so it gets the same guard `parse_field_value` gives every
/// single-line field — `cadet_core::reject_newlines`, the one copy of that
/// rule, not a second one that agrees today. Without it `cadet add $'two\nlines'`
/// writes an orphan frontmatter line, silently truncates the task to `two`, and
/// a value shaped like `estimate: 999` is injected outright.
///
/// Applied here rather than in a backend: local-db stores a multi-line title
/// correctly, so rejecting everywhere costs it a capability it has — but a CLI
/// where `add` succeeds on one project and fails on another for the same input
/// is worse, and a title containing a newline is pathological regardless.
fn check_title(raw: &str) -> Result<(), String> {
    cadet_core::reject_newlines("title", raw).map_err(|e| e.to_string())
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
        "title" => {
            check_title(raw)?;
            changes.title = Some(raw.to_string());
        }
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
    if let Some(Cmd::Project { cmd, project }) = &mut cli.cmd {
        // Noted, never fatal. `--project` is global, and clap propagates the
        // ROOT spelling's value into this field too — so refusing here breaks
        // `cadet --project work project ls`, which is what anyone with
        // `alias c='cadet --project work'` types every time. A global flag that
        // does not apply to one subcommand is conventionally ignored; saying
        // nothing at all is the wart that put it in `project add --help` in the
        // first place.
        if project.is_some() {
            eprintln!("note: --project does not apply to `cadet project`; ignoring");
        }
        let cmd = cmd.take().unwrap_or(project::ProjectCmd::Ls);
        return project::run(cmd, reg).map_err(Into::into);
    }

    // One resolver, shared with `cadet project which`, so the precedence
    // rule exists once. `--project` and `CADET_PROJECT` are one selector and
    // get one error: the env spelling used to fall through to "no default
    // project set", which is false whenever a default *is* set and sends the
    // user to fix the wrong thing.
    let cwd = std::env::current_dir().unwrap_or_default();
    let selection = dirmatch::resolve(&reg, cli.project.as_deref(), &cwd)?;
    if let dirmatch::Source::Dir(pattern) = &selection.source {
        eprintln!(
            "note: selected `{}` — cwd matches `{}`",
            selection.project.id,
            pattern.display()
        );
    }
    let project = selection.project;
    let app = open_app(&reg, &project)?;

    let now = jiff_now_ms();
    let report = app.reconcile(now)?;
    report_reconcile(&report);
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
        due: None,
        due_before: None,
        due_after: None,
        fields: vec![],
    }) {
        Cmd::Project { .. } => unreachable!("handled above"),
        Cmd::Add {
            title,
            due,
            no_due,
            tags,
            priority,
            state,
            set,
        } => {
            reject_duplicate_names(&set)?;
            reject_flag_collisions(
                &set,
                &[
                    ("title", "the positional title", !title.is_empty()),
                    ("due", "`--due`", due.is_some()),
                    ("tags", "`--tag`", !tags.is_empty()),
                    ("priority", "`--priority`", priority.is_some()),
                    ("state", "`--state`", state.is_some()),
                ],
            )?;
            let (cfg, config_path) = load_config(&project)?;
            let mut scratch = TaskChanges::default();
            for pair in &set {
                apply_assignment(&cfg, &mut scratch, pair, &config_path)?;
            }
            // `--set due=` is applied further down and wins on its own terms;
            // this resolves the flag and the two configured defaults.
            let due = resolve_due_for_new_task(
                due.as_deref(),
                no_due,
                cfg.defaults.due.as_deref(),
                reg.defaults.due.as_deref(),
                today(now)?,
            )?;
            let mut draft = TaskDraft {
                // Trimmed, because `--set title=` trims and the two spellings
                // of one field must not disagree. Untrimmed they also split
                // the backends: markdown reads a frontmatter scalar back
                // trimmed, local-db keeps the padding verbatim, so the same
                // `cadet add` stored two different titles depending on the
                // project.
                title: title.join(" ").trim().to_string(),
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
                        "cannot clear `due` on a new task — pass --no-due to skip a configured default"
                            .into(),
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
            check_title(&draft.title)?;
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
            due,
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
            let (due_after, due_before) = match due {
                None => (due_after, due_before),
                Some(_) if due_before.is_some() || due_after.is_some() => {
                    return Err(
                        "`--due` and `--due-before`/`--due-after` say the same thing two ways; use one or the other"
                            .into(),
                    );
                }
                Some(d) => DueBucket::from(d).bounds(today(now)?)?,
            };
            reject_duplicate_names(&fields)?;
            // The config is only read when a filter needs it, so a plain
            // `ls` still costs nothing.
            let cfg = if states.is_empty() && fields.is_empty() {
                None
            } else {
                Some(load_config(&project)?)
            };
            // The write path already refuses an undeclared state — `mv` and
            // `add --state` both do. Without this, a typo here answers "no
            // tasks", which is a wrong answer rather than an error.
            if let Some((cfg, _)) = &cfg {
                for s in &states {
                    if !cfg.workflow.states.contains(s) {
                        return Err(format!(
                            "unknown state `{s}` — declared state(s): {}",
                            cfg.workflow.states.join(", ")
                        )
                        .into());
                    }
                }
            }
            let mut filter = TaskFilter {
                states,
                tags,
                priority: priority.map(Priority::from),
                due_before,
                due_after,
                fields: Vec::new(),
            };
            if let Some((cfg, config_path)) = &cfg {
                for pair in &fields {
                    let name = assignment_name(pair)?;
                    if let Some(msg) = reserved_field_redirect(&name) {
                        return Err(msg.into());
                    }
                    filter
                        .fields
                        .push(declared_assignment(cfg, pair, config_path)?);
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
        Cmd::Edit { key } => {
            let k = parse_key(&app, &key)?;
            let path = app.edit_path(&k)?;
            spawn_editor(&path)?;
            // A fresh clock read: editing can take minutes, and the grace
            // periods reconcile applies are measured against it.
            report_reconcile(&app.reconcile(jiff_now_ms())?);
            app.record_edit(&k, &path);
            print_warnings(&app);
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
        Cmd::Done { keys } => {
            for k in app.complete_tasks(&resolve_keys(&app, &keys)?)? {
                println!("{k} done");
            }
            print_warnings(&app);
        }
        Cmd::Mv { key, state } => {
            let k = parse_key(&app, &key)?;
            app.set_state(&k, &state)?;
            println!("{k} -> {state}");
            print_warnings(&app);
        }
        Cmd::Rm { keys } => {
            for k in app.delete_many(&resolve_keys(&app, &keys)?)? {
                println!("{k} removed");
            }
            print_warnings(&app);
        }
        Cmd::Adopt => {
            // `LocalDbBackend::adopt` is never actually reached: reconcile
            // short-circuits `Outcome::Adopt` the moment an observed entry
            // already carries a uid, and every row a local-db backend serves
            // has one by construction — there are no loose rows for it to
            // find. Left unchecked, `adopt_pending` would run to completion
            // and print "adopted 0 note(s)", a fake success spec §6
            // explicitly rules out. Gate here, on the project's backend
            // kind, before the call.
            if project.backend == BackendKind::LocalDb {
                return Err(
                    "this backend does not support adopt — a local-db project has no loose notes to adopt; every task is already written with a uid"
                        .into(),
                );
            }
            let r = app.adopt_pending(now)?;
            // A forced `PendingCopy` lands in `copies`, not `adopted` — both
            // are notes the user just asked to be given an identity.
            println!("adopted {} note(s)", r.adopted + r.copies);
            print_warnings(&app);
        }
        Cmd::Doctor {
            cmd: Some(DoctorCmd::RepairState { from, to }),
        } => {
            let filter = TaskFilter {
                states: vec![from.clone()],
                ..Default::default()
            };
            let keys: Vec<TaskKey> = app
                .list_filtered(true, &filter)?
                .into_iter()
                .map(|s| s.key)
                .collect();
            if keys.is_empty() {
                println!("no tasks are in `{from}`");
            } else {
                let moved = app.move_tasks(&keys, &to)?;
                println!("moved {moved} task(s) from `{from}` to `{to}`");
            }
        }
        Cmd::Doctor { cmd: None } => {
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
            let stranded = app.stranded()?;
            if !stranded.is_empty() {
                let mut states: Vec<&str> = stranded.iter().map(|s| s.state.as_str()).collect();
                states.sort_unstable();
                states.dedup();
                println!(
                    "stranded: {} task(s) in undeclared state(s): {}",
                    stranded.len(),
                    states.join(", ")
                );
                println!(
                    "  nothing can be written to these until they move — \
                     `cadet doctor repair-state <from> <to>`"
                );
            }
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

/// The local calendar day for a millisecond timestamp. Local, not UTC:
/// `due` has never carried a timezone anywhere in cadet, and a user typing
/// `--due today` means their own day.
fn today(now_ms: i64) -> Result<jiff::civil::Date, jiff::Error> {
    Ok(jiff::Timestamp::from_millisecond(now_ms)?
        .to_zoned(jiff::tz::TimeZone::system())
        .date())
}

pub(crate) fn jiff_now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}
