use std::path::{Path, PathBuf};

/// The project id the environment selects, if any. A blank value counts as
/// unset: `CADET_PROJECT=` left in a shell profile means "no override", not
/// "select the project with the empty name".
pub fn env_project() -> Option<String> {
    std::env::var("CADET_PROJECT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Writes `dirs` into a project table, or removes the key when there are none.
/// Shared by `save`'s two branches — the existing-table and new-table paths —
/// because writing this twice is how a `dirs = []` gets left behind in one of
/// them and not the other.
fn write_dirs(tbl: &mut toml_edit::Table, dirs: &[PathBuf]) {
    if dirs.is_empty() {
        tbl.remove("dirs");
        return;
    }
    let mut arr = toml_edit::Array::new();
    for d in dirs {
        arr.push(d.to_string_lossy().as_ref());
    }
    tbl["dirs"] = toml_edit::value(arr);
}

/// A directory named by an environment variable. A blank or whitespace-only
/// value counts as unset — `CADET_HOME=` left in a shell profile means "no
/// override", not "put the registry in the current working directory". Same
/// rule `env_project` applies, for the same reason.
fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
}

/// Split out from `home` so it is testable without mutating process
/// environment, which is `unsafe` in edition 2024 and racy under a parallel
/// test runner.
fn resolve_home(
    cadet_home: Option<PathBuf>,
    xdg_config: Option<PathBuf>,
    home: Option<PathBuf>,
) -> PathBuf {
    if let Some(h) = cadet_home {
        return h;
    }
    if let Some(x) = xdg_config {
        return x.join("cadet");
    }
    home.map(|h| h.join(".config").join("cadet"))
        .unwrap_or_else(|| PathBuf::from(".cadet"))
}

/// The one piece of local state that is NOT disposable: it is how a fresh
/// install finds your data (spec §3).
#[derive(Debug, Clone)]
pub struct Registry {
    pub root: PathBuf,
    pub projects: Vec<Project>,
    pub default: Option<String>,
    pub project_root: Option<PathBuf>,
    /// The workflow new projects are created with. `None` — the state of
    /// every registry written before this key existed — means the built-in
    /// template, so an untouched registry behaves exactly as it did.
    pub workflow: Option<cadet_core::Workflow>,
    /// Defaults for new tasks in every project. A project's own `[defaults]`
    /// wins over this — unlike `[workflow]`, which is copied once at project
    /// creation, because at creation time there is no project config to
    /// consult and at task creation time there is.
    pub defaults: cadet_core::Defaults,
    // The document `load_from` parsed, kept around so `save` can mutate it
    // in place instead of building a fresh one from scratch — that's what
    // lets unknown top-level keys, and unknown keys inside a `[projects.x]`
    // table, survive a load/save round trip instead of being silently
    // dropped. `None` when there was nothing to load (fresh registry).
    doc: Option<toml_edit::DocumentMut>,
}

/// Which backend a project's data lives in. Absent in the file means
/// `Markdown` — the only backend that existed before this field did, so a
/// registry with no `backend` key must keep working untouched. An
/// unrecognised value is a loud error rather than a silent default: pointing
/// a local-db project at a directory that does not exist reports a
/// confusing I/O error instead of the real problem (spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    #[default]
    Markdown,
    LocalDb,
}

impl BackendKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendKind::Markdown => "markdown",
            BackendKind::LocalDb => "local-db",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "markdown" => Some(BackendKind::Markdown),
            "local-db" => Some(BackendKind::LocalDb),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: String,
    pub path: PathBuf,
    pub backend: BackendKind,
    /// Directories whose contents belong to this project, as glob patterns.
    /// A command run from a matching directory selects this project without
    /// `--project`. Empty for every project that has never configured one,
    /// which is what keeps an existing registry behaving exactly as before.
    pub dirs: Vec<PathBuf>,
}

