//! State source implementations for the responder.
//!
//! Two lifecycles, two impls of [`StateSource`]:
//! - [`RegistryWarmStateSource`] — the resident daemon: resolves a project
//!   (path or id) against the registry, serves its state resident-first and
//!   pages the checkpoint in on a miss.
//! - [`ColdStoreStateSource`] — the one-shot responder: loads state from disk
//!   per request, no registry and no cache.

use crate::flush::Flusher;
use crate::project::{Project, ProjectRegistry};
use crate::{Error, Result};
use filigrio_core::{GraphState, GraphStore};
use filigrio_query::GraphView;
use filigrio_store::FsStore;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::StateSource;

/// Cold store state source for one-shot responder.
///
/// This source uses direct FsStore access, loading state from disk on each request.
/// It's used by the one-shot responder which has no persistent cache.
pub struct ColdStoreStateSource {
    /// Base directory where projects are stored
    base_dir: PathBuf,
}

impl ColdStoreStateSource {
    /// Create a new cold store state source.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Get the output directory for a specific project.
    ///
    /// `pub` (not just crate-private) so the cold *write* path (the one-shot
    /// binary) resolves state to the exact same location this read path already
    /// does — one project-id-to-output-dir rule, not two that can drift.
    ///
    /// Must land in the same place the **resident** daemon does
    /// (`Project::new`'s `root.join(".filigrio-out")`) — `project_id` is
    /// normally an absolute cwd path (`Path::join` with an absolute argument
    /// replaces `base_dir` entirely, so this reduces to "trust the resolved
    /// root" for that common case, and falls back to `base_dir/project_id`
    /// for a short project name). Before this joined `.filigrio-out` too,
    /// `state.json` written by `filigrio build` (resident) was invisible to
    /// `filigrio query --no-daemon` (cold) and vice versa — same project,
    /// two different files, silently diverging.
    pub fn project_output_dir(&self, project_id: &str) -> PathBuf {
        self.base_dir.join(project_id).join(".filigrio-out")
    }

    /// Load state from disk for the given project.
    fn load_state(&self, project_id: &str) -> Result<GraphState> {
        let output_dir = self.project_output_dir(project_id);
        let store = FsStore::new(&output_dir);

        store
            .load_state()?
            .ok_or_else(|| Error::Storage(format!("no state found for project {}", project_id)))
    }
}

impl StateSource for ColdStoreStateSource {
    fn get_state(&self, project_id: &str) -> Result<Arc<GraphState>> {
        let state = self.load_state(project_id)?;
        Ok(Arc::new(state))
    }

    fn has_project(&self, project_id: &str) -> bool {
        let output_dir = self.project_output_dir(project_id);
        // Check if the state file exists
        output_dir.exists()
    }
}

/// Warm state source for the resident daemon: the **registry** resolves a
/// project name (path or id), and the flusher serves its state — resident when
/// hot, paged in from the checkpoint when not.
///
/// It holds the flusher rather than the cache alone, and that is the whole
/// point: registration is the registry's fact and residency is the cache's, and
/// while existence was read off the cache the two were conflated. A daemon
/// starts with an empty cache, so an auto-spawned one (ADR-0032f §6) declined
/// every graph query for a project it had just loaded from `registry.json` —
/// while `Status`, which resolves against the registry and pages state in,
/// answered for the same project in the same breath.
///
/// Resolution is the same path-or-id rule as
/// [`crate::daemon::Daemon::resolve_project_id`], in one place instead of an
/// injected closure duplicating it.
pub struct RegistryWarmStateSource {
    flusher: Arc<Flusher>,
    registry: Arc<Mutex<ProjectRegistry>>,
}

impl RegistryWarmStateSource {
    /// Create a warm state source over the daemon's flusher (which owns the
    /// LRU cache and the paging-in) and registry.
    pub fn new(flusher: Arc<Flusher>, registry: Arc<Mutex<ProjectRegistry>>) -> Self {
        Self { flusher, registry }
    }

    /// The registered project a caller's `project` string names — an exact id,
    /// or a path inside a registered root.
    fn registered(&self, input: &str) -> Option<Project> {
        let registry = self.registry.lock();
        registry
            .get(input)
            .or_else(|| registry.find_by_path(Path::new(input)))
            .cloned()
    }

    fn resolve(&self, input: &str) -> Result<Project> {
        self.registered(input)
            .ok_or_else(|| Error::ProjectNotFound(input.to_string()))
    }

