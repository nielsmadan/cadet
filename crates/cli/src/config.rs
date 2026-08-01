use std::path::{Path, PathBuf};

/// The one piece of local state that is NOT disposable: it is how a fresh
/// install finds your data (spec §3).
#[derive(Debug, Clone)]
pub struct Registry {
    pub root: PathBuf,
    pub projects: Vec<Project>,
    pub default: Option<String>,
    pub project_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: String,
    pub path: PathBuf,
}

impl Registry {
    pub fn home() -> PathBuf {
        if let Ok(h) = std::env::var("CADET_HOME") {
            return PathBuf::from(h);
        }
        directories::ProjectDirs::from("", "", "cadet")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".cadet"))
    }

    fn file(root: &Path) -> PathBuf {
        root.join("config.toml")
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
        if let Some(tbl) = doc.get("projects").and_then(|p| p.as_table_like()) {
            for (id, item) in tbl.iter() {
                if let Some(p) = item.get("path").and_then(|v| v.as_str()) {
                    reg.projects.push(Project {
                        id: id.to_string(),
                        path: PathBuf::from(p),
                    });
                }
            }
        }
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
    /// Inserting into a `Table` also subsumes the old duplicate-id guard:
    /// re-inserting an id overwrites in place, so two entries sharing one id
    /// can never render as two `[projects.<id>]` tables. Order is first-seen,
    /// content is last-write-wins.
    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let mut doc = toml_edit::DocumentMut::new();
        if let Some(d) = &self.default {
            doc["default"] = toml_edit::value(d.as_str());
        }
        if let Some(pr) = &self.project_root {
            doc["project_root"] = toml_edit::value(pr.to_string_lossy().as_ref());
        }
        let mut projects = toml_edit::Table::new();
        projects.set_implicit(true);
        for p in &self.projects {
            let mut entry = toml_edit::Table::new();
            entry["path"] = toml_edit::value(p.path.to_string_lossy().as_ref());
            projects.insert(&p.id, toml_edit::Item::Table(entry));
        }
        doc["projects"] = toml_edit::Item::Table(projects);
        std::fs::write(Self::file(&self.root), doc.to_string())
    }

    // Unused outside tests until Task 8 wires these into the `project` CLI
    // commands; `cadet-cli` is a bin-only crate, so `pub` alone doesn't
    // silence rustc's dead_code lint the way it would in a lib crate.
    #[allow(dead_code)]
    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    #[allow(dead_code)]
    pub fn set_project_root(&mut self, p: PathBuf) {
        self.project_root = Some(p);
    }

    #[allow(dead_code)]
    pub fn set_default(&mut self, id: &str) -> Result<(), String> {
        if !self.projects.iter().any(|p| p.id == id) {
            return Err(format!("unknown project `{id}`"));
        }
        self.default = Some(id.to_string());
        Ok(())
    }

    /// Clearing the default matters as much as removing the entry: a dangling
    /// `default` makes every later command fail with "no default project set",
    /// and there is no command that repairs it.
    #[allow(dead_code)]
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

    pub fn active(&self, requested: Option<&str>) -> Option<&Project> {
        let id = requested
            .map(str::to_string)
            .or_else(|| std::env::var("CADET_PROJECT").ok())
            .or_else(|| self.default.clone())?;
        self.projects.iter().find(|p| p.id == id)
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
                },
                Project {
                    id: "p".into(),
                    path: PathBuf::from("/new"),
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
        });
        reg.upsert_project(Project {
            id: "b".into(),
            path: "/tmp/b".into(),
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
        });
        reg.save().unwrap();
        let again = Registry::load_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(again.projects[0].path, odd);
    }
}