impl Project {
    /// Where this project's hand-editable config lives: inside the folder
    /// for `markdown` (`path` names the folder), in the sibling `.toml` next
    /// to the storage file for `local-db` (`path` names the `.db` itself —
    /// see `LocalDbBackend::open`). Shared by `project::add`, which writes
    /// this file, and `load_config` in `main.rs`, which reads it back, so
    /// the two halves of this rule can't drift apart.
    pub fn config_path(&self) -> PathBuf {
        match self.backend {
            BackendKind::Markdown => self.path.join("project.toml"),
            BackendKind::LocalDb => self.path.with_extension("toml"),
        }
    }
}

impl Registry {
    /// `CADET_HOME`, then `$XDG_CONFIG_HOME/cadet`, then `~/.config/cadet`.
    ///
    /// Deliberately not `directories::ProjectDirs`, which implements Apple's
    /// HIG and resolves to `~/Library/Application Support/cadet` on macOS —
    /// correct for a GUI app, wrong for a CLI. `git`, `gh`, `starship` and
    /// `helix` all use `~/.config` on every platform, and that is where a
    /// user looks for a file they might hand-edit.
    pub fn home() -> PathBuf {
        resolve_home(
            env_dir("CADET_HOME"),
            env_dir("XDG_CONFIG_HOME"),
            std::env::home_dir(),
        )
    }

    fn file(root: &Path) -> PathBuf {
        root.join("config.toml")
    }

    // Process-unique so two `cadet` processes saving against the same root
    // don't clobber each other's in-flight write: without the pid, one of
    // them renames the other's content into place and reports `Ok(())` for
    // data that was never its own. This doesn't make concurrent saves
    // coordinated — last-rename-wins is still the outcome, and that's fine
    // for a single-user CLI — it just stops a save from silently reporting
    // success while persisting someone else's write.
    fn tmp_file(root: &Path) -> PathBuf {
        Self::file(root).with_extension(format!("toml.tmp.{}", std::process::id()))
    }

    pub fn load() -> std::io::Result<Self> {
        Self::load_from(Self::home())
    }

