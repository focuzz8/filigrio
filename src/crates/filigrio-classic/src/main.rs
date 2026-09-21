//! filigrio — the CLI driver. Wires the ports for each subcommand. `anyhow` at
//! this boundary (library errors convert in via `?`).
//!
//! Resource-grouped command tree (ADR-0038 — grouped-only, no aliases; `--store`
//! is a directory, default `.filigrio`):
//! * `graph build <path>` — cold/warm build into a store.
//! * `graph import <graph.json>` — ingest a Python graphify graph (ADR-0017).
//! * `graph query <text>` — load a store, run a traversal.
//! * `graph god [--top N]` / `graph path <src> <dst>` — analysis over the graph.
//! * `project list` — the monorepo architecture map (was `projects`).
//! * `serve` — MCP server over stdio; the endpoint an agent connects to, bridging
//!   to the daemon (ADR-0032d). The one non-grouped top-level command; everything
//!   else is `filigrio <resource> <verb>`.
//!
//! `graph report` is **not here**: it moved to the thin client (`filigrio graph
//! report` → `DataQuery::GraphReport`, rendered daemon-side). Two
//! implementations of one verb differing in *freshness* — store-direct here vs
//! daemon-resident there — is the audit's §D duplication class, and the
//! divergence would be invisible in a markdown file nobody diffs.

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use filigrio_client_mcp::McpServer;
use filigrio_core::{GraphDelta, GraphQuery, GraphStore, QueryOpts};
use filigrio_index::DispatchExtractor;
use filigrio_ingest::FsSource;
use filigrio_pipeline::{ClusterConfig, ClusterStrategy, EdgeWeighting, Pipeline};
use filigrio_query::GraphView;
use filigrio_store::{graphjson, FsStore};
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "filigrio",
    version,
    about = "filigrio — walking-skeleton (mock) pipeline"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// CLI mirror of `ClusterStrategy` (keeps `clap` out of the resolve crate).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ClusterArg {
    Simple,
    Full,
}

/// CLI mirror of `EdgeWeighting`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum WeightArg {
    Uniform,
    Confidence,
}

impl ClusterArg {
    fn to_config(self, weight: WeightArg, resolution: f64) -> ClusterConfig {
        ClusterConfig {
            strategy: match self {
                ClusterArg::Simple => ClusterStrategy::Simple,
                ClusterArg::Full => ClusterStrategy::Full,
            },
            weighting: match weight {
                WeightArg::Uniform => EdgeWeighting::Uniform,
                WeightArg::Confidence => EdgeWeighting::Confidence,
            },
            resolution,
        }
    }
}

// Top level of the resource-grouped CLI (ADR-0038): `filigrio <resource> <verb>`.
//
// Every command is `filigrio <resource> <verb>` — no top-level verbs, no aliases
// (ADR-0038, grouped-only). Each resource is a nested `#[derive(Subcommand)]` enum,
// so adding a new resource group (e.g. `index`/`client`/`hooks` as their ADRs land)
// is a one-line variant here.
#[derive(Subcommand)]
enum Command {
    /// Operate on the code graph: build, import, query, analyse.
    Graph {
        #[command(subcommand)]
        cmd: GraphCmd,
    },
    /// Operate on projects within the graph (the monorepo architecture map).
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Operate the MCP server — the endpoint an agent connects to.
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
}

/// Verbs on the `graph` resource.
#[derive(Subcommand)]
enum GraphCmd {
    /// Ingest a directory and build/update the graph in a store.
    Build(BuildArgs),
    /// Import a Python graphify `graph.json` into a store (migration on-ramp).
    Import(ImportArgs),
    /// Query a previously built store.
    Query(QueryArgs),
    /// Show the most-connected ("god") nodes.
    God(GodArgs),
    /// Shortest directed path between two node labels.
    Path(PathArgs),
}

/// Verbs on the `project` resource.
#[derive(Subcommand)]
enum ProjectCmd {
    /// List the project dependency graph (the monorepo architecture map).
    List(ProjectListArgs),
}

/// Verbs on the `server` resource.
#[derive(Subcommand)]
enum McpCmd {
    /// Run an MCP server over stdio — the endpoint an agent connects to; it holds no
    /// graph, bridging each tool call to the daemon (ADR-0032d).
    Serve(ServeArgs),
}

// ---- arg structs (one per command) ----