    /// Resident state, or the checkpoint paged in (ADR-0032 §2).
    ///
    /// The cache is consulted **first** and the checkpoint probe only after: a
    /// project applied on the producer lane and not yet flushed (ADR-0042 B12)
    /// is resident with no `state.json` behind it, and probing first would call
    /// that never-indexed. A registered project with neither is reported as
    /// unindexed rather than answered with an empty graph — `FsStore` maps a
    /// missing checkpoint onto an empty `GraphState`, so serving that would put
    /// a fabricated zero where the honest answer is "run `project index`".
    fn state_of(&self, project: &Project) -> Result<Arc<GraphState>> {
        if let Some(state) = self.flusher.cache().lock().get_arc(&project.id) {
            return Ok(state);
        }
        if !FsStore::new(&project.output_dir).has_checkpoint() {
            return Err(Error::ProjectNotIndexed(project.id.clone()));
        }
        self.flusher.state_of(project)
    }
}

impl StateSource for RegistryWarmStateSource {
    /// The warm override of the default one-view-per-request build (audit §L1).
    ///
    /// The view is cached **in the same cache entry as the state it derives
    /// from**, so it cannot outlive that state: every path that replaces
    /// resident state (`load_into` after a flush-lane apply, `put` after a
    /// deferred one) or removes it (LRU eviction) replaces the whole entry.
    /// There is no invalidation call to forget, because there is no
    /// invalidation *step*.
    ///
    /// Construction happens outside the cache mutex and is published only if the
    /// entry still holds the exact state it was built from
    /// ([`ProjectStateCache::install_view`]) — a ~100 ms build at next.js scale
    /// must not be held under the one lock every project's apply passes through.
    fn get_view(&self, project_id: &str) -> Result<Arc<GraphView>> {
        let project = self.resolve(project_id)?;
        if let Some(view) = self.flusher.cache().lock().view(&project.id) {
            return Ok(view);
        }
        // Miss: build from the snapshot this request read.
        let state = self.state_of(&project)?;
        let view = Arc::new(GraphView::new(Arc::clone(&state)));
        self.flusher
            .cache()
            .lock()
            .install_view(&project.id, &state, Arc::clone(&view));
        Ok(view)
    }

    fn get_state(&self, project_id: &str) -> Result<Arc<GraphState>> {
        let project = self.resolve(project_id)?;
        self.state_of(&project)
    }

    /// Registration, and nothing else: the registry is the authority (ADR-0032
    /// §2). Whether the project's graph is *resident* is a separate question,
    /// answered — by paging it in if need be — when someone asks for state.
    fn has_project(&self, project_id: &str) -> bool {
        self.registered(project_id).is_some()
    }

    /// The registry *is* the accepted vocabulary for the `project` argument, so
    /// the responder can name it back when a caller guesses wrong.
    fn known_projects(&self) -> Vec<String> {
        self.registry
            .lock()
            .all()
            .into_iter()
            .map(|p| p.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cold_store_state_source() {
        let temp = TempDir::new().unwrap();
        let source = ColdStoreStateSource::new(temp.path().to_path_buf());

        // Test with non-existent project
        assert!(!source.has_project("nonexistent"));

        let result = source.get_state("nonexistent");
        // FsStore returns default state when file doesn't exist
        assert!(result.is_ok());
        let state = result.unwrap();
        // Should be an empty default state
        assert_eq!(state.graph.nodes.len(), 0);
        assert_eq!(state.graph.edges.len(), 0);
    }

    /// ADR-0032f §4/§6 — the cold (`--no-daemon`) and resident lifecycles must
    /// resolve the same project to the same `state.json`, or a `filigrio
    /// build` (resident) is invisible to a `filigrio query --no-daemon`
    /// (cold) run right after it, and vice versa.
    #[test]
    fn cold_output_dir_matches_resident_daemon_convention() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let source = ColdStoreStateSource::new(root.clone());

        // The CLI sends the resolved (absolute) cwd as `project` — the common case.
        let project_id = root.to_string_lossy().to_string();
        let cold_output_dir = source.project_output_dir(&project_id);

        let resident_output_dir = crate::project::Project::new(root).output_dir;
        assert_eq!(
            cold_output_dir, resident_output_dir,
            "one-shot and resident daemon must resolve the same project to the same state.json location"
        );
    }

    /// A warm source over a registry holding `project`, and a cache holding
    /// whatever `resident` says — the two facts the source must keep apart.
    fn warm_source(
        project: &Project,
        resident: Option<GraphState>,
    ) -> (RegistryWarmStateSource, Arc<Flusher>) {
        let cache = Arc::new(Mutex::new(crate::cache::ProjectStateCache::new(8)));
        if let Some(state) = resident {
            cache.lock().load_into(&project.id, state);
        }
        let flusher = Arc::new(Flusher::new(
            Arc::clone(&cache),
            crate::locks::ProjectLocks::new(),
            crate::flush::FlushConfig::default(),
            Arc::new(crate::flush::SystemClock),
        ));
        let registry = Arc::new(Mutex::new(ProjectRegistry::new()));
        registry.lock().add(project.clone()).unwrap();
        (
            RegistryWarmStateSource::new(Arc::clone(&flusher), registry),
            flusher,
        )
    }

    /// The warm source resolves a registered project by id AND by a path inside
    /// its root — the same rule the daemon's command path uses.
    #[test]
    fn warm_source_resolves_by_id_and_path() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());
        let (source, _flusher) = warm_source(&project, Some(GraphState::default()));

