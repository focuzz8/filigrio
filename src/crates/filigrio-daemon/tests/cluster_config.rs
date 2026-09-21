//! The daemon's apply path must honor `DaemonConfig::cluster_cfg` (ADR-0024).
//!
//! The CLI cold build threads its `ClusterConfig` into the pipeline
//! (`Pipeline::new(..).with_cluster_config(..)`), but the daemon's apply path
//! used to construct the pipeline with NO cluster config — so a project
//! cold-built with a non-default config silently degraded to default `Simple`
//! clustering on every incremental daemon apply. These tests drive a real apply
//! through `Daemon::apply_project` and assert the configured clustering is what
//! actually ran.

use filigrio_core::{ChangeSet, GraphStore};
use filigrio_daemon::{Daemon, DaemonConfig, Project};
use filigrio_pipeline::{ClusterConfig, ClusterStrategy};
use filigrio_store::FsStore;
use std::fs;
use tempfile::TempDir;

/// A tiny crate whose cross-file call resolves, so default clustering has an
/// edge to merge across (boot → greet ⇒ fewer communities than nodes).
fn write_fixture(root: &std::path::Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod api;\npub mod handlers;\n").unwrap();
    fs::write(root.join("src/api.rs"), "pub fn greet() -> u32 { 1 }\n").unwrap();
    fs::write(
        root.join("src/handlers.rs"),
        "use crate::api::greet;\npub fn boot() -> u32 { greet() }\n",
    )
    .unwrap();
}

/// Build a daemon whose socket/registry/shutdown-marker paths are all isolated
/// into `tmp` (a daemon test must NEVER touch the real `~/.config` registry),
/// register the fixture project, and run one apply through the daemon path.
fn daemon_apply(tmp: &TempDir, cluster_cfg: ClusterConfig) -> filigrio_core::GraphState {
    let root = tmp.path().join("repo");
    write_fixture(&root);
    let project = Project::new(root);
    fs::create_dir_all(&project.output_dir).unwrap();

    let config = DaemonConfig {
        socket_path: tmp.path().join("daemon.sock"),
        shutdown_marker_path: tmp.path().join("daemon.clean"),
        registry_path: tmp.path().join("registry.json"),
        idle_timeout: None,
        cluster_cfg,
        ..DaemonConfig::default()
    };
    let daemon = Daemon::new(config);
    daemon.registry.lock().add(project.clone()).unwrap();

    let changeset = ChangeSet {
        added: vec![
            "Cargo.toml".to_string(),
            "src/lib.rs".to_string(),
            "src/api.rs".to_string(),
            "src/handlers.rs".to_string(),
        ],
        modified: vec![],
        removed: vec![],
    };
    daemon.apply_project(&project.id, &changeset).unwrap();

    FsStore::new(&project.output_dir)
        .load_state()
        .unwrap()
        .expect("daemon apply must persist state.json")
}

#[test]
fn daemon_apply_honors_a_non_default_cluster_config() {
    // Sanity control first: under the DEFAULT config the fixture's resolved
    // call/contains edges merge nodes, so communities < clustered nodes. This
    // is what the buggy daemon produced regardless of config. (The clustered
    // node count is `partition.node_community.len()` — the `project:` node is
    // not part of the partition.)
    let tmp = TempDir::new().unwrap();
    let state = daemon_apply(&tmp, ClusterConfig::default());
    let clustered = state.partition.node_community.len();
    assert!(clustered > 1, "fixture produces a multi-node partition");
    assert!(
        state.partition.communities.len() < clustered,
        "default clustering merges connected nodes ({} communities / {} clustered nodes)",
        state.partition.communities.len(),
        clustered
    );

    // The observable non-default config: Full strategy at a resolution high
    // enough that NO merge has positive modularity gain — every node stays a
    // singleton. The default Simple/1.0 config can never produce this on a
    // connected fixture, so communities == nodes proves the daemon threaded
    // `DaemonConfig::cluster_cfg` into its apply pipeline.
    let tmp = TempDir::new().unwrap();
    let cfg = ClusterConfig {
        strategy: ClusterStrategy::Full,
        resolution: 100.0,
        ..ClusterConfig::default()
    };
    let state = daemon_apply(&tmp, cfg);
    let clustered = state.partition.node_community.len();
    assert!(clustered > 1, "fixture produces a multi-node partition");
    assert_eq!(
        state.partition.communities.len(),
        clustered,
        "high-resolution config must reach the apply's clustering (all singletons); \
         fewer communities means the daemon ignored DaemonConfig::cluster_cfg"
    );
}
