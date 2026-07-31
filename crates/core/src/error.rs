use crate::model::TaskKey;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("config parse error: {0}")]
    ConfigParse(String),
    #[error("`{0}` is a reserved field name")]
    ReservedFieldName(String),
    #[error("state `{0}` is not declared in the workflow")]
    UnknownState(String),
    #[error("cannot move from `{from}` to `{to}`")]
    IllegalTransition { from: String, to: String },
    #[error("unknown field `{0}`")]
    UnknownField(String),
    #[error("field `{field}` expects {expected}")]
    FieldType { field: String, expected: String },
    #[error("key {0} already in use")]
    DuplicateKey(TaskKey),
    #[error("project prefix must not be empty")]
    EmptyPrefix,
    #[error("workflow has no states")]
    EmptyWorkflow,
    #[error("unknown field type `{ty}` for field `{field}`")]
    UnknownFieldType { field: String, ty: String },
    #[error("duplicate field name `{0}`")]
    DuplicateFieldName(String),
    #[error("task title must not be empty")]
    EmptyTitle,
    #[error("key prefix `{found}` does not belong to this project (expected `{expected}`)")]
    ForeignKeyPrefix { expected: String, found: String },
    #[error("field `{field}` is not a valid {expected}: `{value}`")]
    InvalidDateValue {
        field: String,
        expected: String,
        value: String,
    },
}