#[derive(clap::Args, Clone, Debug, PartialEq)]
struct BuildArgs {
    path: PathBuf,
    #[arg(long, default_value = ".filigrio")]
    store: PathBuf,
    /// Allow the build to shrink the graph (bypass the shrink-guard).
    #[arg(long)]
    force: bool,
    /// Clustering strategy (ADR-0024): `simple` (single-level, incremental,
    /// default) or `full` (multi-level Louvain).
    #[arg(long, value_enum, default_value_t = ClusterArg::Simple)]
    cluster: ClusterArg,
    /// Edge weighting for clustering (ADR-0024): `uniform` (default) or
    /// `confidence` (down-weight AMBIGUOUS homonym bridges).
    #[arg(long = "cluster-weight", value_enum, default_value_t = WeightArg::Uniform)]
    cluster_weight: WeightArg,
    /// Modularity resolution: `>1.0` → more/smaller communities.
    #[arg(long, default_value_t = 1.0)]
    resolution: f64,
}

#[derive(clap::Args, Clone, Debug, PartialEq)]
struct ImportArgs {
    file: PathBuf,
    #[arg(long, default_value = ".filigrio")]
    store: PathBuf,
}

#[derive(clap::Args, Clone, Debug, PartialEq)]
struct QueryArgs {
    text: String,
    #[arg(long, default_value = ".filigrio")]
    store: PathBuf,
    #[arg(long, default_value_t = 2)]
    depth: usize,
    #[arg(long, default_value_t = 32)]
    budget: usize,
}

#[derive(clap::Args, Clone, Debug, PartialEq)]
struct GodArgs {
    #[arg(long, default_value = ".filigrio")]
    store: PathBuf,
    #[arg(long, default_value_t = 10)]
    top: usize,
}

#[derive(clap::Args, Clone, Debug, PartialEq)]
struct ProjectListArgs {
    #[arg(long, default_value = ".filigrio")]
    store: PathBuf,
}

#[derive(clap::Args, Clone, Debug, PartialEq)]
struct PathArgs {
    src: String,
    dst: String,
    #[arg(long, default_value = ".filigrio")]
    store: PathBuf,
    #[arg(long, default_value_t = 8)]
    max_hops: usize,
}

#[derive(clap::Args, Clone, Debug, PartialEq)]
struct ServeArgs {
    #[arg(long, default_value = ".filigrio")]
    store: PathBuf,
}

fn main() -> Result<()> {
    // Grouped-only (ADR-0038): every command is `<resource> <verb>`, no top-level
    // verbs and no aliases — pre-1.0, no compat shim. The old flat verbs (`god`/
    // `projects`/`import`/`report`/`path`/`serve`) are gone from the clap tree;
    // unknown-subcommand handling is left to clap (its standard error + a `--help`
    // hint, exit 2).
    let cli = Cli::parse();
    dispatch(cli.command)
}

/// Route a parsed `<resource> <verb>` command to its handler.
fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Graph { cmd } => dispatch_graph(cmd),
        Command::Project { cmd } => dispatch_project(cmd),
        Command::Mcp { cmd } => dispatch_server(cmd),
    }
}

fn dispatch_graph(cmd: GraphCmd) -> Result<()> {
    match cmd {
        GraphCmd::Build(a) => cmd_build(
            a.path,
            a.store,
            a.force,
            a.cluster.to_config(a.cluster_weight, a.resolution),
        ),
        GraphCmd::Import(a) => cmd_import(a.file, a.store),
        GraphCmd::Query(a) => cmd_query(a.text, a.store, a.depth, a.budget),
        GraphCmd::God(a) => cmd_god(a.store, a.top),
        GraphCmd::Path(a) => cmd_path(a.src, a.dst, a.store, a.max_hops),
    }
}

fn dispatch_project(cmd: ProjectCmd) -> Result<()> {
    match cmd {
        ProjectCmd::List(a) => cmd_projects(a.store),
    }
}

fn dispatch_server(cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::Serve(a) => cmd_serve(a.store),
    }
}

