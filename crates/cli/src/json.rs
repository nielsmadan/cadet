use cadet_core::{FieldValue, Task};
use cadet_store_sqlite::TaskSummary;
use serde::Serialize;
use std::collections::BTreeMap;

const SCHEMA_VERSION: u8 = 1;

#[derive(Serialize)]
pub struct ListOutput {
    schema_version: u8,
    tasks: Vec<TaskSummaryOutput>,
}

impl ListOutput {
    pub fn new(tasks: Vec<TaskSummaryOutput>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            tasks,
        }
    }
}

#[derive(Serialize)]
pub struct ShowOutput {
    schema_version: u8,
    task: TaskOutput,
}

impl ShowOutput {
    pub fn new(task: TaskOutput) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            task,
        }
    }
}

#[derive(Serialize)]
pub struct TaskSummaryOutput {
    project: String,
    uid: String,
    key: String,
    title: String,
    state: String,
    priority: &'static str,
    due: Option<String>,
    tags: Vec<String>,
    fields: BTreeMap<String, JsonFieldValue>,
}

impl TaskSummaryOutput {
    pub fn from_summary(project: &str, task: &TaskSummary) -> Self {
        Self {
            project: project.to_string(),
            uid: task.uid.clone(),
            key: task.key.to_string(),
            title: task.title.clone(),
            state: task.state.clone(),
            priority: task.priority.as_str(),
            due: task.due.clone(),
            tags: task.tags.clone(),
            fields: fields(&task.fields),
        }
    }
}

#[derive(Serialize)]
pub struct TaskOutput {
    project: String,
    uid: String,
    key: String,
    title: String,
    state: String,
    priority: &'static str,
    due: Option<String>,
    tags: Vec<String>,
    fields: BTreeMap<String, JsonFieldValue>,
    created: String,
    updated: String,
    renumbered_from: Option<String>,
    possible_duplicate_of: Option<String>,
    body: String,
}

impl TaskOutput {
    pub fn from_task(project: &str, task: &Task) -> Self {
        Self {
            project: project.to_string(),
            uid: task.uid.as_str().to_string(),
            key: task.key.to_string(),
            title: task.title.clone(),
            state: task.state.clone(),
            priority: task.priority.as_str(),
            due: task.due.clone(),
            tags: task.tags.clone(),
            fields: fields(&task.fields),
            created: task.created.to_string(),
            updated: task.updated.to_string(),
            renumbered_from: task.renumbered_from.as_ref().map(ToString::to_string),
            possible_duplicate_of: task
                .possible_duplicate_of
                .as_ref()
                .map(|uid| uid.as_str().to_string()),
            body: task.body.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum JsonFieldValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<String>),
}

fn fields(values: &BTreeMap<String, FieldValue>) -> BTreeMap<String, JsonFieldValue> {
    values
        .iter()
        .map(|(name, value)| {
            let value = match value {
                FieldValue::Str(value) | FieldValue::Date(value) => {
                    JsonFieldValue::Str(value.clone())
                }
                FieldValue::Int(value) => JsonFieldValue::Int(*value),
                FieldValue::Float(value) => JsonFieldValue::Float(*value),
                FieldValue::Bool(value) => JsonFieldValue::Bool(*value),
                FieldValue::List(value) => JsonFieldValue::List(value.clone()),
            };
            (name.clone(), value)
        })
        .collect()
}
