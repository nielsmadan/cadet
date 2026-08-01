use crate::frontmatter::{parse_frontmatter, render_list, splice};
use crate::probe::{Probe, probe};
use crate::slug::slugify;
use cadet_core::{
    Backend, BackendError, ChangeSet, Cursor, FieldType, FieldValue, Observed, Priority,
    ProjectConfig, Revision, Snapshot, Task, TaskKey, TaskUid, revision,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct FsBackend {
    root: PathBuf,
    config: OnceLock<ProjectConfig>,
}

/// Renders a `FieldValue` to its frontmatter text form. Mirrors how `tags`
/// is written: a list becomes an inline `[a, b, c]` array, via the same
/// `render_list` that quotes an item containing a comma so it round-trips
/// through `Frontmatter::list` as one item, not two.
fn render_field_value(v: &FieldValue) -> String {
    match v {
        FieldValue::Str(s) | FieldValue::Date(s) => s.clone(),
        FieldValue::Int(i) => i.to_string(),
        FieldValue::Float(f) => f.to_string(),
        FieldValue::Bool(b) => b.to_string(),
        FieldValue::List(items) => render_list(items),
    }
}

/// Coerces a raw frontmatter scalar to the `FieldValue` variant its declared
/// `FieldType` demands. A value that does not parse as its declared type is
/// left as `FieldValue::Str` so the file stays readable and `validate_task`
/// reports the type error, rather than the read failing outright.
///
/// `FieldType::ListStr` is handled by the caller (it needs `Frontmatter`,
/// not just the raw scalar, to read a block-list form), so it is never
/// passed here.
fn coerce_field(ty: &FieldType, raw: &str) -> FieldValue {
    match ty {
        FieldType::Int => raw
            .parse::<i64>()
            .map(FieldValue::Int)
            .unwrap_or_else(|_| FieldValue::Str(raw.to_string())),
        FieldType::Float => raw
            .parse::<f64>()
            .map(FieldValue::Float)
            .unwrap_or_else(|_| FieldValue::Str(raw.to_string())),
        FieldType::Bool => raw
            .parse::<bool>()
            .map(FieldValue::Bool)
            .unwrap_or_else(|_| FieldValue::Str(raw.to_string())),
        FieldType::Date | FieldType::DateTime => FieldValue::Date(raw.to_string()),
        FieldType::Str | FieldType::Text | FieldType::Enum(_) => FieldValue::Str(raw.to_string()),
        FieldType::ListStr => FieldValue::Str(raw.to_string()),
    }
}

impl FsBackend {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            config: OnceLock::new(),
        }
    }

    fn io<E: std::fmt::Display>(e: E) -> BackendError {
        BackendError::Io(e.to_string())
    }

    /// Cached project config: parsed from `project.toml` at most once per
    /// process, since `read_task` needs it (to coerce every custom field to
    /// its declared type) on every file read a scan touches.
    fn config(&self) -> Result<&ProjectConfig, BackendError> {
        if let Some(c) = self.config.get() {
            return Ok(c);
        }
        let cfg = self.load_project()?;
        Ok(self.config.get_or_init(|| cfg))
    }

    fn markdown_files(&self) -> Result<Vec<PathBuf>, BackendError> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).map_err(Self::io)? {
                let entry = entry.map_err(Self::io)?;
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "md") {
                    out.push(path);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn read_task(&self, path: &Path) -> Result<Option<Task>, BackendError> {
        if probe(path).map_err(Self::io)? != Probe::Task {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(path).map_err(Self::io)?;
        let Some(fm) = parse_frontmatter(&raw) else {
            return Ok(None);
        };
        let ts = |k: &str| {
            fm.get(k)
                .and_then(|s| s.parse::<jiff::Timestamp>().ok())
                .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        };
        let uid =
            fm.get("uid")
                .and_then(TaskUid::parse)
                .ok_or_else(|| BackendError::Malformed {
                    path: path.display().to_string(),
                    reason: "missing or invalid uid".into(),
                })?;
        let cfg = self.config()?;
        // Only a field declared in `project.toml` enters `task.fields`. A
        // key the project has not declared is not Cadet's to manage: it is
        // never read into memory, so `put`'s removal loop can never decide
        // to delete it, whatever shape it has on disk (scalar, block list,
        // or nested map). It also means a hand-added note-to-self key can
        // never trip `validate_task`'s `UnknownField`, since it never
        // becomes a field at all.
        let mut fields = BTreeMap::new();
        for def in &cfg.fields {
            let k = def.name.as_str();
            if matches!(def.ty, FieldType::ListStr) {
                if fm.keys().any(|fk| fk == k) {
                    fields.insert(k.to_string(), FieldValue::List(fm.list(k)));
                }
            } else if let Some(v) = fm.get(k) {
                fields.insert(k.to_string(), coerce_field(&def.ty, v));
            }
        }
        Ok(Some(Task {
            uid,
            key: fm
                .get("key")
                .and_then(TaskKey::parse)
                .unwrap_or(TaskKey::new("?", 0)),
            title: fm.get("title").unwrap_or_default().to_string(),
            state: fm.get("state").unwrap_or("todo").to_string(),
            created: ts("created"),
            updated: ts("updated"),
            due: fm.get("due").map(str::to_string),
            priority: match fm.get("priority") {
                Some("high") => Priority::High,
                Some("low") => Priority::Low,
                _ => Priority::Normal,
            },
            tags: fm.list("tags"),
            renumbered_from: fm.get("renumbered_from").and_then(TaskKey::parse),
            possible_duplicate_of: fm.get("possible_duplicate_of").and_then(TaskUid::parse),
            fields,
            body: fm.body,
        }))
    }

    fn path_for(&self, uid: &TaskUid) -> Result<Option<PathBuf>, BackendError> {
        for p in self.markdown_files()? {
            if let Some(t) = self.read_task(&p)?
                && &t.uid == uid
            {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }

    /// Atomic: write to a temp file in the same directory, then rename.
    fn write_atomic(path: &Path, contents: &str) -> Result<(), BackendError> {
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, contents).map_err(Self::io)?;
        std::fs::rename(&tmp, path).map_err(Self::io)?;
        Ok(())
    }
}

impl Backend for FsBackend {
    fn load_project(&self) -> Result<ProjectConfig, BackendError> {
        let src = std::fs::read_to_string(self.root.join("project.toml")).map_err(Self::io)?;
        ProjectConfig::parse(&src).map_err(|e| BackendError::Malformed {
            path: "project.toml".into(),
            reason: e.to_string(),
        })
    }

    fn save_project(&self, _cfg: ProjectConfig) -> Result<(), BackendError> {
        Err(BackendError::Io(
            "save_project is not implemented in milestone 1".into(),
        ))
    }

    fn get(&self, uid: TaskUid) -> Result<Option<Task>, BackendError> {
        match self.path_for(&uid)? {
            Some(p) => self.read_task(&p),
            None => Ok(None),
        }
    }

    fn put(&self, task: Task, expected: Option<Revision>) -> Result<Revision, BackendError> {
        let existing = self.path_for(&task.uid)?;
        // A caller holding `expected` believes the task already exists at
        // some known revision. If it is not on disk at all (e.g. it was
        // concurrently deleted), that belief is stale — resurrecting the
        // task instead of reporting the mismatch would silently discard the
        // deletion. `delete` already treats "not found" this strictly;
        // `put` must match it rather than falling through to a create.
        match (&expected, &existing) {
            (Some(want), Some(path)) => {
                let current = self.read_task(path)?.ok_or(BackendError::NotFound)?;
                if &revision(&current) != want {
                    return Err(BackendError::RevisionMismatch);
                }
            }
            (Some(_), None) => return Err(BackendError::RevisionMismatch),
            (None, _) => {}
        }
        // Once a task has a file, `put` never re-slugifies it even if the
        // title changes: an Obsidian vault (or any other note) may already
        // link to this exact filename, and Cadet has no way to find and
        // rewrite those inbound links. The slug is a cosmetic, one-time
        // filename choice at creation (spec §4); renaming later would rot
        // links Cadet never touched in the first place, so `put` always
        // writes back to `existing` when a path is already known.
        let path = match existing {
            Some(p) => p,
            None => {
                // Slugs are NOT unique. An entirely non-Latin title slugifies to
                // `untitled`, so two Japanese-titled tasks would land on the same
                // filename and the second `put` would overwrite the first — silent
                // data loss. Two ordinary titles can collide the same way.
                let base = slugify(&task.title);
                let mut candidate = self.root.join(format!("{base}.md"));
                if candidate.exists() {
                    candidate = self.root.join(format!("{base}-{}.md", task.key));
                }
                candidate
            }
        };
        let base = std::fs::read_to_string(&path).unwrap_or_default();
        let mut edits: Vec<(String, Option<String>)> = vec![
            ("uid".to_string(), Some(task.uid.as_str().to_string())),
            ("key".to_string(), Some(task.key.to_string())),
            ("title".to_string(), Some(task.title.clone())),
            ("state".to_string(), Some(task.state.clone())),
            ("created".to_string(), Some(task.created.to_string())),
            ("updated".to_string(), Some(task.updated.to_string())),
            ("due".to_string(), task.due.clone()),
            (
                "priority".to_string(),
                Some(
                    match task.priority {
                        Priority::High => "high",
                        Priority::Normal => "normal",
                        Priority::Low => "low",
                    }
                    .to_string(),
                ),
            ),
            (
                "tags".to_string(),
                if task.tags.is_empty() {
                    None
                } else {
                    Some(render_list(&task.tags))
                },
            ),
            (
                "renumbered_from".to_string(),
                task.renumbered_from.as_ref().map(TaskKey::to_string),
            ),
        ];
        // A *declared* custom-field key that is on disk but absent from
        // `task.fields` must be removed, or deleting a field in memory
        // would never delete it on disk (`splice` only touches keys it is
        // told about). An undeclared key is never Cadet's to delete,
        // whatever its shape — `read_task` never reads it into
        // `task.fields` in the first place (see the comment there), so it
        // must never be a candidate for removal here either.
        let cfg = self.config()?;
        if let Some(fm) = parse_frontmatter(&base) {
            for def in &cfg.fields {
                let k = def.name.as_str();
                if task.fields.contains_key(k) {
                    continue;
                }
                // Only a value `read_task` could actually have read is a
                // candidate for removal. A declared field whose on-disk shape
                // does not match its declared type (a scalar written as a
                // block list, say) never reaches `task.fields`, so it is
                // indistinguishable here from one the caller deleted — and
                // deleting it would be silent data loss on an ordinary state
                // change. Preserve it untouched, exactly like an undeclared
                // key.
                let readable = if matches!(def.ty, FieldType::ListStr) {
                    fm.keys().any(|fk| fk == k)
                } else {
                    fm.get(k).is_some()
                };
                if readable {
                    edits.push((k.to_string(), None));
                }
            }
        }
        for (name, value) in &task.fields {
            edits.push((name.clone(), Some(render_field_value(value))));
        }
        let edit_refs: Vec<(&str, Option<String>)> =
            edits.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let mut out = splice(&base, &edit_refs);
        if base.is_empty() && !task.body.is_empty() {
            out.push_str(&task.body);
        }
        Self::write_atomic(&path, &out)?;
        Ok(revision(&task))
    }

    fn delete(&self, uid: TaskUid, expected: Option<Revision>) -> Result<(), BackendError> {
        let path = self.path_for(&uid)?.ok_or(BackendError::NotFound)?;
        if let Some(want) = expected {
            let current = self.read_task(&path)?.ok_or(BackendError::NotFound)?;
            if revision(&current) != want {
                return Err(BackendError::RevisionMismatch);
            }
        }
        std::fs::remove_file(path).map_err(Self::io)
    }

    fn adopt(
        &self,
        path: String,
        uid: TaskUid,
        key: TaskKey,
        now: jiff::Timestamp,
    ) -> Result<Task, BackendError> {
        let full = self.root.join(&path);
        let raw = std::fs::read_to_string(&full).map_err(Self::io)?;
        let created = parse_frontmatter(&raw)
            .and_then(|fm| fm.get("created").map(str::to_string))
            .unwrap_or_else(|| now.to_string());
        let spliced = splice(
            &raw,
            &[
                ("uid", Some(uid.as_str().to_string())),
                ("key", Some(key.to_string())),
                ("created", Some(created)),
                ("updated", Some(now.to_string())),
            ],
        );
        Self::write_atomic(&full, &spliced)?;
        self.read_task(&full)?.ok_or(BackendError::NotFound)
    }

    fn scan(&self, _since: Option<Cursor>) -> Result<ChangeSet, BackendError> {
        // A broken or unreadable `project.toml` is a genuine failure and
        // must error out — unlike an individual task file below, there is
        // no meaningful way to "skip" the project config. Loading it here
        // also warms the cache once, up front, so no per-file read_task call
        // below can independently fail on it.
        self.config()?;
        let mut observed = Vec::new();
        let mut tasks = BTreeMap::new();
        let mut complete = true;
        for p in self.markdown_files()? {
            let rel = p
                .strip_prefix(&self.root)
                .unwrap_or(&p)
                .to_string_lossy()
                .to_string();
            // A single locked file, cloud-sync permission glitch or stray
            // ACL must not take down every read in the project — treat it
            // the same as a cloud placeholder: mark the snapshot incomplete
            // and move on, rather than aborting the whole scan.
            let probed = match probe(&p) {
                Ok(v) => v,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            match probed {
                Probe::NotATask => continue,
                // A placeholder means the tree is not fully materialised: the
                // snapshot must not be used to infer deletion (§5 guard 1).
                Probe::NotMaterialised => {
                    complete = false;
                    continue;
                }
                Probe::Task => {}
            }
            // A task-shaped file whose uid is missing is not an error here — it
            // is a candidate for adoption, so record it with `uid: None`.
            match self.read_task(&p) {
                Ok(Some(t)) => {
                    observed.push(Observed {
                        uid: Some(t.uid.clone()),
                        path: rel.clone(),
                        revision: revision(&t),
                    });
                    tasks.insert(rel, t);
                }
                Ok(None) => {}
                Err(BackendError::Malformed { .. }) => match std::fs::read_to_string(&p) {
                    Ok(raw) => {
                        observed.push(Observed {
                            uid: None,
                            path: rel,
                            revision: Revision::from_raw(
                                blake3::hash(raw.as_bytes()).to_hex().to_string(),
                            ),
                        });
                    }
                    Err(_) => {
                        complete = false;
                    }
                },
                Err(BackendError::Io(_)) => {
                    complete = false;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(ChangeSet::Snapshot {
            snapshot: Snapshot { complete, observed },
            tasks,
        })
    }
}
