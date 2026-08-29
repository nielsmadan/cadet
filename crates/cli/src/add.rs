use super::{
    AssignmentMode, Priority, Project, Registry, apply_assignment, check_title,
    extract_description_tags, extract_tags, load_config, parse_priority, parse_tag, priority_label,
    reject_duplicate_names, reject_flag_collisions,
};
use crate::prompt::{self, PromptInput};
use cadet_app::{TaskChanges, TaskDraft};
use cadet_core::{FieldValue, ProjectConfig, resolve_due};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;

pub struct Options {
    pub title: Vec<String>,
    pub interactive: bool,
    pub literal: bool,
    pub due: Option<String>,
    pub no_due: bool,
    pub tags: Vec<String>,
    pub priority: Option<Priority>,
    pub state: Option<String>,
    pub set: Vec<String>,
}

struct Seed {
    title: String,
    description: String,
    title_tags: Vec<String>,
    description_tags: Vec<String>,
    explicit_tags: Vec<String>,
    due_spec: Option<String>,
    priority: Option<Priority>,
    state: Option<String>,
    fields: BTreeMap<String, FieldValue>,
    literal: bool,
    interactive: bool,
}

struct Context<'a> {
    cfg: &'a ProjectConfig,
    config_path: &'a Path,
    global_due: Option<&'a str>,
    today: jiff::civil::Date,
    is_tty: bool,
}

pub fn prepare(
    options: Options,
    reg: &Registry,
    project: &Project,
    today: jiff::civil::Date,
) -> Result<Option<TaskDraft>, Box<dyn std::error::Error>> {
    let (cfg, config_path) = load_config(project)?;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut writer = std::io::stderr();
    prepare_with(
        options,
        Context {
            cfg: &cfg,
            config_path: &config_path,
            global_due: reg.defaults.due.as_deref(),
            today,
            is_tty: prompt::is_interactive(),
        },
        &mut reader,
        &mut writer,
    )
    .map_err(Into::into)
}

fn prepare_with<R: BufRead, W: Write>(
    options: Options,
    context: Context<'_>,
    reader: &mut R,
    writer: &mut W,
) -> Result<Option<TaskDraft>, String> {
    let seed = resolve_seed(
        options,
        context.cfg,
        context.config_path,
        context.global_due,
    )?;
    let use_wizard = if seed.interactive {
        if !context.is_tty {
            return Err("`cadet add --interactive` requires a terminal".to_string());
        }
        true
    } else if seed.title.trim().is_empty() {
        if !context.is_tty {
            return Err(
                "task title is required — pass a title, or run `cadet add` in a terminal"
                    .to_string(),
            );
        }
        true
    } else {
        false
    };
    if use_wizard {
        collect(seed, context.cfg, context.today, reader, writer)
    } else {
        validate_required_fields(&seed, context.cfg)?;
        finalize(seed, context.today).map(Some)
    }
}

fn resolve_seed(
    options: Options,
    cfg: &ProjectConfig,
    config_path: &Path,
    global_due: Option<&str>,
) -> Result<Seed, String> {
    reject_duplicate_names(&options.set)?;

    let positional_given = !options.title.is_empty();
    let (title, description, title_tags, description_tags) =
        parse_message(&options.title.join(" "), options.literal)?;
    reject_flag_collisions(
        &options.set,
        &[
            ("title", "the positional title", positional_given),
            (
                "body",
                "the positional description",
                !description.is_empty(),
            ),
            (
                "due",
                "`--due`/`--no-due`",
                options.due.is_some() || options.no_due,
            ),
            (
                "tags",
                "`--tag`/`[tag]` shorthand",
                !options.tags.is_empty() || !title_tags.is_empty() || !description_tags.is_empty(),
            ),
            ("priority", "`--priority`", options.priority.is_some()),
            ("state", "`--state`", options.state.is_some()),
        ],
    )?;

    let mut scratch = TaskChanges::default();
    for pair in &options.set {
        apply_assignment(
            cfg,
            &mut scratch,
            pair,
            config_path,
            AssignmentMode::NewTask,
        )?;
    }
    let title = scratch.title.take().unwrap_or(title);
    let description = scratch.body.take().unwrap_or(description);
    check_title(&title)?;
    let state = scratch.state.take().or(options.state);
    let priority = scratch.priority.take().or(options.priority);
    let explicit_tags = scratch.tags.take().unwrap_or(options.tags);
    let assigned_due = match scratch.due.take() {
        Some(Some(spec)) => Some(spec),
        Some(None) => {
            return Err(
                "cannot clear `due` on a new task — pass --no-due to skip a configured default"
                    .to_string(),
            );
        }
        None => None,
    };
    let explicit_due = assigned_due.or(options.due);
    let due_spec = cadet_core::select_due_for_new_task(
        explicit_due.as_deref(),
        options.no_due,
        cfg.defaults.due.as_deref(),
        global_due,
    )
    .map(str::to_string);
    if let Some(spec) = &due_spec {
        cadet_core::reject_newlines("due", spec).map_err(|e| e.to_string())?;
    }

    let mut fields = BTreeMap::new();
    for (name, value) in scratch.fields {
        match value {
            Some(value) => {
                fields.insert(name, value);
            }
            None => {
                return Err(format!(
                    "cannot clear `{name}` — there is nothing to clear on a new task"
                ));
            }
        }
    }
    Ok(Seed {
        title,
        description,
        title_tags,
        description_tags,
        explicit_tags,
        due_spec,
        priority,
        state,
        fields,
        literal: options.literal,
        interactive: options.interactive,
    })
}

