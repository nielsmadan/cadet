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

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectConfig {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub match_mode: MatchMode,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub workflow: Workflow,
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

        let str_vec = |item: &toml_edit::Item, k: &str| -> Vec<String> {
            item.get(k)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };

        let workflow = Workflow {
            states: str_vec(wf_item, "states"),
            initial: get(wf_item, "initial"),
            terminal: str_vec(wf_item, "terminal"),
            transitions: wf_item
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

        if workflow.states.is_empty() {
            return Err(CoreError::EmptyWorkflow);
        }
        if !workflow.states.contains(&workflow.initial) {
            return Err(CoreError::UnknownState(workflow.initial));
        }
        for state in &workflow.terminal {
            if !workflow.states.contains(state) {
                return Err(CoreError::UnknownState(state.clone()));
            }
        }
        for (from, allowed) in &workflow.transitions {
            if !workflow.states.contains(from) {
                return Err(CoreError::UnknownState(from.clone()));
            }
            for to in allowed {
                if !workflow.states.contains(to) {
                    return Err(CoreError::UnknownState(to.clone()));
                }
            }
        }

        let prefix = get(project, "prefix");
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
                    "enum" => FieldType::Enum(
                        t.get("values")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    ),
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

    #[test]
    fn rejects_empty_states_list() {
        let src = SAMPLE.replace("states = [\"todo\", \"doing\", \"done\"]", "states = []");
        let err = ProjectConfig::parse(&src).unwrap_err();
        assert!(matches!(err, CoreError::EmptyWorkflow));
    }
}
