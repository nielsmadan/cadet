use crate::error::CoreError;

pub const RESERVED_FIELDS: &[&str] = &[
    "uid",
    "key",
    "title",
    "state",
    "created",
    "updated",
    "due",
    "priority",
    "tags",
    "body",
    "renumbered_from",
    "possible_duplicate_of",
];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MatchMode {
    #[default]
    Frontmatter,
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Str,
    Text,
    Int,
    Float,
    Bool,
    Date,
    DateTime,
    Enum(Vec<String>),
    ListStr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub ty: FieldType,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    pub states: Vec<String>,
    pub initial: String,
    pub terminal: Vec<String>,
    /// Empty map means anything-to-anything.
    pub transitions: std::collections::BTreeMap<String, Vec<String>>,
}

impl Workflow {
    /// Parse and validate a `[workflow]` table. The registry and every
    /// `project.toml` share this one implementation: two copies of the state
    /// rules is exactly the divergence this codebase keeps producing.
    pub fn from_toml_item(item: &toml_edit::Item) -> Result<Self, CoreError> {
        let wf = Workflow {
            states: str_vec(item, "states"),
            initial: item
                .get("initial")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            terminal: str_vec(item, "terminal"),
            transitions: item
                .get("transitions")
                .and_then(|t| t.as_table_like())
                .map(|t| {
                    t.iter()
                        .map(|(k, v)| {
                            let allowed = v
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default();
                            (k.to_string(), allowed)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        wf.validate()?;
        Ok(wf)
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        if self.states.is_empty() {
            return Err(CoreError::EmptyWorkflow);
        }
        if !self.states.contains(&self.initial) {
            return Err(CoreError::UnknownState(self.initial.clone()));
        }
        for state in &self.terminal {
            if !self.states.contains(state) {
                return Err(CoreError::UnknownState(state.clone()));
            }
        }
        for (from, allowed) in &self.transitions {
            if !self.states.contains(from) {
                return Err(CoreError::UnknownState(from.clone()));
            }
            for to in allowed {
                if !self.states.contains(to) {
                    return Err(CoreError::UnknownState(to.clone()));
                }
            }
        }
        Ok(())
    }

    /// Write into an existing table, leaving every key this type does not own
    /// (comments, unknown keys) exactly where it was.
    pub fn write_into(&self, tbl: &mut toml_edit::Table) {
        tbl["states"] = toml_edit::value(to_array(&self.states));
        tbl["initial"] = toml_edit::value(self.initial.as_str());
        tbl["terminal"] = toml_edit::value(to_array(&self.terminal));
        if self.transitions.is_empty() {
            tbl.remove("transitions");
        } else {
            let mut t = toml_edit::Table::new();
            for (from, allowed) in &self.transitions {
                t[from.as_str()] = toml_edit::value(to_array(allowed));
            }
            t.set_implicit(false);
            tbl["transitions"] = toml_edit::Item::Table(t);
        }
    }
}

/// The `[defaults]` table: what a new task gets when the command line says
/// nothing. Lives in both the registry and every `project.toml`, parsed and
/// written by this one type so the two spellings cannot drift.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Defaults {
    /// A due specification, not a date — `+7d` stays correct tomorrow.
    pub due: Option<String>,
}

impl Defaults {
    pub fn from_toml_item(item: &toml_edit::Item) -> Result<Self, CoreError> {
        let d = Defaults {
            due: item
                .get("due")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty()),
        };
        d.validate()?;
        Ok(d)
    }

    /// A due specification is date-independent, so validating it against any
    /// day proves it for every day. Checked on load rather than on first use:
    /// a typo that only surfaces the next time a task is created is a typo
    /// discovered by a task with the wrong due date.
    pub fn validate(&self) -> Result<(), CoreError> {
        if let Some(spec) = &self.due {
            crate::due::resolve_due(spec, jiff::civil::date(2000, 1, 1))?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.due.is_none()
    }

    pub fn write_into(&self, tbl: &mut toml_edit::Table) {
        match &self.due {
            Some(d) => tbl["due"] = toml_edit::value(d.as_str()),
            None => {
                tbl.remove("due");
            }
        }
    }
}

fn to_array(v: &[String]) -> toml_edit::Array {
    v.iter().map(String::as_str).collect()
}

fn str_vec(item: &toml_edit::Item, k: &str) -> Vec<String> {
    item.get(k)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectConfig {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub match_mode: MatchMode,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub workflow: Workflow,
    pub defaults: Defaults,
    pub fields: Vec<FieldDef>,
}

impl ProjectConfig {
    pub fn parse(src: &str) -> Result<Self, CoreError> {
        let doc: toml_edit::DocumentMut = src
            .parse()
            .map_err(|e| CoreError::ConfigParse(format!("{e}")))?;
        let get = |t: &toml_edit::Item, k: &str| -> String {
            t.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let project = doc
            .get("project")
            .ok_or_else(|| CoreError::ConfigParse("missing [project] table".into()))?;
        let wf_item = doc
            .get("workflow")
            .ok_or_else(|| CoreError::ConfigParse("missing [workflow] table".into()))?;

        let workflow = Workflow::from_toml_item(wf_item)?;
        let defaults = match doc.get("defaults") {
            Some(item) => Defaults::from_toml_item(item)?,
            None => Defaults::default(),
        };

        // Trimmed before the check, and stored trimmed: a whitespace-only
        // prefix passes `is_empty` and then renders keys as `" -1"`, and the
        // prefix is load-bearing for identity.
        let prefix = get(project, "prefix").trim().to_string();
        if prefix.is_empty() {
            return Err(CoreError::EmptyPrefix);
        }

        let mut fields = Vec::new();
        let mut seen_field_names = std::collections::HashSet::new();
        if let Some(arr) = doc.get("fields").and_then(|f| f.as_array_of_tables()) {
            for t in arr {
                let name = t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if RESERVED_FIELDS.contains(&name.as_str()) {
                    return Err(CoreError::ReservedFieldName(name));
                }
                if !seen_field_names.insert(name.clone()) {
                    return Err(CoreError::DuplicateFieldName(name));
                }
                let ty_name = t.get("type").and_then(|v| v.as_str()).unwrap_or("string");
                let ty = match ty_name {
                    "str" | "string" => FieldType::Str,
                    "text" => FieldType::Text,
                    "int" => FieldType::Int,
                    "float" => FieldType::Float,
                    "bool" => FieldType::Bool,
                    "date" => FieldType::Date,
                    "datetime" => FieldType::DateTime,
                    "list<string>" => FieldType::ListStr,
                    // Both spellings are accepted: `choices` is what a user
                    // reaches for unprompted, and reading only `values`
                    // turned that into an enum with no options — one that
                    // rejects every value with an empty `expects one of:`
                    // list rather than saying the declaration is wrong.
                    "enum" => {
                        let options: Vec<String> = ["values", "choices"]
                            .iter()
                            .find_map(|k| t.get(k).and_then(|v| v.as_array()))
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if options.is_empty() {
                            return Err(CoreError::EmptyEnum(name));
                        }
                        FieldType::Enum(options)
                    }
                    other => {
                        return Err(CoreError::UnknownFieldType {
                            field: name,
                            ty: other.to_string(),
                        });
                    }
                };
                fields.push(FieldDef {
                    name,
                    ty,
                    required: t.get("required").and_then(|v| v.as_bool()).unwrap_or(false),
                });
            }
        }

        let tasks = doc.get("tasks");
        let match_mode = match tasks.map(|t| get(t, "match")).as_deref() {
            Some("all") => MatchMode::All,
            _ => MatchMode::Frontmatter,
        };

        Ok(ProjectConfig {
            id: get(project, "id"),
            name: get(project, "name"),
            prefix,
            match_mode,
            include: tasks.map(|t| str_vec(t, "include")).unwrap_or_default(),
            exclude: tasks.map(|t| str_vec(t, "exclude")).unwrap_or_default(),
            workflow,
            defaults,
            fields,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[project]
id = "personal"
name = "Personal"
prefix = "PERS"

[workflow]
states = ["todo", "doing", "done"]
initial = "todo"
terminal = ["done"]

[[fields]]
name = "category"
type = "enum"
values = ["shopping", "admin"]
"#;

    #[test]
    fn parses_a_project() {
        let cfg = ProjectConfig::parse(SAMPLE).unwrap();
        assert_eq!(cfg.prefix, "PERS");
        assert_eq!(cfg.workflow.initial, "todo");
        assert_eq!(cfg.fields.len(), 1);
        assert!(matches!(cfg.fields[0].ty, FieldType::Enum(ref v) if v.len() == 2));
    }

    #[test]
    fn rejects_custom_field_shadowing_reserved_name() {
        let src = SAMPLE.replace("name = \"category\"", "name = \"title\"");
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(err, CoreError::ReservedFieldName(ref n) if n == "title"));
    }

    #[test]
    fn rejects_custom_field_shadowing_body() {
        let src = SAMPLE.replace("name = \"category\"", "name = \"body\"");
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(err, CoreError::ReservedFieldName(ref n) if n == "body"));
    }

    #[test]
    fn rejects_initial_state_not_in_states() {
        let src = SAMPLE.replace("initial = \"todo\"", "initial = \"nope\"");
        assert!(ProjectConfig::parse(&src).is_err());
    }

    #[test]
    fn match_mode_defaults_to_frontmatter() {
        let cfg = ProjectConfig::parse(SAMPLE).unwrap();
        assert_eq!(cfg.match_mode, MatchMode::Frontmatter);
    }

    #[test]
    fn rejects_empty_prefix() {
        let src = SAMPLE.replace("prefix = \"PERS\"", "prefix = \"\"");
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(err, CoreError::EmptyPrefix));
    }

    #[test]
    fn rejects_whitespace_only_prefix() {
        for raw in ["prefix = \" \"", "prefix = \"\\t \""] {
            let src = SAMPLE.replace("prefix = \"PERS\"", raw);
            let err =
                ProjectConfig::parse(&src).expect_err("a whitespace prefix renders keys as ` -1`");
            assert!(matches!(err, CoreError::EmptyPrefix), "{raw}: {err:?}");
        }
    }

    #[test]
    fn a_padded_prefix_is_stored_trimmed() {
        let src = SAMPLE.replace("prefix = \"PERS\"", "prefix = \"  PERS  \"");
        assert_eq!(ProjectConfig::parse(&src).unwrap().prefix, "PERS");
    }

    #[test]
    fn rejects_terminal_state_not_in_states() {
        let src = SAMPLE.replace(
            "terminal = [\"done\"]",
            "terminal = [\"done\", \"archived\"]",
        );
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(err, CoreError::UnknownState(ref s) if s == "archived"));
    }

    #[test]
    fn rejects_transition_referencing_unknown_state() {
        let src = SAMPLE.replace(
            "terminal = [\"done\"]",
            "terminal = [\"done\"]\n\n[workflow.transitions]\ntodo = [\"ghost\"]",
        );
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(err, CoreError::UnknownState(ref s) if s == "ghost"));
    }

    #[test]
    fn rejects_unknown_field_type() {
        let src = SAMPLE.replace("type = \"enum\"", "type = \"sting\"");
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(
            err,
            CoreError::UnknownFieldType { ref field, ref ty }
            if field == "category" && ty == "sting"
        ));
    }

    #[test]
    fn rejects_duplicate_field_names() {
        let src = format!("{SAMPLE}\n[[fields]]\nname = \"category\"\ntype = \"str\"\n");
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(err, CoreError::DuplicateFieldName(ref n) if n == "category"));
    }

    /// `choices` is the spelling a user reaches for unprompted. Reading only
    /// `values` turned it into an enum with no options — one that rejects
    /// every value with `expects one of:` and an empty list.
    #[test]
    fn an_enum_may_spell_its_options_choices() {
        let src = SAMPLE.replace("values = ", "choices = ");
        let cfg = ProjectConfig::parse(&src).unwrap();
        assert!(
            matches!(cfg.fields[0].ty, FieldType::Enum(ref v) if v == &["shopping", "admin"]),
            "{:?}",
            cfg.fields[0].ty
        );
    }

    #[test]
    fn rejects_an_enum_with_no_options() {
        let src = SAMPLE.replace(r#"values = ["shopping", "admin"]"#, "");
        let err =
            ProjectConfig::parse(&src).expect_err("an enum with no options can never be satisfied");
        assert!(
            matches!(err, CoreError::EmptyEnum(ref n) if n == "category"),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_an_enum_whose_options_list_is_empty() {
        let src = SAMPLE.replace(r#"values = ["shopping", "admin"]"#, "values = []");
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(
            matches!(err, CoreError::EmptyEnum(ref n) if n == "category"),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_empty_states_list() {
        let src = SAMPLE.replace("states = [\"todo\", \"doing\", \"done\"]", "states = []");
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(err, CoreError::EmptyWorkflow));
    }

    fn workflow_of(src: &str) -> Result<Workflow, CoreError> {
        let doc: toml_edit::DocumentMut = src.parse().unwrap();
        Workflow::from_toml_item(doc.get("workflow").unwrap())
    }

    #[test]
    fn a_standalone_workflow_table_parses_like_a_project_one() {
        let wf = workflow_of(
            r#"
[workflow]
states = ["todo", "doing", "done"]
initial = "todo"
terminal = ["done"]
"#,
        )
        .unwrap();
        assert_eq!(wf, ProjectConfig::parse(SAMPLE).unwrap().workflow);
    }

    #[test]
    fn a_standalone_workflow_table_is_validated() {
        let err = workflow_of("[workflow]\nstates = []\ninitial = \"todo\"\n").unwrap_err();
        assert!(matches!(err, CoreError::EmptyWorkflow));
        let err = workflow_of("[workflow]\nstates = [\"todo\"]\ninitial = \"nope\"\n").unwrap_err();
        assert!(
            matches!(err, CoreError::UnknownState(ref s) if s == "nope"),
            "{err:?}"
        );
        let err = workflow_of(
            "[workflow]\nstates = [\"todo\"]\ninitial = \"todo\"\nterminal = [\"x\"]\n",
        )
        .unwrap_err();
        assert!(
            matches!(err, CoreError::UnknownState(ref s) if s == "x"),
            "{err:?}"
        );
    }

    const WITH_TRANSITIONS: &str = r#"
[workflow]
states = ["todo", "doing", "done"]
initial = "todo"
terminal = ["done"]

[workflow.transitions]
todo = ["doing"]
doing = ["done"]
"#;

    #[test]
    fn write_into_round_trips_through_the_parser() {
        let wf = workflow_of(WITH_TRANSITIONS).unwrap();
        let mut doc = toml_edit::DocumentMut::new();
        doc["workflow"] = toml_edit::Item::Table(toml_edit::Table::new());
        wf.write_into(doc["workflow"].as_table_mut().unwrap());
        assert_eq!(workflow_of(&doc.to_string()).unwrap(), wf);
    }

    #[test]
    fn write_into_drops_an_emptied_transitions_table() {
        let mut wf = workflow_of(WITH_TRANSITIONS).unwrap();
        wf.transitions.clear();
        let mut doc: toml_edit::DocumentMut = WITH_TRANSITIONS.parse().unwrap();
        wf.write_into(doc["workflow"].as_table_mut().unwrap());
        let out = doc.to_string();
        assert!(!out.contains("transitions"), "{out}");
        assert_eq!(workflow_of(&out).unwrap(), wf);
    }

    #[test]
    fn write_into_leaves_unrelated_keys_and_comments_alone() {
        let src = "# keep me\n[workflow]\n# and me\nstates = [\"todo\"]\ninitial = \"todo\"\nmystery = 7\n";
        let mut doc: toml_edit::DocumentMut = src.parse().unwrap();
        let wf = Workflow {
            states: vec!["todo".into(), "done".into()],
            initial: "todo".into(),
            terminal: vec!["done".into()],
            transitions: Default::default(),
        };
        wf.write_into(doc["workflow"].as_table_mut().unwrap());
        let out = doc.to_string();
        assert!(out.contains("# keep me"), "{out}");
        assert!(out.contains("# and me"), "{out}");
        assert!(out.contains("mystery = 7"), "{out}");
        assert_eq!(workflow_of(&out).unwrap(), wf);
    }
}