fn parse_message(
    joined: &str,
    literal: bool,
) -> Result<(String, String, Vec<String>, Vec<String>), String> {
    if literal {
        return Ok((
            joined.trim().to_string(),
            String::new(),
            Vec::new(),
            Vec::new(),
        ));
    }
    let (title, description) = joined.split_once(" | ").unwrap_or((joined, ""));
    let (title, title_tags) = extract_tags(title)?;
    let (description, description_tags) = extract_description_tags(description)?;
    cadet_core::reject_newlines("description", &description).map_err(|e| e.to_string())?;
    Ok((title, description, title_tags, description_tags))
}

fn collect<R: BufRead, W: Write>(
    mut seed: Seed,
    cfg: &ProjectConfig,
    today: jiff::civil::Date,
    reader: &mut R,
    writer: &mut W,
) -> Result<Option<TaskDraft>, String> {
    let (title, title_tags) = loop {
        let default = (!seed.title.is_empty()).then_some(seed.title.as_str());
        match ask(reader, writer, "Title", default)? {
            PromptInput::Eof => return cancelled(writer),
            PromptInput::DefaultAccepted if seed.title.trim().is_empty() => {
                show_error(writer, "task title must not be empty")?;
            }
            PromptInput::DefaultAccepted => {
                break (seed.title.clone(), seed.title_tags.clone());
            }
            PromptInput::Value(raw) => {
                let keep_existing_tags = seed.title.trim().is_empty();
                let parsed = if seed.literal {
                    Ok((raw.trim().to_string(), Vec::new()))
                } else {
                    extract_tags(&raw)
                };
                match parsed.and_then(|(title, mut tags)| {
                    check_title(&title)?;
                    if title.trim().is_empty() {
                        Err("task title must not be empty".to_string())
                    } else {
                        if keep_existing_tags {
                            tags.splice(0..0, seed.title_tags.iter().cloned());
                        }
                        Ok((title, tags))
                    }
                }) {
                    Ok(value) => break value,
                    Err(error) => show_error(writer, &error)?,
                }
            }
        }
    };
    seed.title = title;
    seed.title_tags = title_tags;

    let (description, description_tags) = loop {
        let default = (!seed.description.is_empty()).then_some(seed.description.as_str());
        match ask(reader, writer, "Description (none to clear)", default)? {
            PromptInput::Eof => return cancelled(writer),
            PromptInput::DefaultAccepted => {
                break (seed.description.clone(), seed.description_tags.clone());
            }
            PromptInput::Value(raw) if raw.eq_ignore_ascii_case("none") => {
                break (String::new(), Vec::new());
            }
            PromptInput::Value(raw) => {
                let parsed = if seed.literal {
                    Ok((raw.trim().to_string(), Vec::new()))
                } else {
                    extract_description_tags(&raw)
                };
                match parsed.and_then(|(description, tags)| {
                    cadet_core::reject_newlines("description", &description)
                        .map_err(|e| e.to_string())?;
                    Ok((description, tags))
                }) {
                    Ok(value) => break value,
                    Err(error) => show_error(writer, &error)?,
                }
            }
        }
    };
    seed.description = description;
    seed.description_tags = description_tags;

    let mut due_default = seed.due_spec.clone();
    let due = loop {
        match ask(
            reader,
            writer,
            "Due (none for no due)",
            due_default.as_deref(),
        )? {
            PromptInput::Eof => return cancelled(writer),
            PromptInput::DefaultAccepted => match due_default.as_deref() {
                Some(spec) => match resolved_due(spec, today, writer) {
                    Ok(due) => break Some(due),
                    Err(error) => show_error(writer, &error)?,
                },
                None => break None,
            },
            PromptInput::Value(raw) if raw.eq_ignore_ascii_case("none") => break None,
            PromptInput::Value(raw) => match resolved_due(&raw, today, writer) {
                Ok(due) => break Some(due),
                Err(error) => {
                    show_error(writer, &error)?;
                    due_default = Some(raw);
                }
            },
        }
    };
    seed.due_spec = due.clone();

    if due.is_none() {
        let mut default = priority_label(seed.priority.unwrap_or_default()).to_string();
        loop {
            match ask(reader, writer, "Priority", Some(&default))? {
                PromptInput::Eof => return cancelled(writer),
                PromptInput::DefaultAccepted => match parse_priority(&default) {
                    Ok(priority) => {
                        seed.priority = Some(priority);
                        break;
                    }
                    Err(error) => show_error(writer, &error)?,
                },
                PromptInput::Value(raw) => match parse_priority(&raw) {
                    Ok(priority) => {
                        seed.priority = Some(priority);
                        break;
                    }
                    Err(error) => {
                        show_error(writer, &error)?;
                        default = raw;
                    }
                },
            }
        }
    }

    let mut tags = seed.title_tags.clone();
    tags.extend(seed.description_tags.clone());
    tags.extend(seed.explicit_tags.clone());
    loop {
        let shown = (!tags.is_empty()).then(|| tags.join(", "));
        match ask(
            reader,
            writer,
            "Tags (comma-separated; none to clear)",
            shown.as_deref(),
        )? {
            PromptInput::Eof => return cancelled(writer),
            PromptInput::DefaultAccepted => break,
            PromptInput::Value(raw) if raw.eq_ignore_ascii_case("none") => {
                tags.clear();
                break;
            }
            PromptInput::Value(raw) => match parse_tags(&raw) {
                Ok(parsed) => {
                    tags = parsed;
                    break;
                }
                Err(error) => show_error(writer, &error)?,
            },
        }
    }

    let missing_fields: Vec<_> = cfg
        .fields
        .iter()
        .filter(|field| field.required && !seed.fields.contains_key(&field.name))
        .collect();
    for field in missing_fields {
        loop {
            match ask(reader, writer, &format!("{} (required)", field.name), None)? {
                PromptInput::Eof => return cancelled(writer),
                PromptInput::DefaultAccepted => {
                    show_error(writer, &format!("`{}` is required", field.name))?;
                }
                PromptInput::Value(raw) => match cadet_core::parse_field_value(field, &raw) {
                    Ok(value) => {
                        seed.fields.insert(field.name.clone(), value);
                        break;
                    }
                    Err(error) => show_error(writer, &error.to_string())?,
                },
            }
        }
    }

    Ok(Some(build_draft(seed, due, tags)))
}