fn cmd_build(
    path: PathBuf,
    store_dir: PathBuf,
    force: bool,
    cluster_cfg: ClusterConfig,
) -> Result<()> {
    let source = FsSource::new(&path);
    let extractor = DispatchExtractor::with_defaults();
    let store = FsStore::new(&store_dir).with_force(force);
    let pipeline = Pipeline::new(&source, &extractor, &store).with_cluster_config(cluster_cfg);

    let report = pipeline.build().context("build failed")?;
    // The apply path no longer snapshots (ADR-0042 F2); classic's `build` keeps
    // producing the `graph.json` interchange file by exporting explicitly, so the
    // perf-ledger jq recipes and oracle diffs keep working unchanged.
    store.snapshot().context("export graph.json")?;
    // Since ADR-0042 Phase 1d P1 the delta is a patch, so `BuildReport` reports
    // the edge *change*, not the graph's edge total. The total is still worth
    // printing on a build, so read it from the state that was just persisted.
    let edges = store
        .load_state()
        .ok()
        .flatten()
        .map_or(0, |s| s.graph.edges.len());
    println!(
        "built: {} files changed → +{} nodes, -{} removed, {edges} edges (+{}/-{} this build, affected {})",
        report.changed,
        report.nodes_added,
        report.nodes_removed,
        report.edges_added,
        report.edges_removed,
        report.affected
    );
    println!("store → {}/ (state.json + graph.json)", store_dir.display());
    Ok(())
}

fn cmd_import(file: PathBuf, store_dir: PathBuf) -> Result<()> {
    let bytes = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
    let state = graphjson::import(&bytes).context("parse filigrio graph.json")?;
    let store = FsStore::new(&store_dir);
    // Import as one bulk delta (whole-graph), then snapshot for compat.
    let delta = GraphDelta {
        nodes_added: state.graph.nodes,
        edges_added: state.graph.edges,
        partition: state.partition,
        ..Default::default()
    };
    store
        .apply_delta(&delta)
        .context("persist imported graph")?;
    store.snapshot()?;
    let loaded = store.load_state()?.unwrap_or_default();
    println!(
        "imported {} → {} nodes, {} edges into {}/",
        file.display(),
        loaded.graph.nodes.len(),
        loaded.graph.edges.len(),
        store_dir.display()
    );
    Ok(())
}

fn cmd_query(text: String, store_path: PathBuf, depth: usize, budget: usize) -> Result<()> {
    let view = load_view(&store_path)?;
    let opts = QueryOpts {
        depth,
        budget,
        ..Default::default()
    };
    let sub = view.query(&text, opts)?;
    println!(
        "query {text:?} → {} nodes, {} edges",
        sub.nodes.len(),
        sub.edges.len()
    );
    for n in &sub.nodes {
        let comm = sub
            .communities
            .get(&n.id)
            .map(|c| format!(" «{c}»"))
            .unwrap_or_default();
        println!("  - {} [{}]{} {}", n.label, n.kind, comm, n.loc());
    }
    Ok(())
}

fn cmd_god(store_path: PathBuf, top: usize) -> Result<()> {
    let view = load_view(&store_path)?;
    for (n, deg) in view.god_nodes(top)? {
        println!("{deg:>4}  {} [{}]", n.label, n.kind);
    }
    Ok(())
}

fn cmd_projects(store_path: PathBuf) -> Result<()> {
    let view = load_view(&store_path)?;
    let pg = view.project_graph()?;
    if pg.projects.is_empty() {
        println!("No projects — the tree has no package/module manifests.");
        return Ok(());
    }
    let label = |root: &str| -> String {
        pg.projects
            .iter()
            .find(|p| p.root == root)
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| root.to_string())
    };
    for p in &pg.projects {
        let name = p.name.clone().unwrap_or_else(|| p.root.clone());
        let deps: Vec<String> = p.depends_on.iter().map(|r| label(r)).collect();
        let arrow = if deps.is_empty() {
            String::new()
        } else {
            format!(" → {}", deps.join(", "))
        };
        println!("{name}  [{} files]{arrow}", p.files);
    }
    Ok(())
}

