use std::path::{Path, PathBuf};

/// The one piece of local state that is NOT disposable: it is how a fresh
/// install finds your data (spec §3).
#[derive(Debug, Clone)]
pub struct Registry {
    pub root: PathBuf,
    pub projects: Vec<Project>,
    pub default: Option<String>,
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
        let root = Self::home();
        let path = Self::file(&root);
        let mut reg = Registry {
            root,
            projects: vec![],
            default: None,
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

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let mut out = String::new();
        if let Some(d) = &self.default {
            out.push_str(&format!("default = \"{d}\"\n\n"));
        }
        // Defence in depth alongside `upsert_project`: even if `self.projects`
        // somehow holds two entries for the same id, only the most recent one
        // is ever written — never two `[projects.<id>]` tables, which would
        // be invalid TOML and corrupt the file for every project, not just
        // the duplicated one. Order is first-seen, content is last-write-wins.
        let mut order: Vec<&str> = Vec::new();
        let mut latest: std::collections::BTreeMap<&str, &Project> =
            std::collections::BTreeMap::new();
        for p in &self.projects {
            if !latest.contains_key(p.id.as_str()) {
                order.push(p.id.as_str());
            }
            latest.insert(p.id.as_str(), p);
        }
        for id in order {
            let p = latest[id];
            out.push_str(&format!(
                "[projects.{}]\npath = \"{}\"\n\n",
                p.id,
                p.path.display()
            ));
        }
        std::fs::write(Self::file(&self.root), out)
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