fn validate_required_fields(seed: &Seed, cfg: &ProjectConfig) -> Result<(), String> {
    if let Some(field) = cfg
        .fields
        .iter()
        .find(|field| field.required && !seed.fields.contains_key(&field.name))
    {
        return Err(format!(
            "required field `{}` must be supplied with `--set {}=VALUE`",
            field.name, field.name
        ));
    }
    Ok(())
}

fn finalize(seed: Seed, today: jiff::civil::Date) -> Result<TaskDraft, String> {
    check_title(&seed.title)?;
    if seed.title.trim().is_empty() {
        return Err("task title must not be empty".to_string());
    }
    let due = seed
        .due_spec
        .as_deref()
        .map(|spec| resolve_due(spec, today).map_err(|e| e.to_string()))
        .transpose()?;
    let mut tags = seed.title_tags.clone();
    tags.extend(seed.description_tags.clone());
    tags.extend(seed.explicit_tags.clone());
    Ok(build_draft(seed, due, tags))
}

fn build_draft(seed: Seed, due: Option<String>, tags: Vec<String>) -> TaskDraft {
    TaskDraft {
        title: seed.title,
        due,
        priority: seed.priority,
        tags,
        state: seed.state,
        fields: seed.fields,
        body: if seed.description.trim().is_empty() {
            String::new()
        } else {
            format!("\n{}\n", seed.description.trim())
        },
    }
}