        assert!(
            source.has_project(&project.id),
            "must resolve by registry id"
        );
        assert!(
            source.has_project(&temp.path().join("src/lib.rs").to_string_lossy()),
            "must resolve by a path under the project root"
        );
        assert!(!source.has_project("unregistered"));
        assert!(source.get_state(&project.id).is_ok());
    }

    /// **Registration is not residency.** A registered project whose graph is
    /// not in the LRU cache is still registered — the state every daemon starts
    /// in, and the one an auto-spawned daemon answers its first request from.
    #[test]
    fn warm_source_reports_a_registered_project_that_is_not_resident() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());
        let (source, _flusher) = warm_source(&project, None);

        assert!(
            source.has_project(&project.id),
            "an empty cache made a registered project look unregistered"
        );
        assert!(source.has_project(&temp.path().join("src/lib.rs").to_string_lossy()));
    }

    /// A cold cache must page the checkpoint in, not refuse the read: the state
    /// is on disk and the registry says whose it is.
    #[test]
    fn warm_source_pages_a_registered_project_in_from_disk() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());
        let mut on_disk = GraphState::default();
        on_disk
            .graph
            .nodes
            .push(filigrio_core::Node::new("fn:a", "a", "function"));
        std::fs::create_dir_all(&project.output_dir).unwrap();
        filigrio_store::FsStore::new(&project.output_dir)
            .save_state(&on_disk)
            .unwrap();

        let (source, _flusher) = warm_source(&project, None);
        let state = source.get_state(&project.id).expect("cold read");
        assert_eq!(
            state.graph.nodes.len(),
            1,
            "the checkpoint on disk must answer a cold read"
        );
    }

    /// Registered with no checkpoint at all is *not* an empty graph and *not* an
    /// unregistered project: `FsStore` reads a missing `state.json` as an empty
    /// `GraphState`, so serving it would answer a graph query with a confident
    /// zero. It names the verb that fixes it instead.
    #[test]
    fn warm_source_distinguishes_never_indexed_from_never_registered() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());
        let (source, _flusher) = warm_source(&project, None);

        let err = source.get_state(&project.id).expect_err("no checkpoint");
        assert!(matches!(err, Error::ProjectNotIndexed(_)), "{err}");
        assert!(err.to_string().contains("project index"), "{err}");

        let err = source
            .get_state("never-registered")
            .expect_err("no registry entry");
        assert!(matches!(err, Error::ProjectNotFound(_)), "{err}");
    }

    /// Resident-but-never-persisted is the ordering trap: a producer-lane apply
    /// (ADR-0042 B12 write-behind) leaves state in the cache with no
    /// `state.json` behind it. Probing the checkpoint before the cache would
    /// call that project unindexed while the daemon is serving its graph.
    #[test]
    fn warm_source_serves_resident_state_that_has_never_been_flushed() {
        let temp = TempDir::new().unwrap();
        let project = Project::new(temp.path().to_path_buf());
        let mut resident = GraphState::default();
        resident
            .graph
            .nodes
            .push(filigrio_core::Node::new("fn:a", "a", "function"));
        let (source, _flusher) = warm_source(&project, Some(resident));

        assert!(
            !filigrio_store::FsStore::new(&project.output_dir).has_checkpoint(),
            "precondition: nothing has been persisted"
        );
        let state = source.get_state(&project.id).expect("resident read");
        assert_eq!(state.graph.nodes.len(), 1);
    }
}