fn cmd_path(src: String, dst: String, store_path: PathBuf, max_hops: usize) -> Result<()> {
    let view = load_view(&store_path)?;
    // The port addresses endpoints by id (ADR-0027); the CLI takes human labels, so
    // resolve each label (or literal id) to a node id first. First match on a homonym
    // label — the CLI is a human tool; the MCP surface disambiguates instead.
    let resolve = |s: &str| -> Result<Option<String>> {
        if view.node_by_id(s)?.is_some() {
            return Ok(Some(s.to_string()));
        }
        Ok(view.nodes_by_label(s)?.first().map(|n| n.id.0.clone()))
    };
    let (Some(from_id), Some(to_id)) = (resolve(&src)?, resolve(&dst)?) else {
        println!("no path {src:?} → {dst:?}: unknown endpoint");
        return Ok(());
    };
    match view.shortest_path(&from_id, &to_id, max_hops)? {
        Some(path) => {
            let hops = path.len().saturating_sub(1);
            let chain = path
                .iter()
                .map(|n| n.label.as_str())
                .collect::<Vec<_>>()
                .join(" → ");
            println!("{hops} hops: {chain}");
        }
        None => println!("no path {src:?} → {dst:?} within {max_hops} hops"),
    }
    Ok(())
}

fn cmd_serve(store_path: PathBuf) -> Result<()> {
    let view = load_view(&store_path)?;
    // Per-call hot-reload: a tool arg `project_path` names another store dir to
    // load fresh for that call, so one server can serve several built graphs.
    let server = McpServer::with_reloader(
        view,
        Box::new(|p| load_view(&PathBuf::from(p)).map_err(|e| e.to_string())),
    );
    eprintln!("filigrio-mcp: JSON-RPC over stdio (one object per line). Ctrl-D to exit.");
    server.serve(BufReader::new(io::stdin()), io::stdout())?;
    Ok(())
}

// ---- helpers -----------------------------------------------------------

fn load_view(store_dir: &PathBuf) -> Result<GraphView> {
    let store = FsStore::new(store_dir);
    let state = store
        .load_state()?
        .with_context(|| format!("no store at {} — run `build` first", store_dir.display()))?;
    Ok(GraphView::new(Arc::new(state)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn graph_build_parses() {
        let cli = Cli::try_parse_from(["filigrio", "graph", "build", "some/path"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Graph {
                cmd: GraphCmd::Build(_)
            }
        ));
    }

    #[test]
    fn build_carries_cluster_flag() {
        let cli =
            Cli::try_parse_from(["filigrio", "graph", "build", "p", "--cluster", "full"]).unwrap();
        let a = match cli.command {
            Command::Graph {
                cmd: GraphCmd::Build(a),
            } => a,
            _ => panic!(),
        };
        assert!(matches!(a.cluster, ClusterArg::Full));
    }

    #[test]
    fn build_keeps_all_flags() {
        let cli = Cli::try_parse_from([
            "filigrio",
            "graph",
            "build",
            "p",
            "--store",
            "s",
            "--force",
            "--cluster",
            "full",
            "--cluster-weight",
            "confidence",
            "--resolution",
            "1.5",
        ])
        .unwrap();
        let a = match cli.command {
            Command::Graph {
                cmd: GraphCmd::Build(a),
            } => a,
            _ => panic!(),
        };
        assert_eq!(a.store, PathBuf::from("s"));
        assert!(a.force);
        assert!(matches!(a.cluster, ClusterArg::Full));
        assert!(matches!(a.cluster_weight, WeightArg::Confidence));
        assert_eq!(a.resolution, 1.5);
    }

    #[test]
    fn project_list_parses() {
        let cli = Cli::try_parse_from(["filigrio", "project", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Project {
                cmd: ProjectCmd::List(_)
            }
        ));
    }

    #[test]
    fn graph_god_carries_top_flag() {
        let cli = Cli::try_parse_from(["filigrio", "graph", "god", "--top", "5"]).unwrap();
        let a = match cli.command {
            Command::Graph {
                cmd: GraphCmd::God(a),
            } => a,
            _ => panic!(),
        };
        assert_eq!(a.top, 5);
    }

    #[test]
    fn graph_path_carries_flags() {
        let cli = Cli::try_parse_from(["filigrio", "graph", "path", "A", "B", "--max-hops", "12"])
            .unwrap();
        let a = match cli.command {
            Command::Graph {
                cmd: GraphCmd::Path(a),
            } => a,
            _ => panic!(),
        };
        assert_eq!(a.src, "A");
        assert_eq!(a.dst, "B");
        assert_eq!(a.max_hops, 12);
    }

    #[test]
    fn graph_import_parses() {
        let cli = Cli::try_parse_from(["filigrio", "graph", "import", "g.json"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Graph {
                cmd: GraphCmd::Import(_)
            }
        ));
    }
}
