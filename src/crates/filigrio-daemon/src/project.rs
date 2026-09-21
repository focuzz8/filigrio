//! Project registry (ADR-0032 §2).
//!
//! Manages registered projects and their per-project state.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Failure loading a registry from disk (ADR-0032 §2 registry-persist).
///
/// The distinction is deliberate: an **absent** file is the first-ever run and is
/// NOT an error (→ empty registry), but a **corrupt** file IS — silently starting
/// empty would drop every registered project without a trace.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry file is corrupt: {0}")]
    Corrupt(#[from] serde_json::Error),
}

/// The on-disk registry document. Projects are stored as an order-independent
/// list (they are re-keyed by id on load), with a `version` for forward migration.
#[derive(Debug, Serialize, Deserialize)]
struct RegistryDoc {
    version: u32,
    projects: Vec<Project>,
}

/// A registered project.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    /// Project identifier (derived from path).
    pub id: String,
    /// Root path of the project.
    pub root: PathBuf,
    /// Output directory for state.json.
    pub output_dir: PathBuf,
    /// Explicit per-project watch mode (ADR-0042 F6b), persisted with the
    /// registry. Default OFF: a newly registered project is cold by contract —
    /// `serve` startup reconciles + watches only `watch == true` projects.
    /// `#[serde(default)]` so every pre-F6b registry file loads as unwatched.
    #[serde(default)]
    pub watch: bool,
}

impl Project {
    /// Create a new project from a root path (watch mode OFF, ADR-0042 F6b).
    pub fn new(root: PathBuf) -> Self {
        let id = Self::project_id(&root);
        let output_dir = root.join(".filigrio-out");
        Project {
            id,
            root,
            output_dir,
            watch: false,
        }
    }

    /// Generate a project ID from the path.
    fn project_id(root: &Path) -> String {
        root.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    }
}

/// The **one** "you named a project I don't have" message (ADR-0032b OQ4).
///
/// Every path that can decline for want of a registration formats it here — the
/// data plane's queries, `Status`, and each control-plane verb. They used to
/// spell one condition three ways (`project not found` from `Status`, `project
/// not registered` from `Submit`, a third and longer text from the responder),
/// and both an agent and the `integration status` hook diagnostic quote
/// whichever they hit verbatim: two spellings read as two conditions.
///
/// It carries only what a caller can act on — the ids that ARE registered, the
/// verb that fixes it, the two accepted forms of the argument, and the warning
/// that `project_graph`'s rows are ADR-0019 sub-projects *inside* one indexed
/// repo rather than values for this field (the collision an agent-eval run
/// spent four of its ten steps on).
///
/// `known` empty means "no ids to offer": a source without a registry (the
/// one-shot cold store, which resolves a project by looking for a directory)
/// genuinely cannot enumerate, and that is not the same claim as "nothing is
/// registered".
pub(crate) fn unregistered_project_message(project: &str, known: &[String]) -> String {
    // Bounded: a long registry must not turn one wrong argument into a page of
    // text the model then has to read past.
    const MAX_LISTED: usize = 20;
    let mut ids: Vec<&str> = known.iter().map(String::as_str).collect();
    ids.sort_unstable();
    let listed = if ids.is_empty() {
        String::new()
    } else {
        let shown = ids
            .iter()
            .take(MAX_LISTED)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        match ids.len().saturating_sub(MAX_LISTED) {
            0 => format!(" Registered projects: {shown}."),
            more => format!(" Registered projects: {shown} (+{more} more)."),
        }
    };
    format!(
        "project not registered: '{project}'.{listed} \
         Register it with `filigrio project register` from the project root. \
         Valid values are a registered project id or an absolute path inside its root; \
         omit the `project` field entirely to use the current directory. \
         Names and roots from `project_graph` are sub-projects *within* one indexed repo, \
         not values for `project`."
    )
}

/// Registry of projects.
#[derive(Clone, Debug, Default)]
pub struct ProjectRegistry {
    projects: HashMap<String, Project>,
}

impl ProjectRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a project to the registry.
    pub fn add(&mut self, project: Project) -> Result<(), String> {
        if self.projects.contains_key(&project.id) {
            return Err(format!("project already exists: {}", project.id));
        }
        self.projects.insert(project.id.clone(), project);
        Ok(())
    }

    /// Remove a project from the registry.
    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        self.projects
            .remove(id)
            .ok_or_else(|| format!("project not registered: {}", id))
            .map(|_| ())
    }

    /// Get a project by ID.
    pub fn get(&self, id: &str) -> Option<&Project> {
        self.projects.get(id)
    }

    /// Get a project by path.
    pub fn find_by_path(&self, path: &Path) -> Option<&Project> {
        self.projects.values().find(|p| path.starts_with(&p.root))
    }

    /// Get all projects.
    pub fn all(&self) -> Vec<&Project> {
        self.projects.values().collect()
    }

    /// Set a project's persisted watch mode (ADR-0042 F6b). Errors if the
    /// project is not registered. The caller persists via `save_to`.
    pub fn set_watch(&mut self, id: &str, watch: bool) -> Result<(), String> {
        match self.projects.get_mut(id) {
            Some(p) => {
                p.watch = watch;
                Ok(())
            }
            None => Err(format!("project not registered: {}", id)),
        }
    }

    /// Get the number of registered projects.
    pub fn count(&self) -> usize {
        self.projects.len()
    }

    /// Load a registry from `path` (ADR-0032 §2 registry-persist).
    ///
    /// An **absent** file yields an empty registry (first-ever run — never a
    /// startup crash). A **present but corrupt** file is a hard error, so a
    /// truncated write can't silently erase every registered project.
    pub fn load_from(path: &Path) -> Result<Self, RegistryError> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            // First-ever run: no file yet is expected, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(RegistryError::Io(e)),
        };
        // Present but unparseable → loud error (never silently drop projects).
        let doc: RegistryDoc = serde_json::from_slice(&bytes)?;
        let mut registry = Self::default();
        for project in doc.projects {
            registry.projects.insert(project.id.clone(), project);
        }
        Ok(registry)
    }

    /// Persist the registry to `path` atomically (ADR-0032 §2 registry-persist).
    ///
    /// Creates the parent directory if missing and writes via a temp file +
    /// rename so a crash mid-write can never corrupt the previous good file
    /// (rename is atomic on the same filesystem).
    pub fn save_to(&self, path: &Path) -> Result<(), RegistryError> {
        let mut projects: Vec<Project> = self.projects.values().cloned().collect();
        // Stable on-disk order (id) → deterministic file, friendlier to diffs.
        projects.sort_by(|a, b| a.id.cmp(&b.id));
        let doc = RegistryDoc {
            version: 1,
            projects,
        };
        let json = serde_json::to_vec_pretty(&doc)?;
        filigrio_store::atomic::atomic_write(path, &json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_add_project() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());

        let mut registry = ProjectRegistry::new();
        registry.add(project.clone()).unwrap();

        assert_eq!(registry.count(), 1);
        assert!(registry.get(&project.id).is_some());
    }

    #[test]
    fn test_duplicate_project() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());

        let mut registry = ProjectRegistry::new();
        registry.add(project.clone()).unwrap();
        let result = registry.add(project);

        assert!(result.is_err());
    }

    #[test]
    fn test_find_by_path() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());

        let mut registry = ProjectRegistry::new();
        registry.add(project).unwrap();

        let found = registry.find_by_path(&temp.path().join("src/main.rs"));
        assert!(found.is_some());

        let not_found = registry.find_by_path(Path::new("/other/path"));
        assert!(not_found.is_none());
    }
}