    pub fn load_from(root: PathBuf) -> std::io::Result<Self> {
        let path = Self::file(&root);
        let mut reg = Registry {
            root,
            projects: vec![],
            default: None,
            project_root: None,
            workflow: None,
            defaults: Default::default(),
            doc: None,
        };
        let Ok(src) = std::fs::read_to_string(&path) else {
            return Ok(reg);
        };
        // A malformed registry must never silently become an empty one — that
        // would make every project it lists invisible, including ones that
        // had nothing to do with whatever corrupted the file. Fail loudly
        // and name the file and the problem, the same fix already applied to
        // `high_water` in Task 10.
        let doc: toml_edit::DocumentMut = src.parse().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("malformed registry at {}: {e}", path.display()),
            )
        })?;
        reg.default = doc
            .get("default")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        reg.project_root = doc
            .get("project_root")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        // Validated on load, loudly, for the same reason a malformed registry
        // is: a default workflow that is quietly ignored gets discovered when
        // a project created from it turns out to have the built-in states
        // instead, long after the typo.
        if let Some(item) = doc.get("defaults") {
            reg.defaults = cadet_core::Defaults::from_toml_item(item).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("[defaults] in {}: {e}", path.display()),
                )
            })?;
        }
        if let Some(item) = doc.get("workflow") {
            reg.workflow = Some(cadet_core::Workflow::from_toml_item(item).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("[workflow] in {}: {e}", path.display()),
                )
            })?);
        }
        if let Some(tbl) = doc.get("projects").and_then(|p| p.as_table_like()) {
            for (id, item) in tbl.iter() {
                if let Some(p) = item.get("path").and_then(|v| v.as_str()) {
                    let backend = match item.get("backend").and_then(|v| v.as_str()) {
                        None => BackendKind::Markdown,
                        Some(raw) => BackendKind::parse(raw).ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "project `{id}` in {}: unknown backend `{raw}` — expected `markdown` or `local-db`",
                                    path.display()
                                ),
                            )
                        })?,
                    };
                    let dirs = item
                        .get("dirs")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .map(PathBuf::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    reg.projects.push(Project {
                        id: id.to_string(),
                        path: PathBuf::from(p),
                        backend,
                        dirs,
                    });
                }
            }
        }
        reg.doc = Some(doc);
        Ok(reg)
    }

    /// Replaces an existing project entry with the same id rather than
    /// appending a duplicate. `Registry::save` writes one TOML table per
    /// entry in `self.projects`; two entries sharing an id would serialize
    /// as two `[projects.<id>]` tables, which is invalid TOML and corrupts
    /// the file the moment `load` tries to parse it back.
    pub fn upsert_project(&mut self, project: Project) {
        if let Some(existing) = self.projects.iter_mut().find(|p| p.id == project.id) {
            *existing = project;
        } else {
            self.projects.push(project);
        }
    }

    /// Built with `toml_edit`, never string formatting: a vault path
    /// containing a quote or a backslash — or a project id that needs a
    /// quoted key — would otherwise produce a file `load` correctly refuses
    /// to parse, locking the user out of every project at once. This is the
    /// one piece of local state that is not disposable.
    ///
    /// Mutates the `DocumentMut` `load_from` parsed (falling back to a fresh
    /// one only when there was nothing to load), rather than building a new
    /// document from scratch, so unknown top-level keys and unknown keys
    /// inside a `[projects.x]` table — content written by a newer binary
    /// this one doesn't understand — survive a load/save round trip instead
    /// of being silently erased. Reinserting a project id overwrites its
    /// `path` in place, so two entries sharing one id can never render as
    /// two `[projects.<id>]` tables.
    ///
    /// Writes to a process-unique temp file next to the target and renames
    /// it into place — the same pattern `backend-markdown` uses for its
    /// (disposable) task files — rather than truncating `config.toml`
    /// directly: a crash mid-write must never leave this file half-written
    /// and unparseable, which would lock the user out of every project at
    /// once. The pid in the temp name keeps two concurrent `cadet`
    /// processes from clobbering each other's in-flight write (see
    /// `tmp_file`); if the rename fails, the temp file is removed rather
    /// than left behind.
    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let mut doc = self.doc.clone().unwrap_or_default();

        match &self.default {
            Some(d) => doc["default"] = toml_edit::value(d.as_str()),
            None => {
                doc.remove("default");
            }
        }
        match &self.project_root {
            Some(pr) => doc["project_root"] = toml_edit::value(pr.to_string_lossy().as_ref()),
            None => {
                doc.remove("project_root");
            }
        }
        if self.defaults.is_empty() {
            doc.remove("defaults");
        } else {
            if !matches!(doc.get("defaults"), Some(item) if item.is_table()) {
                doc["defaults"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            self.defaults.write_into(
                doc["defaults"]
                    .as_table_mut()
                    .expect("just ensured this is a table"),
            );
        }
        match &self.workflow {
            Some(wf) => {
                if !matches!(doc.get("workflow"), Some(item) if item.is_table()) {
                    doc["workflow"] = toml_edit::Item::Table(toml_edit::Table::new());
                }
                wf.write_into(
                    doc["workflow"]
                        .as_table_mut()
                        .expect("just ensured this is a table"),
                );
            }
            None => {
                doc.remove("workflow");
            }
        }

        let has_projects_table = matches!(doc.get("projects"), Some(item) if item.is_table());
        if !has_projects_table {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            doc["projects"] = toml_edit::Item::Table(t);
        }
        let projects = doc["projects"]
            .as_table_mut()
            .expect("just ensured this is a table");

        let keep: Vec<&str> = self.projects.iter().map(|p| p.id.as_str()).collect();
        let stale: Vec<String> = projects
            .iter()
            .map(|(id, _)| id.to_string())
            .filter(|id| !keep.contains(&id.as_str()))
            .collect();
        for id in &stale {
            projects.remove(id);
        }
        for p in &self.projects {
            match projects.get_mut(&p.id).and_then(|item| item.as_table_mut()) {
                Some(existing) => {
                    existing["path"] = toml_edit::value(p.path.to_string_lossy().as_ref());
                    existing["backend"] = toml_edit::value(p.backend.as_str());
                    write_dirs(existing, &p.dirs);
                }
                None => {
                    let mut entry = toml_edit::Table::new();
                    entry["path"] = toml_edit::value(p.path.to_string_lossy().as_ref());
                    entry["backend"] = toml_edit::value(p.backend.as_str());
                    write_dirs(&mut entry, &p.dirs);
                    projects.insert(&p.id, toml_edit::Item::Table(entry));
                }
            }
        }

        let target = Self::file(&self.root);
        let tmp = Self::tmp_file(&self.root);
        std::fs::write(&tmp, doc.to_string())?;
        match std::fs::rename(&tmp, &target) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub fn set_project_root(&mut self, p: PathBuf) {
        self.project_root = Some(p);
    }

    pub fn set_default(&mut self, id: &str) -> Result<(), String> {
        if !self.projects.iter().any(|p| p.id == id) {
            return Err(format!(
                "unknown project `{id}` — configured project(s): {}",
                self.known_projects()
            ));
        }
        self.default = Some(id.to_string());
        Ok(())
    }

    /// A comma-separated list of every configured project id, or `(none)` —
    /// shared by every "unknown project" error so a typo always comes back
    /// with the same list a user could have checked with `cadet project`.
    pub fn known_projects(&self) -> String {
        if self.projects.is_empty() {
            return "(none)".to_string();
        }
        self.projects
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Clearing the default matters as much as removing the entry: a dangling
    /// `default` makes every later command fail with "no default project set",
    /// and there is no command that repairs it.
    pub fn remove_project(&mut self, id: &str) -> bool {
        let before = self.projects.len();
        self.projects.retain(|p| p.id != id);
        if self.projects.len() == before {
            return false;
        }
        if self.default.as_deref() == Some(id) {
            self.default = self.projects.first().map(|p| p.id.clone());
        }
        true
    }

    pub fn find(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// The registry default, and only that. `--project` and `CADET_PROJECT`
    /// are resolved by the caller (see `main.rs`) so that both spellings of
    /// "select this project" run through one lookup with one error, instead
    /// of the env spelling falling through to "no default project set" —
    /// which is false when a default is set, and names the wrong thing to
    /// fix.
    pub fn default_project(&self) -> Option<&Project> {
        self.find(self.default.as_deref()?)
    }

    pub fn repo_dir(&self, project: &str) -> PathBuf {
        self.root.join("repos").join(format!("{project}.git"))
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(root: &Path, projects: Vec<Project>, default: Option<&str>) -> Registry {
        Registry {
            root: root.to_path_buf(),
            projects,
            default: default.map(str::to_string),
            project_root: None,
            workflow: None,
            defaults: Default::default(),
            doc: None,
        }
    }

    /// A path containing a quote or a backslash is legal on every filesystem
    /// Cadet targets, and `load` is (correctly) a hard error on a malformed
    /// registry — so an unescaped write here locks the user out of every
    /// project they have, not just this one.
    #[test]
    fn a_path_with_quotes_and_backslashes_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let awkward = PathBuf::from(r#"/tmp/say "hi"\back\slash"#);
        registry(
            dir.path(),
            vec![Project {
                id: "personal".into(),
                path: awkward.clone(),
                backend: BackendKind::Markdown,
                dirs: vec![],
            }],
            Some("personal"),
        )
        .save()
        .unwrap();

        let loaded = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(loaded.default.as_deref(), Some("personal"));
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].path, awkward);
    }

    #[test]
    fn a_project_id_needing_a_quoted_key_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        registry(
            dir.path(),
            vec![Project {
                id: "my project".into(),
                path: PathBuf::from("/tmp/v"),
                backend: BackendKind::Markdown,
                dirs: vec![],
            }],
            None,
        )
        .save()
        .unwrap();

        let loaded = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].id, "my project");
    }

    #[test]
    fn a_duplicate_id_is_written_once_with_the_latest_path() {
        let dir = tempfile::tempdir().unwrap();
        registry(
            dir.path(),
            vec![
                Project {
                    id: "p".into(),
                    path: PathBuf::from("/old"),
                    backend: BackendKind::Markdown,
                    dirs: vec![],
                },
                Project {
                    id: "p".into(),
                    path: PathBuf::from("/new"),
                    backend: BackendKind::Markdown,
                    dirs: vec![],
                },
            ],
            None,
        )
        .save()
        .unwrap();

        let loaded = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].path, PathBuf::from("/new"));
    }

    #[test]
    fn project_root_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        reg.set_project_root(PathBuf::from("/tmp/notes"));
        reg.save().unwrap();

        let again = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(again.project_root(), Some(Path::new("/tmp/notes")));
    }

    #[test]
    fn removing_the_default_project_promotes_another() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        reg.upsert_project(Project {
            id: "a".into(),
            path: "/tmp/a".into(),
            backend: BackendKind::Markdown,
            dirs: vec![],
        });
        reg.upsert_project(Project {
            id: "b".into(),
            path: "/tmp/b".into(),
            backend: BackendKind::Markdown,
            dirs: vec![],
        });
        reg.set_default("a").unwrap();

        assert!(reg.remove_project("a"));
        assert_eq!(reg.default.as_deref(), Some("b"));
        assert_eq!(reg.projects.len(), 1);
    }

    #[test]
    fn removing_the_last_project_clears_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        reg.upsert_project(Project {
            id: "only".into(),
            path: "/tmp/only".into(),
            backend: BackendKind::Markdown,
            dirs: vec![],
        });
        reg.set_default("only").unwrap();
        assert!(reg.remove_project("only"));
        assert_eq!(reg.default, None);
    }

    #[test]
    fn removing_a_project_that_is_not_there_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert!(!reg.remove_project("ghost"));
    }

    #[test]
    fn set_default_rejects_an_unknown_project() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert!(reg.set_default("ghost").is_err());
    }

    #[test]
    fn a_path_with_quotes_and_backslashes_round_trips_with_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        let odd = PathBuf::from(r#"/tmp/say "hi"\back"#);
        reg.upsert_project(Project {
            id: "odd".into(),
            path: odd.clone(),
            backend: BackendKind::Markdown,
            dirs: vec![],
        });
        reg.save().unwrap();
        let again = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(again.projects[0].path, odd);
    }

    /// Guards against a future regression where `load` or `save` starts
    /// globbing `config.toml*` (e.g. to "recover" from a previous crash) and
    /// picks up a leftover temp file from an interrupted `save`. A save that
    /// crashes before its rename must leave the last good `config.toml`
    /// exactly as it was.
    #[test]
    fn a_stale_tmp_file_from_an_interrupted_save_does_not_corrupt_the_real_file() {
        let dir = tempfile::tempdir().unwrap();
        registry(
            dir.path(),
            vec![Project {
                id: "a".into(),
                path: "/tmp/a".into(),
                backend: BackendKind::Markdown,
                dirs: vec![],
            }],
            Some("a"),
        )
        .save()
        .unwrap();

        // Simulate a crash between the temp write and the rename: a
        // truncated, unparseable `.tmp` sibling left next to a valid file.
        std::fs::write(dir.path().join("config.toml.tmp"), "default = \"a").unwrap();

        let loaded = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(loaded.default.as_deref(), Some("a"));
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].path, PathBuf::from("/tmp/a"));
    }

    #[test]
    fn unknown_keys_survive_a_load_save_cycle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "future_key = \"something\"\n\n[projects.a]\npath = \"/tmp/a\"\nfuture_project_key = \"x\"\n",
        )
        .unwrap();

        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        reg.upsert_project(Project {
            id: "a".into(),
            path: "/tmp/a-new".into(),
            backend: BackendKind::Markdown,
            dirs: vec![],
        });
        reg.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(raw.contains("future_key"), "raw file:\n{raw}");
        assert!(raw.contains("future_project_key"), "raw file:\n{raw}");

        let again = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(again.projects.len(), 1);
        assert_eq!(again.projects[0].path, PathBuf::from("/tmp/a-new"));
    }

    /// Two `cadet` processes saving against the same root must not share a
    /// temp file name — a shared name lets one process's rename silently
    /// carry the other's content into `config.toml` while both `save()`
    /// calls report success (see the coordinator's interleaving probe on
    /// the prior fix round).
    #[test]
    fn the_temp_file_name_contains_the_current_process_id() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = Registry::tmp_file(dir.path());
        assert!(
            tmp.to_string_lossy()
                .contains(&std::process::id().to_string()),
            "tmp path was {tmp:?}"
        );
    }

    #[test]
    fn no_temp_file_remains_after_a_successful_save() {
        let dir = tempfile::tempdir().unwrap();
        registry(
            dir.path(),
            vec![Project {
                id: "a".into(),
                path: "/tmp/a".into(),
                backend: BackendKind::Markdown,
                dirs: vec![],
            }],
            Some("a"),
        )
        .save()
        .unwrap();

        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "leftover temp files: {leftover:?}");
    }

    #[test]
    fn cadet_home_wins_over_everything() {
        assert_eq!(
            resolve_home(
                Some(PathBuf::from("/explicit")),
                Some(PathBuf::from("/xdg")),
                Some(PathBuf::from("/home/u")),
            ),
            PathBuf::from("/explicit")
        );
    }

    #[test]
    fn xdg_config_home_is_used_when_cadet_home_is_unset() {
        assert_eq!(
            resolve_home(
                None,
                Some(PathBuf::from("/xdg")),
                Some(PathBuf::from("/home/u"))
            ),
            PathBuf::from("/xdg/cadet")
        );
    }

    #[test]
    fn falls_back_to_dot_config_under_home() {
        assert_eq!(
            resolve_home(None, None, Some(PathBuf::from("/home/u"))),
            PathBuf::from("/home/u/.config/cadet")
        );
    }

    #[test]
    fn with_no_home_at_all_it_stays_relative_rather_than_guessing() {
        assert_eq!(resolve_home(None, None, None), PathBuf::from(".cadet"));
    }

    #[test]
    fn a_blank_env_var_counts_as_unset() {
        // `CADET_HOME=` in a shell profile must not put the registry in $PWD.
        assert_eq!(env_dir("CADET_HOME_DEFINITELY_UNSET_XYZ"), None);
    }

    #[test]
    fn a_project_with_no_backend_key_defaults_to_markdown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "default = \"a\"\n\n[projects.a]\npath = \"/tmp/a\"\n",
        )
        .unwrap();
        let reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(reg.projects[0].backend, BackendKind::Markdown);
    }

    #[test]
    fn the_backend_kind_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        reg.upsert_project(Project {
            id: "scratch".into(),
            path: "/tmp/scratch.db".into(),
            backend: BackendKind::LocalDb,
            dirs: vec![],
        });
        reg.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(raw.contains("backend = \"local-db\""), "{raw}");

        let again = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(again.projects[0].backend, BackendKind::LocalDb);
    }

    #[test]
    fn a_registry_with_no_workflow_key_never_grows_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "default = \"a\"\n\n[projects.a]\npath = \"/tmp/a\"\n",
        )
        .unwrap();
        let reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert!(reg.workflow.is_none());
        reg.save().unwrap();
        let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(!raw.contains("workflow"), "{raw}");
    }

    #[test]
    fn a_default_workflow_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load_from(dir.path().to_path_buf()).unwrap();
        let wf = cadet_core::Workflow {
            states: vec!["todo".into(), "blocked".into(), "done".into()],
            initial: "todo".into(),
            terminal: vec!["done".into()],
            transitions: Default::default(),
        };
        reg.workflow = Some(wf.clone());
        reg.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(raw.contains("[workflow]"), "{raw}");

        let again = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(again.workflow, Some(wf));
    }

    #[test]
    fn a_malformed_default_workflow_is_a_loud_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[workflow]\nstates = [\"todo\"]\ninitial = \"nope\"\n",
        )
        .unwrap();
        let err = Registry::load_from(dir.path().to_path_buf()).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn an_unknown_backend_kind_is_a_loud_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[projects.a]\npath = \"/tmp/a\"\nbackend = \"telepathy\"\n",
        )
        .unwrap();
        let err = Registry::load_from(dir.path().to_path_buf()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("telepathy"),
            "the error must name the value: {msg}"
        );
    }
}