fn resolved_due<W: Write>(
    spec: &str,
    today: jiff::civil::Date,
    writer: &mut W,
) -> Result<String, String> {
    let resolved = resolve_due(spec, today).map_err(|e| e.to_string())?;
    if spec.trim() != resolved {
        writeln!(writer, "  → {resolved}").map_err(|e| e.to_string())?;
    }
    Ok(resolved)
}

fn parse_tags(raw: &str) -> Result<Vec<String>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(parse_tag)
        .collect()
}

fn ask<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: Option<&str>,
) -> Result<PromptInput, String> {
    prompt::ask_cancelable_with(reader, writer, label, default).map_err(|e| e.to_string())
}

fn show_error<W: Write>(writer: &mut W, error: &str) -> Result<(), String> {
    writeln!(writer, "  error: {error}").map_err(|e| e.to_string())
}

fn cancelled<W: Write, T>(writer: &mut W) -> Result<Option<T>, String> {
    writeln!(writer, "  cancelled").map_err(|e| e.to_string())?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn cfg(default_due: Option<&str>) -> ProjectConfig {
        let defaults = default_due
            .map(|due| format!("\n[defaults]\ndue = \"{due}\"\n"))
            .unwrap_or_default();
        ProjectConfig::parse(&format!(
            r#"
[project]
id = "p"
name = "P"
prefix = "P"
[workflow]
states = ["todo", "done"]
initial = "todo"
terminal = ["done"]
{defaults}
"#
        ))
        .unwrap()
    }

    fn options(title: &[&str]) -> Options {
        Options {
            title: title.iter().map(|s| s.to_string()).collect(),
            interactive: false,
            literal: false,
            due: None,
            no_due: false,
            tags: Vec::new(),
            priority: None,
            state: None,
            set: Vec::new(),
        }
    }

    fn run(
        options: Options,
        input: &str,
        is_tty: bool,
        default_due: Option<&str>,
    ) -> (Result<Option<TaskDraft>, String>, String) {
        let cfg = cfg(default_due);
        let mut reader = Cursor::new(input.as_bytes());
        let mut writer = Vec::new();
        let result = prepare_with(
            options,
            Context {
                cfg: &cfg,
                config_path: Path::new("/tmp/project.toml"),
                global_due: None,
                today: "2026-08-09".parse().unwrap(),
                is_tty,
            },
            &mut reader,
            &mut writer,
        );
        (result, String::from_utf8(writer).unwrap())
    }

    #[test]
    fn message_shorthand_sets_body_and_extracts_tags_from_both_sides() {
        let (result, _) = run(
            options(&["[bug] Ship | Read [docs](url) [backend]"]),
            "",
            false,
            None,
        );
        let draft = result.unwrap().unwrap();
        assert_eq!(draft.title, "Ship");
        assert_eq!(draft.body, "\nRead [docs](url)\n");
        assert_eq!(draft.tags, vec!["bug", "backend"]);
    }

    #[test]
    fn literal_mode_preserves_every_shorthand_character() {
        let mut input = options(&["[bug] Ship | description [backend]"]);
        input.literal = true;
        let draft = run(input, "", false, None).0.unwrap().unwrap();
        assert_eq!(draft.title, "[bug] Ship | description [backend]");
        assert!(draft.body.is_empty());
        assert!(draft.tags.is_empty());
    }

    #[test]
    fn undated_wizard_asks_for_priority_and_collects_description_tags() {
        let (result, shown) = run(
            options(&[]),
            "Task\nAbout this [backend]\n\n\n\n",
            true,
            None,
        );
        let draft = result.unwrap().unwrap();
        assert_eq!(draft.title, "Task");
        assert_eq!(draft.body, "\nAbout this\n");
        assert_eq!(draft.priority, Some(Priority::Normal));
        assert_eq!(draft.tags, vec!["backend"]);
        assert!(shown.contains("Priority"), "{shown}");
    }

    #[test]
    fn dated_wizard_skips_priority_and_reports_the_resolved_date() {
        let (result, shown) = run(options(&[]), "Task\n\n1w\n\n", true, None);
        let draft = result.unwrap().unwrap();
        assert_eq!(draft.due.as_deref(), Some("2026-08-16"));
        assert!(shown.contains("→ 2026-08-16"), "{shown}");
        assert!(!shown.contains("Priority"), "{shown}");
    }

    #[test]
    fn none_clears_a_configured_due_and_triggers_priority() {
        let (result, shown) = run(options(&[]), "Task\n\nnone\nhigh\n\n", true, Some("+1w"));
        let draft = result.unwrap().unwrap();
        assert_eq!(draft.due, None);
        assert_eq!(draft.priority, Some(Priority::High));
        assert!(shown.contains("[+1w]"), "{shown}");
    }

    #[test]
    fn tags_without_a_title_survive_the_title_prompt() {
        let (result, _) = run(
            options(&["[bug]"]),
            "Prompted title\n\nnone\n\n\n",
            true,
            None,
        );
        let draft = result.unwrap().unwrap();
        assert_eq!(draft.title, "Prompted title");
        assert_eq!(draft.tags, vec!["bug"]);
    }

    #[test]
    fn command_line_values_are_editable_wizard_defaults() {
        let mut input = options(&["[bug] Original | Old description"]);
        input.interactive = true;
        input.priority = Some(Priority::Low);
        input.tags = vec!["frontend".into()];
        let (result, shown) = run(
            input,
            "Changed\nNew description [backend]\n\nhigh\n\n",
            true,
            None,
        );
        let draft = result.unwrap().unwrap();
        assert_eq!(draft.title, "Changed");
        assert_eq!(draft.body, "\nNew description\n");
        assert_eq!(draft.priority, Some(Priority::High));
        assert_eq!(draft.tags, vec!["backend", "frontend"]);
        assert!(shown.contains("[Original]"), "{shown}");
        assert!(shown.contains("[Old description]"), "{shown}");
        assert!(shown.contains("[low]"), "{shown}");
    }

    #[test]
    fn wizard_retries_invalid_values_until_they_are_corrected() {
        let (result, shown) = run(
            options(&[]),
            "[] bad\nTask\n\nbanana\nnone\nurgent\nhigh\nbad\rtag\nbackend\n",
            true,
            None,
        );
        let draft = result.unwrap().unwrap();
        assert_eq!(draft.title, "Task");
        assert_eq!(draft.due, None);
        assert_eq!(draft.priority, Some(Priority::High));
        assert_eq!(draft.tags, vec!["backend"]);
        assert_eq!(shown.matches("error:").count(), 4, "{shown}");
    }

    #[test]
    fn wizard_collects_and_validates_missing_required_fields() {
        let cfg = ProjectConfig::parse(
            r#"
[project]
id = "p"
name = "P"
prefix = "P"
[workflow]
states = ["todo", "done"]
initial = "todo"
terminal = ["done"]
[[fields]]
name = "estimate"
type = "int"
required = true
"#,
        )
        .unwrap();
        let mut reader = Cursor::new("Task\n\nnone\n\n\nbad\n3\n".as_bytes());
        let mut writer = Vec::new();
        let result = prepare_with(
            options(&[]),
            Context {
                cfg: &cfg,
                config_path: Path::new("/tmp/project.toml"),
                global_due: None,
                today: "2026-08-09".parse().unwrap(),
                is_tty: true,
            },
            &mut reader,
            &mut writer,
        );
        let draft = result.unwrap().unwrap();
        assert_eq!(draft.fields.get("estimate"), Some(&FieldValue::Int(3)));
        let shown = String::from_utf8(writer).unwrap();
        assert!(shown.contains("estimate (required)"), "{shown}");
        assert!(shown.contains("whole number"), "{shown}");
    }

    #[test]
    fn eof_cancels_the_whole_wizard() {
        let (result, shown) = run(options(&[]), "Task\n", true, None);
        assert!(result.unwrap().is_none());
        assert!(shown.contains("cancelled"), "{shown}");
    }

    #[test]
    fn set_title_remains_a_complete_noninteractive_add() {
        let mut input = options(&[]);
        input.set = vec!["title=From set".into()];
        let draft = run(input, "", false, None).0.unwrap().unwrap();
        assert_eq!(draft.title, "From set");
    }

    #[test]
    fn missing_title_and_forced_wizard_both_reject_a_non_tty() {
        assert!(run(options(&[]), "", false, None).0.is_err());
        let mut input = options(&["Already titled"]);
        input.interactive = true;
        assert!(run(input, "", false, None).0.is_err());
    }
}
