//! filigrio-client-cli — the engine-free human CLI, a daemon client (ADR-0032f §1).
//! Ships the `filigrio` binary; the crate was `filigrio-cli` before 2026-07-28.
//!
//! This CLI is a thin proxy that speaks the §3 contract to the daemon.
//! It holds no engine, graph state, or indices at runtime or link time.
//!
//! Commands daemon: start/stop/status, project register
//! Commands graph: god/query/path proxy to daemon data plane
//! Flag --no-daemon: spawns one-shot responder (ADR-0032f §4)

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use filigrio_client_core::resolve_project_path;
use filigrio_install::{ClientId, Scope};
use filigrio_protocol::{default_socket_path, DaemonClient, DaemonClientTrait};
use std::path::{Path, PathBuf};
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser)]
#[command(
    name = "filigrio",
    version,
    about = "filigrio — engine-free CLI proxy to daemon (ADR-0032f)"
)]
struct Cli {
    #[command(subcommand)]
    command: AppCommand,

    /// Explicitly use daemon mode (default behavior per ADR-0032f §5)
    #[arg(long, global = true, action = clap::ArgAction::SetTrue, overrides_with = "no_daemon")]
    daemon: bool,

    /// Use one-shot mode (spawn ephemeral daemon per ADR-0032f §4)
    #[arg(long, global = true, action = clap::ArgAction::SetTrue)]
    no_daemon: bool,

    /// Socket path for daemon communication.
    #[arg(long, global = true)]
    #[arg(default_value_os_t = default_socket_path())]
    socket: PathBuf,

    // No `--wait` flag: commands execute synchronously daemon-side and the
    // response is the outcome (ADR-0042 F6c) — waiting is the only behavior.
    /// Verbose logging.
    #[arg(long, global = true)]
    verbose: bool,
}

impl Cli {
    fn is_daemon_mode(&self) -> bool {
        if self.no_daemon {
            return false;
        }
        true
    }
}

#[derive(Subcommand)]
enum AppCommand {
    /// Pin or inspect the resident daemon — the process that actually holds the
    /// graph (ADR-0032f §6, ADR-0032d §1).
    ///
    /// This CLI links no engine of its own; it is a *client* (ADR-0032f §1) and
    /// the daemon is the only engine host. The graph, the watchers and every
    /// piece of resident state live over there, which is why the other two
    /// resources read as things asked of something else rather than as work
    /// this process does.
    ///
    /// **`daemon start` is not a prerequisite of anything.** First use in a
    /// repository starts or attaches to the daemon by itself, ssh-agent style
    /// (ADR-0032 §1), so this group exists to pin the lifetime *deliberately* —
    /// a chosen `--idle-timeout` on a machine that should keep the graph warm,
    /// a `stop` before swapping binaries, a `status` when an answer looks
    /// stale. The opposite choice is `--no-daemon` on any command, which spawns
    /// a one-shot responder that answers the single request and exits, leaving
    /// nothing behind (ADR-0032f §4).
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Register, index and follow a repository — the registry and lifecycle of
    /// an indexed project (ADR-0033, ADR-0042).
    ///
    /// **Indexing is a property of a project, not of "the graph"**, and that is
    /// the shape worth saying out loud because it is the one people expect to
    /// find under `graph`. `project index` is the incremental update *and* the
    /// initial build — ADR-0042 F5 collapsed the old `build` verb into it, so
    /// there is one verb for "make this project's index match its files" rather
    /// than two that differ only in what was there before. `watch`, `export`
    /// and `flush` sit here for the same reason: each is something done **to**
    /// one registered repository, not a question asked of a graph.
    ///
    /// Registering is also what makes a project addressable at all. The daemon
    /// federates every project in its registry (ADR-0033 §2), which is what lets
    /// a `graph` verb name one with `--project` instead of requiring a `cd` into
    /// it.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Ask questions about one project's graph: statistics, hubs, neighbors,
    /// paths, communities, the report.
    ///
    /// **Every verb here is a question, and none of them mutates anything.**
    /// Indexing, watching and exporting are `project` verbs precisely because
    /// they are done to a repository; this resource only reads. The answers come
    /// from the daemon's resident state, which is the fresher of the two copies:
    /// under ADR-0042 F4 write-behind a live watch persists on a quiescence
    /// window, so `.filigrio-out` may lag the graph these verbs are answering
    /// from.
    ///
    /// Every verb takes `--project`, and without it asks about the current
    /// directory. Since the daemon federates the whole registry, a second
    /// repository is a flag rather than a `cd`.
    Graph {
        #[command(subcommand)]
        cmd: GraphCmd,
    },
    /// Wire coding agents into this repository: each agent's MCP registration
    /// plus the capability doc it reads (ADR-0034 §17).
    ///
    /// **`agent`, not `client`**: three crates in this workspace already use
    /// "client" for the other end of ADR-0032f's topology (`filigrio-client-cli`
    /// is itself a *daemon* client), and `agent` is the ecosystem's own word —
    /// `AGENTS.md`, agents.md.
    ///
    /// This command touches **no git hooks and no completions**. That is the
    /// whole point of §17: the three families answer three different questions,
    /// and no command spans them, so oversupply is unrepresentable rather than a
    /// default to get right.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Manage this repository's `AGENTS.md` — the agent documentation the
    /// repository itself carries (ADR-0034 §18.2).
    ///
    /// **A resource and not an agent.** It shipped as `--agent agents-md`, which
    /// was a category error with its own tell: an "agent" that registered
    /// nothing and whose detection had to explain that there was nothing to
    /// detect. `AGENTS.md` is user-owned, edited through a managed block
    /// precisely because it is not ours, and read by agents this build supports
    /// *and by agents it does not*. Adding it is a decision about the repository
    /// — "this project documents its tooling in `AGENTS.md`" — in the same sense
    /// a README is, and not a consequence of which editor one contributor
    /// happens to run.
    ///
    /// **No `--global`**, because it is project-scoped by definition: `agents.md`
    /// puts the file at the repository root, and a `~/AGENTS.md` is a different
    /// file that no agent reads *for this repository*. With nothing outside the
    /// checkout to consent to, there is no gate to build.
    ///
    /// **No member selector**, and `docs install` just installs — the reasoning
    /// §17.2 records for `hooks` and `completions`: few, cohesive, inside the
    /// repository the user named. There is exactly one artifact today.
    Docs {
        #[command(subcommand)]
        cmd: DocsCmd,
    },
    /// Manage the ADR-0032b git hooks that keep the graph fresh at commit time
    /// (ADR-0034 §17) — and, under `run`, be the thing they call.
    ///
    /// One resource, both halves: `install`/`uninstall`/`status` are ADR-0038's
    /// surface over the four scripts, and `run` is the verb those scripts
    /// contain. The managing and the running are about the same four hooks, so
    /// they belong under the same word — see `hooks run` for why the
    /// alternative, a top-level sibling, was a trap that `hide` could not close.
    // Sibling verbs are named as shell words rather than as rustdoc links
    // because clap renders this doc comment verbatim into `hooks --help`, and
    // its first line into all three completion scripts, where a
    // `[HooksCmd::Run]` is noise the reader cannot follow.
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },
    /// Manage this CLI's bash/zsh/fish completions (ADR-0034 §6, §17).
    ///
    /// No `--global`: shell completions live under `$HOME` by nature and nowhere
    /// else, so naming the command *is* the consent (§17.1).
    Completions {
        #[command(subcommand)]
        cmd: CompletionsCmd,
    },
}

/// The flags every `filigrio agent` verb takes.
#[derive(clap::Args, Debug, Clone)]
struct AgentArgs {
    /// Agents to act on: `--agent cursor,opencode`, repeatable.
    ///
    /// **There is no `all`** (ADR-0034 §17.2). Installing every agent from one
    /// command is the oversupply §17 exists to prevent, one flag deeper, so an
    /// agent that is to be installed is an agent that was named — and `install`
    /// with no `--agent` prints the roster and exits non-zero rather than
    /// guessing.
    ///
    /// `uninstall` and `status` read an empty selection as *every* agent, and
    /// that asymmetry is deliberate: see [`run_agent`].
    #[arg(long = "agent", value_delimiter = ',')]
    agents: Vec<String>,
    /// Consent to writes outside this repository (ADR-0034 §17.1).
    ///
    /// Not a scope toggle with a default — a **gate**. Without it, this command
    /// cannot write outside the repository, with no exceptions to remember; an
    /// agent whose config only exists under `$HOME` is refused by name and says
    /// which flag would have included it. It gates *reach* on removal too: a
    /// bare `uninstall` sweeps the repository and reports what it left under
    /// `$HOME` rather than silently leaving it.
    #[arg(long)]
    global: bool,
    /// Repository to wire (default: the current directory).
    ///
    /// **`--repo`, not `--project`**, and the two are different questions. A
    /// `graph` verb's `--project` names something the *daemon* already has —
    /// its registry id, or a path inside its root — and answers "which graph
    /// am I asking about". This one names a **checkout on this disk**, because
    /// files are written into it, and it is meaningful for a repository the
    /// daemon has never heard of: wiring an agent is a decision about a working
    /// tree, not about an index. One flag word spanning both would have made
    /// `--project some-registered-name` a plausible thing to type here, and it
    /// would have failed at `canonicalize` rather than at the point of the
    /// misunderstanding.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Print every note in full. Default: one line per kind naming the agents
    /// it covers, plus a count of what that folded away.
    #[arg(long)]
    explain: bool,
}

#[derive(Subcommand, Debug)]
enum AgentCmd {
    /// Wire the named agents (idempotent: a second run changes nothing).
    Install {
        #[command(flatten)]
        args: AgentArgs,
    },
    /// Remove exactly what install added, leaving user content untouched.
    Uninstall {
        #[command(flatten)]
        args: AgentArgs,
    },
    /// Report what is installed, current, stale, or absent.
    Status {
        #[command(flatten)]
        args: AgentArgs,
    },
}

/// The flags every `filigrio docs` verb takes.
///
/// Two, and the two that are missing are the decision. No `--global`: the
/// artifact's home is the repository root by the convention's own definition
/// (ADR-0034 §18.2), so there is no second scope to select and nothing outside
/// the checkout to consent to. No member selector: one artifact.
///
/// The shape is still able to grow — a second document would arrive as a
/// `--doc` value list resolved exactly as [`HooksArgs::hooks`] is — but adding
/// the flag before the second artifact would be a selector whose absence means
/// "the only one there is", which is the shape ADR-0038 §2b names as the thing
/// to be suspicious of.
#[derive(clap::Args, Debug, Clone)]
struct DocsArgs {
    /// Repository to document (default: the current directory).
    ///
    /// A checkout on this disk, not a daemon-registered project — see the same
    /// flag on `filigrio agent` for why the word differs from `graph`'s.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Print every note in full.
    #[arg(long)]
    explain: bool,
}

#[derive(Subcommand, Debug)]
enum DocsCmd {
    /// Write (or refresh) the `filigrio` block in `AGENTS.md`.
    Install {
        #[command(flatten)]
        args: DocsArgs,
    },
    /// Remove exactly the managed block install added, leaving the user's own
    /// text untouched.
    Uninstall {
        #[command(flatten)]
        args: DocsArgs,
    },
    /// Report whether the block is installed, current, stale, or absent.
    Status {
        #[command(flatten)]
        args: DocsArgs,
    },
}

/// The flags every `filigrio hooks` verb takes.
///
/// **There is no member gate here, and that is a decision** the ADR now records
/// (ADR-0034 §17.2, last paragraph). §17.2 requires `agent install` to be told
/// its agents, and the argument it gives is about *scope and breadth*: eight
/// agents nobody uses all of, five of them writing outside the repository, three
/// into credential stores for products the user may not have installed.
///
/// None of that is true here. `hooks install` writes four files into one
/// directory of one repository the user named, all four the same artifact, all
/// four inside the checkout. The four are not independently meaningful either:
/// wanting the graph refreshed after a commit but not after a merge or a rebase
/// is not a preference anyone has, it is a graph that is silently stale after
/// two of the four ways a tree changes. So `--hook` exists for the surgical case
/// (repairing one file, excluding one hook a repository's own tooling owns) and
/// omitting it means all four. Requiring
/// `--hook post-commit,post-checkout,post-merge,post-rewrite` would be friction
/// with no safety behind it, and friction with no safety is how a safety
/// mechanism gets ignored where it does matter.
#[derive(clap::Args, Debug, Clone)]
struct HooksArgs {
    /// Hooks to act on: `--hook post-commit,post-merge`, repeatable. Default:
    /// all four.
    #[arg(long = "hook", value_delimiter = ',')]
    hooks: Vec<String>,
    /// Repository whose hooks to manage (default: the current directory).
    ///
    /// A checkout on this disk — git's hooks live in *its* `.git`, so the
    /// question this answers is which working tree, never which index. See the
    /// same flag on `filigrio agent`.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Print every note in full.
    #[arg(long)]
    explain: bool,
}

#[derive(Subcommand, Debug)]
enum HooksCmd {
    /// Install the git hooks (chained into an existing hook, never clobbering).
    Install {
        #[command(flatten)]
        args: HooksArgs,
    },
    /// Remove exactly the managed block install added.
    Uninstall {
        #[command(flatten)]
        args: HooksArgs,
    },
    /// Report what is installed, current, stale, or absent — and, for `status`
    /// only, which project id the hooks submit under (ADR-0032b OQ4).
    Status {
        #[command(flatten)]
        args: HooksArgs,
    },
    /// The git-hook entry point (ADR-0032b): turn a commit-boundary transition
    /// into one high-priority `Submit`.
    ///
    /// This is the line the scripts `install` writes actually contain —
    /// `filigrio hooks run <event> "$@"` — because ADR-0034 §11 makes the
    /// installed hook a *shim* and not a producer. git's own hook arguments are
    /// passed through positionally and unmodified, and `post-rewrite`'s rewrite
    /// pairs are read from stdin exactly as git supplies them.
    ///
    /// **Nested under `hooks` rather than beside it, because that is the only
    /// place the trap can be removed instead of hidden.** As a top-level
    /// `filigrio hook` it sat one letter from `filigrio hooks` in `--help`, and
    /// the interesting-looking one of the pair is the one no human should ever
    /// type. `#[command(hide = true)]` answered the help text and nothing else:
    /// measured against `clap_complete` 4.6.8, its AOT generators consult
    /// `is_hide_set()` only for *possible values* and enumerate subcommands with
    /// a bare `get_subcommands()` in every shell backend, so the hidden verb was
    /// still tab-completable in bash, zsh and fish. Nor could it be filtered
    /// out: `clap::Command` has no subcommand-removal API at all — only
    /// `mut_subcommand`, `mut_subcommands`, `get_subcommands_mut`, and a
    /// `subcommands` that adds — so suppressing it meant rebuilding the whole
    /// `Command` by hand or doing text surgery on three 70–90 KB generated
    /// scripts. That measurement is recorded here, where the decision lives, so
    /// nobody re-litigates it by reaching for `hide` again. Under `hooks`,
    /// completing `hooks run` is *correct*, so there is nothing left to filter.
    ///
    /// What git fixes is the hook *file*, its argv order, and `post-rewrite`'s
    /// stdin — all of which this leaves alone. The name of the verb our own
    /// generated script calls is ours: git never sees that string, and the only
    /// caller in the world is a script we write.
    ///
    /// **None of the management flags**, deliberately: `--hook`, `--project` and
    /// `--explain` select which scripts to manage and how to report on them. Git
    /// has already chosen the event and the repository by the time this runs,
    /// and a hook prints diagnostics rather than a note block, so flattening
    /// them here would offer three flags that could only be accepted and
    /// ignored.
    ///
    /// Exits **0** whatever happens: a hook that can fail a `git commit` is
    /// worse than no hook (§4). Set `FILIGRIO_SKIP_HOOK=1` to opt out.
    /// Diagnostics go to stderr; nothing is ever written to stdout.
    Run {
        /// The git hook that fired: post-commit, post-checkout, post-merge or
        /// post-rewrite.
        event: String,
        /// git's own hook arguments, verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// The flags every `filigrio completions` verb takes.
///
/// **No member gate**, for the same reason as [`HooksArgs`] and one more of its
/// own: three files, one per shell, and a user does not know today which shell
/// they will be in tomorrow. The scope question §17.2 exists to force does not
/// arise — completions have exactly one home (§17.1), so naming the command is
/// the consent, which is also why there is no `--global`.
#[derive(clap::Args, Debug, Clone)]
struct CompletionsArgs {
    /// Shells to act on: `--shell zsh,fish`, repeatable. Default: all three.
    #[arg(long = "shell", value_delimiter = ',')]
    shells: Vec<String>,
    /// Print every note in full.
    #[arg(long)]
    explain: bool,
}

#[derive(Subcommand, Debug)]
enum CompletionsCmd {
    /// Generate and install the completion files.
    Install {
        #[command(flatten)]
        args: CompletionsArgs,
    },
    /// Remove the completion files this CLI wrote (a stranger's file is
    /// refused, never deleted).
    Uninstall {
        #[command(flatten)]
        args: CompletionsArgs,
    },
    /// Report what is installed, current, stale, absent, or not ours.
    Status {
        #[command(flatten)]
        args: CompletionsArgs,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start the freshness daemon.
    Start {
        /// Idle shutdown timeout in seconds (0 = never).
        #[arg(long, default_value_t = 300)]
        idle_timeout: u64,
    },
    /// Stop the running daemon.
    Stop,
    /// Show daemon status.
    Status,
}

/// The polarity of `project watch` (ADR-0042 F6b).
///
/// The two literal words are the argument, so they are a type. A
/// `value_parser = ["on", "off"]` over a `String` checks the same two spellings
/// at the door but still hands the verb a `String`, which leaves `state == "on"`
/// as the real decision — a comparison where a third spelling, or a typo in
/// *this* file, reads as "off" and silently stops a watch the user asked to
/// start.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum WatchState {
    /// Re-check the index (a synchronous deep reconcile), then follow live.
    On,
    /// Stop following. The index stays exactly where it is.
    Off,
}

impl std::fmt::Display for WatchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_value_enum(self, f)
    }
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Register current directory as a project.
    Register {
        /// Force registration even if not a recognized project.
        #[arg(long)]
        force: bool,
    },
    /// Remove project from registry.
    Remove {
        /// Project name or path.
        project: String,
    },
    /// List registered projects.
    List,
    /// Show project status.
    Status {
        /// Project name (default: current directory).
        project: Option<String>,
    },
    /// Index a project (incremental update; on an empty store this IS the
    /// initial build — ADR-0042 F5 collapsed the old `build` verb into this).
    /// Runs to completion: the daemon executes synchronously and the exit
    /// status reflects the outcome (ADR-0042 F6c).
    Index {
        /// Project name or path.
        project: Option<String>,
        /// Reindex from scratch (wipe and rebuild). Reserved: not implemented
        /// yet — fails fast rather than being silently ignored.
        ///
        /// `--clean`, not `--force` (ADR-0042 F7): `--force` on
        /// `project register` means "override the safety check", and one flag
        /// word must not carry two semantics.
        #[arg(long)]
        clean: bool,
    },
    /// Watch a project: `watch on` re-checks the index (a synchronous deep
    /// reconcile) and then follows file changes live; `watch off` stops
    /// following (ADR-0042 F6b). Requires the resident daemon.
    Watch {
        /// Start following, or stop.
        #[arg(value_enum)]
        state: WatchState,
        /// Project name or path.
        project: Option<String>,
    },
    /// Export the project's graph.json interchange snapshot (ADR-0042 F2:
    /// the apply path no longer writes it — this verb is its only producer).
    Export {
        /// Project name or path.
        project: Option<String>,
    },
    /// Persist the daemon's resident state for a project to its store now
    /// (ADR-0042 F4). Watch-mode applies are write-behind — the daemon serves
    /// the newest graph but `.filigrio-out` may lag by up to the quiescence
    /// window — so this is how an out-of-process reader gets a current file
    /// without waiting. Idempotent: a project with nothing outstanding is a
    /// no-op, not an error.
    Flush {
        /// Project name or path.
        project: Option<String>,
    },
}

/// The flag every `filigrio graph` verb takes.
///
/// **A name-or-path, and deliberately not a `PathBuf`.** This `--project` is not
/// the one `agent`/`docs`/`hooks` carry: those name a *repository to wire*, a
/// directory that must exist on this machine because files are written into it.
/// This one names a project the **daemon** already has, and the registry resolves
/// either its id or an absolute path inside its root — the same value the MCP
/// bridge passes for its own `project` argument. Typing it as a path would rule
/// out the id, which is the spelling a human reading `project list` actually has.
///
/// Flattened into every verb rather than sitting on the `graph` group, so it
/// reads after the thing being asked for (`graph query "auth" --project myproj`)
/// — the same shape [`AgentArgs`] and [`HooksArgs`] use.
#[derive(clap::Args, Debug, Clone)]
struct GraphArgs {
    /// Project to ask about: its daemon-registry id, or an absolute path inside
    /// its root (default: the current directory).
    ///
    /// The daemon federates every registered project, but until this flag existed
    /// the CLI could only ever ask about the directory the shell happened to be
    /// in, so querying a second repository meant a `cd` — a limit of the client,
    /// never of the daemon.
    #[arg(long)]
    project: Option<String>,
}

/// The `--direction` of `graph neighbors`.
///
/// A local enum and not [`filigrio_protocol::Direction`] itself, because the
/// protocol crate must not grow a `clap` dependency to spell one CLI flag: it is
/// linked by the daemon and the MCP bridge, neither of which parses an argv.
///
/// Typed rather than a `String` matched at use, which is why no arm here rejects
/// an invalid direction: the runtime `_ =>` that shape needs reports a typo only
/// *after* the daemon has been connected to — and, under `--no-daemon`, after one
/// has been spawned — where clap refuses it before anything runs. The value set
/// also reaches `--help` and the completions this CLI installs for itself, which
/// a `String` can carry neither of.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum EdgeDirection {
    /// Only edges pointing **at** the node — "who depends on this?".
    Incoming,
    /// Only edges leaving the node — "what does this depend on?".
    Outgoing,
    /// Both, and the default: the first question a human asks of a node is who
    /// touches it, which is not a question about one polarity.
    Both,
}

/// The CLI's words are not the wire's — `incoming`/`outgoing` against the
/// contract's `in`/`out` — and neither may be renamed to spare this impl: the
/// wire spelling is frozen (ADR-0029, asserted in `query_types`) and the CLI
/// spelling is what users already type. One `From` is the whole cost of keeping
/// both, and it is the only place the two meet.
impl From<EdgeDirection> for filigrio_protocol::Direction {
    fn from(direction: EdgeDirection) -> Self {
        match direction {
            EdgeDirection::Incoming => Self::In,
            EdgeDirection::Outgoing => Self::Out,
            EdgeDirection::Both => Self::Both,
        }
    }
}

impl std::fmt::Display for EdgeDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_value_enum(self, f)
    }
}

#[derive(Subcommand, Debug)]
enum GraphCmd {
    /// Query the graph.
    Query {
        /// Query text.
        query: String,
        /// Maximum depth.
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Result budget.
        #[arg(long, default_value_t = 32)]
        budget: usize,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Show god nodes (high-centrality).
    God {
        /// Number of nodes to show.
        #[arg(long, default_value_t = 10)]
        top: usize,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Find shortest path.
    Path {
        /// Source node.
        from: String,
        /// Destination node.
        to: String,
        /// Maximum hops.
        #[arg(long, default_value_t = 8)]
        max_hops: usize,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Show graph statistics.
    Stats {
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Get specific node.
    GetNode {
        /// Node address (id or "label:src").
        address: String,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Show neighbors.
    Neighbors {
        /// Node address.
        address: String,
        /// Which edges make a node a neighbor.
        #[arg(long, value_enum, default_value_t = EdgeDirection::Both)]
        direction: EdgeDirection,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Get project graph (monorepo architecture).
    ProjectGraph {
        #[command(flatten)]
        args: GraphArgs,
    },
    /// List the graph's communities — the clusters ADR-0024 detects — largest
    /// first, each with its id, label, size and cohesion.
    ///
    /// **This is the discovery half of a pair, and the only place a community
    /// id comes from.** The `community=` attribute stamped on a node carries the
    /// derived *label* (its highest-degree member's name) and not the id, so
    /// without this listing `graph community <id>` has nothing to be given —
    /// list here, then ask about one. Rosters are deliberately absent: a
    /// membership dump of every community is not an answer anyone reads, which
    /// is what the singular verb is for.
    ///
    /// The same reads the MCP bridge has served as `list_communities` all along.
    /// Until this verb existed the two transports over one daemon exposed
    /// different surfaces, and a human could reach communities only by writing a
    /// whole `graph report`.
    Communities {
        /// How many communities to list, largest first.
        ///
        /// `--top`, matching `graph god --top` and `graph report --top`: one
        /// word for "rank and cut" whatever is being ranked. Bounded by default
        /// because clustering routinely yields hundreds of communities on a real
        /// repository, and the interesting ones are the large ones.
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Show one community's members, by an id `graph communities` printed.
    ///
    /// The follow-up half of the pair: `communities` ranks the clusters and is
    /// where the id comes from, this expands exactly one of them. An id is not
    /// guessable and no other read emits one, so a bare `graph community 7` is
    /// only ever typed after a listing.
    Community {
        /// The community id, as printed by `graph communities`.
        id: u64,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// Write the human-facing `GRAPH_REPORT.md` (stats, god nodes, every
    /// community's roster and cohesion, cross-community bridges, projects).
    ///
    /// The markdown is rendered **daemon-side** (`DataQuery::GraphReport`) and
    /// this verb is the pipe to a file — the CLI links no engine (ADR-0032f §1),
    /// and the daemon's resident state is the fresh one (a store read would be
    /// stale by design under F4 write-behind).
    Report {
        /// Where to write the report (`-` for stdout).
        #[arg(long, default_value = "GRAPH_REPORT.md")]
        out: PathBuf,
        /// How many god nodes to rank.
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[command(flatten)]
        args: GraphArgs,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // An `EnvFilter` directive matches on the *event target*, which is the
    // emitting crate's `module_path!()`. For a binary that is the **target**
    // name from `[[bin]]` — `filigrio` — not the package name. This read
    // `filigrio_cli=…` until 2026-07-28, which matched nothing at all: no
    // `info!` in this file had ever been printed and `--verbose` was silently a
    // no-op. `CARGO_CRATE_NAME` is the target's own name, so the directive
    // cannot drift out of step with a future rename the way the literal did.
    //
    // Target matching is a **prefix** match, so this also enables the client's
    // own libraries (`filigrio_client_core`, `filigrio_protocol`) — which is
    // the intent: they are where auto-start and the socket send actually
    // happen, and "the CLI was verbose but said nothing" is the bug being
    // fixed.
    let directive = if cli.verbose {
        concat!(env!("CARGO_CRATE_NAME"), "=debug")
    } else {
        concat!(env!("CARGO_CRATE_NAME"), "=info")
    };
    let filter = EnvFilter::from_env("FILIGRIO_LOG").add_directive(
        directive
            .parse()
            .expect("compile-time-constant filter directive"),
    );

    // Logs go to **stderr**. This CLI's stdout is its data output (query
    // results are pretty-printed JSON); with the filter finally matching, an
    // `info!` on stdout would corrupt every `filigrio graph … | jq` pipeline.
    // Same split the MCP bridge already makes, for the same reason.
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let use_daemon = cli.is_daemon_mode();

    match cli.command {
        AppCommand::Daemon { cmd } => run_daemon(cmd, cli.socket).await,
        AppCommand::Project { cmd } => run_project(cmd, cli.socket, use_daemon).await,
        AppCommand::Graph { cmd } => run_graph(cmd, cli.socket, use_daemon).await,
        AppCommand::Agent { cmd } => run_agent(cmd, cli.socket),
        AppCommand::Docs { cmd } => run_docs(cmd, cli.socket),
        AppCommand::Hooks { cmd } => run_hooks(cmd, cli.socket),
        AppCommand::Completions { cmd } => run_completions(cmd, cli.socket),
    }
}

/// The selectors, with repeats folded away, in the order the user first named
/// them.
///
/// `--hook post-commit --hook post-commit` is one request. Without this the loop
/// ran twice over the same file, and the run said `✅ 2 artifact(s) handled` over
/// one file — a count of *passes*, presented as a count of artifacts, to a user
/// who is reading it to find out what is on their disk. The same for `--agent`,
/// where every note is repeated too.
///
/// `Vec::contains` rather than a set: these lists are three, four and eight
/// elements long, order is what the report prints, and none of `ClientId`,
/// `Hook` or `Shell` has a `Hash`/`Ord` that exists for any other reason.
fn deduped<T: Clone + PartialEq>(items: &[T]) -> Vec<T> {
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

/// Write a value-enum variant using the spelling **clap** accepts for it.
///
/// The messages that echo a typed argument back at the user (`Neighbors of x
/// (both direction):`, `❌ Watch on failed`) read the word out of the parser's own
/// value set instead of re-typing the literal, so a renamed variant cannot leave
/// a message quoting a word the CLI no longer takes.
///
/// A `Display` body and not a `-> &'static str` helper because
/// `PossibleValue::get_name` borrows from the temporary the lookup returns; the
/// borrow is only sound while it is being written.
fn fmt_value_enum<T: clap::ValueEnum>(
    value: &T,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    f.write_str(
        value
            .to_possible_value()
            .expect("neither enum hides or skips a variant")
            .get_name(),
    )
}

/// The exit status a usage error gets (ADR-0034 §17.2).
///
/// **2, not 1**, and the distinction is the whole reason §17.2 asks for a
/// non-zero exit at all: 1 already means "the run happened and some artifact
/// failed", and a bare `install` is not that — nothing was attempted. A CI job
/// that runs the bare command must fail loudly rather than appear to succeed
/// while doing nothing, and it should be able to tell "you invoked me wrong"
/// from "a file could not be written". 2 is what clap itself exits with for a
/// usage error, so the two doors out of "you typed something we cannot act on"
/// agree.
const USAGE_ERROR: i32 = 2;

/// Resolve the environment every installer verb needs.
///
/// `repo` is canonicalised because `--repo` may be relative or behind a
/// symlink, and every path this run reports is derived from it.
fn install_environment(
    repo: Option<&Path>,
    socket: PathBuf,
) -> Result<filigrio_install::Environment> {
    let project_root = match repo {
        Some(p) => {
            std::fs::canonicalize(p).with_context(|| format!("resolving --repo {}", p.display()))?
        }
        None => resolve_project_path().context("resolving the current directory")?,
    };
    // The version the artifacts claim is *this binary's*, so it is read here and
    // passed in: `env!` inside the installer library would report that crate's
    // version instead, which is the same number today only by coincidence.
    Ok(filigrio_install::Environment::discover(
        project_root,
        socket,
        env!("CARGO_PKG_VERSION").to_string(),
    )?)
}

/// Print a finished report and map it onto the process exit status.
///
/// One function for all three resources, because the rendering decision is one
/// decision: the notes are the only part with a density to choose, the choice is
/// the user's (`--explain`), and a failure must reach `$?`. Three copies of that
/// would be three places for one of them to stop consulting `report.is_ok()`.
fn finish(report: &filigrio_install::Report, explain: bool) -> Result<()> {
    use filigrio_install::Notes;
    print!(
        "{}",
        report.render(if explain {
            Notes::Verbatim
        } else {
            Notes::Aggregated
        })
    );
    if report.is_ok() {
        println!("✅ {} artifact(s) handled, no failures", report.steps.len());
        Ok(())
    } else {
        // Honest failure: the run completed, the failures are named above, and
        // the exit status says so (ADR-0042 F6c's posture, applied here).
        eprintln!(
            "❌ {} of {} artifact(s) failed — see the `!` lines above",
            report.failures.len(),
            report.steps.len() + report.failures.len()
        );
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// `filigrio agent` (ADR-0034 §17)
// ---------------------------------------------------------------------------

/// How this run chose its agents, which is what decides whether a scope refusal
/// is a **failure** or a **report** (ADR-0034 §17.2).
///
/// Only two things can produce a selection, and they are not the same request.
/// An agent the user *named* and cannot have is a failure: they asked for it and
/// did not get it. An agent left out of a **sweep** is not, because a sweep asked
/// for whatever this scope holds and got exactly that — but it is still owed a
/// sentence saying what was left where, or a bare `uninstall` quietly leaves six
/// registrations under `$HOME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    /// A list the user spelled out. The only way `install` can select anything.
    Named,
    /// Every agent, from an empty `--agent` on a verb that removes or reports.
    /// There is no such thing for `install` — see [`run_agent`].
    Swept,
}

/// Resolve `--agent` values into adapters. `None` when none were named.
///
/// An unknown name is a **hard error**, not a silent no-op: "I asked for
/// `claud-code` and nothing happened" is the failure mode this project's
/// honest-failure rule exists to prevent. `all` is an unknown name like any
/// other now, and lands in the same message with the eight real ones beside it.
fn resolve_agents(names: &[String]) -> Result<Option<Vec<ClientId>>> {
    if names.is_empty() {
        return Ok(None);
    }
    let named = names
        .iter()
        .map(|name| {
            ClientId::parse(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown agent `{name}` — known: {}",
                    filigrio_install::ALL_CLIENTS
                        .iter()
                        .map(|c| c.slug())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // After the parse, not before: the dedupe is about the *agent*, and an
    // unknown spelling must still be a hard error rather than a duplicate
    // quietly folded away.
    Ok(Some(deduped(&named)))
}

/// How one agent's artifacts stand right now, for the roster.
///
/// Runs the adapter's own `status` into a throwaway report rather than
/// re-deriving the answer here — a second opinion about "is it installed?" is a
/// second thing to get wrong, and this one would be the one a user reads *before*
/// deciding what to run.
fn installed_verdict(
    env: &filigrio_install::Environment,
    id: ClientId,
    scope: Scope,
) -> &'static str {
    use filigrio_install::Action;
    let mut probe = filigrio_install::Report::default();
    filigrio_install::installer_for(id).status(env, scope, &mut probe);
    if !probe.failures.is_empty() {
        return "unreadable";
    }
    if probe.steps.is_empty() {
        return "—";
    }
    let present = probe
        .steps
        .iter()
        .filter(|s| s.action == Action::Present)
        .count();
    if present == 0 {
        "no"
    } else if present < probe.steps.len() {
        "partly"
    } else if probe
        .steps
        .iter()
        .any(|s| s.detail == filigrio_install::STALE)
    {
        "stale"
    } else {
        "yes"
    }
}

/// The roster a bare `install` prints instead of guessing (ADR-0034 §17.2).
///
/// Every agent, the scope *this invocation* would use, whether it was detected,
/// and whether it is already installed — then the invocation to use. The scope
/// column is resolved against the `--global` the user actually passed rather than
/// against the adapter's default, because a roster that showed `project` for
/// hermes under `--global` would be describing a command nobody ran.
///
/// `install` alone, because it is the only verb with nothing to fall back on:
/// `uninstall` and `status` read an empty selection as every agent
/// ([`run_agent`]).
fn print_agent_roster(env: &filigrio_install::Environment, global: bool) {
    let requested = if global {
        Scope::Global
    } else {
        Scope::Project
    };
    println!(
        "agents this build can wire — {}",
        env.project_root.display()
    );
    println!(
        "  {:<14}{:<10}{:<11}installed",
        "agent", "scope", "detected"
    );
    for id in filigrio_install::ALL_CLIENTS {
        let installer = filigrio_install::installer_for(*id);
        let (scope, installed) = match installer.scope_support().resolve(requested) {
            Ok(scope) => (scope.slug(), installed_verdict(env, *id, scope)),
            // Not "project"/"global" but the reason it is neither: this row is
            // the one a user has to act on, and `—` in the scope column is what
            // sends them to the guidance below rather than to a retry.
            Err(_) => ("excluded", "—"),
        };
        let detected = match installer.detect(env) {
            filigrio_install::Detection::Present(_) => "yes",
            filigrio_install::Detection::Absent(_) => "no",
            filigrio_install::Detection::Unknown(_) => "unknown",
        };
        println!(
            "  {:<14}{:<10}{:<11}{}",
            id.slug(),
            scope,
            detected,
            installed
        );
    }
    println!();
    println!(
        "Nothing was written: `agent install` acts on the agents you name, and naming none is not \
         naming all (ADR-0034 §17.2)."
    );
    println!("  filigrio agent install --agent claude-code");
    println!("  filigrio agent install --agent cursor,opencode");
    println!("  filigrio agent install --agent hermes --global   # …one that lives under $HOME");
    println!();
    println!(
        "  filigrio agent status      # every agent, reported, nothing written\n  \
         filigrio agent uninstall   # every agent's artifacts in this repository, removed"
    );
    // The roster is where a user finds out that five of these seven get a
    // registration and no manual, so it is where the other command belongs
    // (ADR-0034 §18.2). Naming it here is the difference between "legible" and
    // "documented somewhere": the alternative is a user reading a `+` beside
    // `cursor/mcp` and concluding Cursor is fully wired.
    println!(
        "\nFive of these seven read this repository's AGENTS.md rather than a doc of their own \
         (claude-code and opencode each write their own skill):\n  \
         filigrio docs install      # write the AGENTS.md block — a repository decision, not an \
         agent's"
    );
}

/// `filigrio agent install | uninstall | status` (ADR-0034 §17).
///
/// The CLI's whole job here is selection, the scope gate and rendering; every
/// artifact decision lives in `filigrio-install`, which links no engine
/// (ADR-0032f §1).
///
/// ## Only `install` has to be told its agents, and the asymmetry is the point
///
/// A bare `install` prints the roster and exits non-zero (§17.2). A bare
/// `uninstall` removes every agent artifact it finds, and a bare `status`
/// reports on every agent.
///
/// That is not an oversight in either direction — the oracle has the same shape
/// (`_project_uninstall_all` exists; there is no install-all), and the reason is
/// that the two sweeps are not the same kind of act. **A sweep that removes can
/// only remove what we wrote**: a marker-delimited block, a named key, never a
/// line of the user's own — so the worst case is that it removed something the
/// user still wanted, which they can put back with one `install`. **A sweep that
/// creates cannot be undone by the user not having asked**: it puts sixteen
/// artifacts in six directories, three of them credential stores for products
/// that may not be installed, and every one of them has to be found before it
/// can be removed. Generous on removal, conservative on creation.
///
/// `--global` still gates reach on all three: a bare `uninstall` removes what is
/// in the repository and *reports* what it left under `$HOME`, naming the flag —
/// see [`report_sweep_exclusions`]. Leaving them is a decision; leaving them
/// silently is the defect.
fn run_agent(cmd: AgentCmd, socket: PathBuf) -> Result<()> {
    let args = match &cmd {
        AgentCmd::Install { args } | AgentCmd::Uninstall { args } | AgentCmd::Status { args } => {
            args.clone()
        }
    };
    let env = install_environment(args.repo.as_deref(), socket)?;

    let Some(agents) = resolve_agents(&args.agents)? else {
        if matches!(cmd, AgentCmd::Install { .. }) {
            print_agent_roster(&env, args.global);
            std::process::exit(USAGE_ERROR);
        }
        return run_agent_verbs(
            &cmd,
            &env,
            Selection::Swept,
            filigrio_install::ALL_CLIENTS,
            &args,
        );
    };

    run_agent_verbs(&cmd, &env, Selection::Named, &agents, &args)
}

/// The scope gate and the verb loop — `match (verb)` per agent, never a provider
/// registry.
///
/// The gate runs **once**, here, over two facts each adapter declares
/// ([`ScopeSupport`]). That is the mechanism behind §17.1's sentence: *`filigrio
/// agent install` without `--global` cannot write outside the repository*. There
/// is no per-adapter branch that could grow an exception, because there is no
/// per-adapter branch.
fn run_agent_verbs(
    cmd: &AgentCmd,
    env: &filigrio_install::Environment,
    selection: Selection,
    agents: &[ClientId],
    args: &AgentArgs,
) -> Result<()> {
    use filigrio_install::Report;

    let requested = if args.global {
        Scope::Global
    } else {
        Scope::Project
    };
    let verb = match cmd {
        AgentCmd::Install { .. } => "install",
        AgentCmd::Uninstall { .. } => "uninstall",
        AgentCmd::Status { .. } => "status",
    };

    let mut report = Report::default();

    // Before writing anything: do the binaries the artifacts will name actually
    // exist? `Environment::discover` guarantees the paths are absolute, not that
    // they resolve, and a registration pointing at a binary that is not there
    // installs cleanly and then does nothing — the failure a user reads as "the
    // tools are just missing". Skipped for `uninstall`, which removes paths
    // rather than writing them. `need_bridge` is **true for this command and
    // only this command**: an MCP registration is the one artifact that names
    // the bridge, and it is what `agent` writes.
    if !matches!(cmd, AgentCmd::Uninstall { .. }) {
        filigrio_install::check_binaries(env, true, &mut report);
    }

    // Agents this scope cannot reach. Named ones fail immediately; swept ones are
    // collected, because eight adapters produce up to six of these and six
    // paragraph-length notes is how the one line that matters scrolls past.
    let mut out_of_reach: Vec<ClientId> = Vec::new();

    for id in agents {
        let installer = filigrio_install::installer_for(*id);
        let scope = match installer.scope_support().resolve(requested) {
            Ok(scope) => scope,
            Err(why) => {
                match selection {
                    // §17.2: every agent is named explicitly, so a scope
                    // mismatch is always a failure. The flag that would have
                    // included it is appended only when the adapter's own
                    // sentence has not already said so — two of the four reasons
                    // name `--global` themselves, and "…so drop --global to
                    // install it. Drop `--global` to include it" is the shape of
                    // a message assembled by a machine that is not reading
                    // itself.
                    Selection::Named => {
                        let hint = if why.contains("--global") {
                            String::new()
                        } else {
                            match requested {
                                Scope::Project => {
                                    " Re-run with `--global` to include it.".to_string()
                                }
                                Scope::Global => " Drop `--global` to include it.".to_string(),
                            }
                        };
                        report.fail(
                            format!("{}/scope", id.slug()),
                            scope_root(env, requested),
                            format!("{why}.{hint}"),
                        );
                    }
                    Selection::Swept => out_of_reach.push(*id),
                }
                continue;
            }
        };
        match cmd {
            AgentCmd::Install { .. } => installer.install(env, scope, &mut report),
            AgentCmd::Uninstall { .. } => installer.uninstall(env, scope, &mut report),
            AgentCmd::Status { .. } => installer.status(env, scope, &mut report),
        }
    }

    report_sweep_exclusions(env, cmd, requested, &out_of_reach, &mut report);

    println!("filigrio agent {verb} — {}", env.project_root.display());
    finish(&report, args.explain)
}

/// What a sweep did **not** reach, in one note.
///
/// A bare `uninstall` is scoped to the repository, so the agents that register
/// only under `$HOME` survive it. Saying nothing would make "remove everything"
/// a claim the command does not honour, and the user would find the leftovers
/// the way people always find leftovers — when something they uninstalled keeps
/// spawning a binary that is gone.
///
/// So the note counts what is **actually still there**, by asking each excluded
/// adapter's own `status` rather than asserting that anything is. "3 of 4 still
/// registered" and "none of them was registered" are different sentences and
/// only one of them is ever true; a note that always said the first would be the
/// permanent-alarm defect [`installed_verdict`] exists to avoid one layer down.
///
/// One note and not one per agent: with `--agent all` retired, the only sweeps
/// left are `uninstall` and `status`, where up to six adapters can be excluded at
/// once. Six paragraph-length notes reusing each adapter's own `SCOPE_NOTE` is
/// how a `!` line scrolls past unread — the exact verbosity `Notes::Aggregated`
/// exists to fight. It is emitted **unkinded**, so it is never folded into a
/// count: it names a flag, which makes it one of the handful of notes that asks
/// the user to do something.
fn report_sweep_exclusions(
    env: &filigrio_install::Environment,
    cmd: &AgentCmd,
    requested: Scope,
    excluded: &[ClientId],
    report: &mut filigrio_install::Report,
) {
    if excluded.is_empty() {
        return;
    }
    let elsewhere = match requested {
        Scope::Project => "outside this repository",
        Scope::Global => "inside this repository",
    };
    let flag = match requested {
        Scope::Project => "re-run with `--global`",
        Scope::Global => "drop `--global`",
    };
    let names = excluded
        .iter()
        .map(|id| id.slug())
        .collect::<Vec<_>>()
        .join(", ");

    // Probed at the adapter's **default** scope, which is where these artifacts
    // are: an agent lands here precisely because `requested` was the scope it
    // could not resolve to.
    let still_there: Vec<&str> = excluded
        .iter()
        .filter(|id| {
            let scope = filigrio_install::installer_for(**id)
                .scope_support()
                .default;
            matches!(
                installed_verdict(env, **id, scope),
                "yes" | "partly" | "stale"
            )
        })
        .map(|id| id.slug())
        .collect();

    let tail = match cmd {
        AgentCmd::Uninstall { .. } if still_there.is_empty() => {
            " None of them has anything installed to remove.".to_string()
        }
        AgentCmd::Uninstall { .. } => format!(
            " {} of {} still {} artifacts on disk ({}); {flag} to remove those too.",
            still_there.len(),
            excluded.len(),
            if still_there.len() == 1 {
                "has"
            } else {
                "have"
            },
            still_there.join(", ")
        ),
        _ => format!(" {flag} to include them."),
    };

    report.note(format!(
        "{} agent(s) keep their artifacts {elsewhere} and this {}-scope sweep did not touch them: \
         {names}.{tail}",
        excluded.len(),
        requested.slug()
    ));
}

/// The root a scope refusal names in its `!` line.
///
/// A refusal has no artifact and therefore no artifact path, and inventing one
/// would put a file on the screen that this run never considered. What it does
/// have is the root the user asked to write under, which is the fact the refusal
/// is *about*.
fn scope_root(env: &filigrio_install::Environment, scope: Scope) -> &Path {
    match scope {
        Scope::Project => &env.project_root,
        Scope::Global => &env.home,
    }
}

// ---------------------------------------------------------------------------
// `filigrio docs` (ADR-0034 §18.2)
// ---------------------------------------------------------------------------

/// `filigrio docs install | uninstall | status` (ADR-0034 §18.2).
///
/// The shortest of the four resource verbs, and every absence in it is argued
/// somewhere: no scope gate (one home, in the repository), no member resolution
/// (one artifact), no [`check_binaries`] bridge check (this document names the
/// bridge only as a path to quote back, and a doc that mentions a binary is not
/// inert the way a registration pointing at a missing one is — but the *CLI*
/// binary it tells the reader to run has to exist, so `need_bridge` is false and
/// the check still runs).
///
/// [`check_binaries`]: filigrio_install::check_binaries
fn run_docs(cmd: DocsCmd, socket: PathBuf) -> Result<()> {
    use filigrio_install::{docs, Report};

    let args = match &cmd {
        DocsCmd::Install { args } | DocsCmd::Uninstall { args } | DocsCmd::Status { args } => {
            args.clone()
        }
    };
    let verb = match &cmd {
        DocsCmd::Install { .. } => "install",
        DocsCmd::Uninstall { .. } => "uninstall",
        DocsCmd::Status { .. } => "status",
    };
    let env = install_environment(args.repo.as_deref(), socket)?;

    let mut report = Report::default();
    // `need_bridge` is false: the rendered section quotes the CLI as the command
    // a reader runs to refresh the graph, and names no bridge of its own.
    if !matches!(cmd, DocsCmd::Uninstall { .. }) {
        filigrio_install::check_binaries(&env, false, &mut report);
    }

    match &cmd {
        DocsCmd::Install { .. } => docs::install(&env, &mut report),
        DocsCmd::Uninstall { .. } => docs::uninstall(&env, &mut report),
        DocsCmd::Status { .. } => docs::status(&env, &mut report),
    }

    println!("filigrio docs {verb} — {}", env.project_root.display());
    finish(&report, args.explain)
}

// ---------------------------------------------------------------------------
// `filigrio hooks` (ADR-0034 §17, ADR-0035)
// ---------------------------------------------------------------------------

/// `filigrio hooks install | uninstall | status | run` (ADR-0034 §17).
fn run_hooks(cmd: HooksCmd, socket: PathBuf) -> Result<()> {
    use filigrio_install::hooks::{self, Hook, HOOKS};
    use filigrio_install::Report;

    // `run` shares the resource with the three management verbs but none of
    // their machinery: git has already chosen the event and the repository, so
    // there is no selection to resolve, no `Environment` to build and no
    // `Report` to render. It leaves before any of that exists rather than
    // threading an unused `HooksArgs` through the installer below.
    let (args, verb) = match &cmd {
        HooksCmd::Run { event, args } => {
            run_hook(event, args, &socket);
            return Ok(());
        }
        HooksCmd::Install { args } => (args.clone(), "install"),
        HooksCmd::Uninstall { args } => (args.clone(), "uninstall"),
        HooksCmd::Status { args } => (args.clone(), "status"),
    };

    // Kept before the move: the hook-target probe (ADR-0032b OQ4, below) needs
    // the same socket the hooks themselves would use.
    let hook_socket = socket.clone();
    let env = install_environment(args.repo.as_deref(), socket)?;

    // An unknown hook name is a hard error for the same reason an unknown agent
    // is: a silently-ignored `--hook post-comit` writes nothing and exits 0.
    let hooks: Vec<Hook> = if args.hooks.is_empty() {
        HOOKS.to_vec()
    } else {
        let named = args
            .hooks
            .iter()
            .map(|name| {
                Hook::parse(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown hook `{name}` — known: {}",
                        HOOKS.iter().map(|h| h.name).collect::<Vec<_>>().join(", ")
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        deduped(&named)
    };

    let mut report = Report::default();
    // `need_bridge` is false: a hook script names the CLI, never the bridge.
    if !matches!(cmd, HooksCmd::Uninstall { .. }) {
        filigrio_install::check_binaries(&env, false, &mut report);
    }

    match &cmd {
        HooksCmd::Install { .. } => hooks::install(&env, &hooks, &mut report),
        HooksCmd::Uninstall { .. } => hooks::uninstall(&env, &hooks, &mut report),
        HooksCmd::Status { .. } => hooks::status(&env, &hooks, &mut report),
        // `Run` returned above, before the environment existed. Named rather
        // than folded into a `_` arm so that a fifth verb added later is a
        // compile error here instead of a silent `status`.
        HooksCmd::Run { .. } => unreachable!("`hooks run` returns before the installer"),
    }

    println!("filigrio hooks {verb} — {}", env.project_root.display());
    print!(
        "{}",
        report.render(if args.explain {
            filigrio_install::Notes::Verbatim
        } else {
            filigrio_install::Notes::Aggregated
        })
    );

    // ADR-0032b OQ4 — hook-side outcome visibility.
    //
    // Installing the hooks proves the *scripts* are in place; it says nothing
    // about whether the changesets they submit reach anything. Delivery is
    // fire-and-forget (§4), so an unregistered project — including the
    // individually-registered monorepo, where the hook submits the repository
    // root and the daemon can only match an ancestor — declines every commit
    // in silence.
    //
    // This is the once-not-per-commit place to ask. It runs only for `status`
    // (a probe is a question, and `install` is not asking one). The family gate
    // it used to carry is gone because the family *is* the command now: the
    // other two resources cannot reach this code at all, which is a stronger
    // guarantee than the `families.contains(&Family::Hooks)` it replaces.
    // Deliberately **not** folded into `Report`: it is a diagnostic about the
    // daemon, not an artifact that was installed or failed, and it must not
    // change the exit status — a repository you have not registered yet is not
    // a broken installation.
    if matches!(cmd, HooksCmd::Status { .. }) {
        use filigrio_client_cli::hook::{HookEnv, HookTarget, SpoolDir};

        let spool = SpoolDir::new(HookEnv::from_env().spool_dir);
        let probe = HookTarget::probe(&env.project_root, &hook_socket, &spool);
        println!();
        for line in probe.report() {
            println!("{line}");
        }
    }

    if report.is_ok() {
        println!("✅ {} artifact(s) handled, no failures", report.steps.len());
        Ok(())
    } else {
        eprintln!(
            "❌ {} of {} artifact(s) failed — see the `!` lines above",
            report.failures.len(),
            report.steps.len() + report.failures.len()
        );
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// `filigrio completions` (ADR-0034 §6, §17)
// ---------------------------------------------------------------------------

/// `filigrio completions install | uninstall | status` (ADR-0034 §17).
///
/// No `--project`: a completion is a property of this user's shell and has
/// nothing to do with which repository they are standing in. The environment
/// still needs a project root — `Environment` is one struct — so the current
/// directory supplies it and nothing here reads it.
fn run_completions(cmd: CompletionsCmd, socket: PathBuf) -> Result<()> {
    use filigrio_install::completions::{self, ALL_SHELLS};
    use filigrio_install::{Report, Shell};

    let args = match &cmd {
        CompletionsCmd::Install { args }
        | CompletionsCmd::Uninstall { args }
        | CompletionsCmd::Status { args } => args.clone(),
    };
    let verb = match &cmd {
        CompletionsCmd::Install { .. } => "install",
        CompletionsCmd::Uninstall { .. } => "uninstall",
        CompletionsCmd::Status { .. } => "status",
    };
    let env = install_environment(None, socket)?;

    let shells: Vec<Shell> = if args.shells.is_empty() {
        ALL_SHELLS.to_vec()
    } else {
        let named = args
            .shells
            .iter()
            .map(|name| {
                Shell::parse(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown shell `{name}` — known: {}",
                        ALL_SHELLS
                            .iter()
                            .map(|s| s.slug())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        deduped(&named)
    };

    let mut command = <Cli as clap::CommandFactory>::command();
    let mut report = Report::default();
    // `need_bridge` is false: the completion trailer names the CLI, never the
    // bridge.
    if !matches!(cmd, CompletionsCmd::Uninstall { .. }) {
        filigrio_install::check_binaries(&env, false, &mut report);
    }

    match &cmd {
        CompletionsCmd::Install { .. } => {
            completions::install(&mut command, "filigrio", &env, &shells, &mut report)
        }
        CompletionsCmd::Uninstall { .. } => completions::uninstall(&env, &shells, &mut report),
        CompletionsCmd::Status { .. } => {
            completions::status(&mut command, "filigrio", &env, &shells, &mut report)
        }
    }

    println!("filigrio completions {verb}");
    finish(&report, args.explain)
}

/// Decide what a daemon response prints and whether it is a failure.
///
/// `Ok(text)` → stdout, exit 0; `Err(text)` → stderr, exit **non-zero**. Split
/// out from [`handle_response`] purely so the exit-status decision is testable
/// without `process::exit` — this is the ADR-0042 F6c contract that a failed
/// command must not exit 0, and it deserves a test rather than a code read.
fn render_response(response: filigrio_protocol::Response) -> std::result::Result<String, String> {
    match response {
        filigrio_protocol::Response::QueryResult { data } => {
            Ok(serde_json::to_string_pretty(&data)
                .unwrap_or_else(|_| serde_json::to_value(&data).unwrap_or_default().to_string()))
        }
        filigrio_protocol::Response::CommandCompleted { outcome } => Ok(describe_outcome(&outcome)),
        filigrio_protocol::Response::Error { message } => Err(format!("Error: {}", message)),
    }
}

/// Print a daemon response and map it onto the process exit status.
///
/// ADR-0042 F6c: a command's response IS its outcome (there is no ack to print
/// and no job to poll), and a failure **exits non-zero** — the old ack path
/// logged the failure daemon-side, printed "Command accepted", and exited 0.
fn handle_response(response: filigrio_protocol::Response) -> Result<()> {
    match render_response(response) {
        Ok(text) => {
            println!("{}", text);
            Ok(())
        }
        Err(text) => {
            eprintln!("{}", text);
            std::process::exit(1);
        }
    }
}

/// Write a `DataQuery::GraphReport` response to `out` (`-` = stdout).
///
/// The daemon renders the markdown; this is the whole client half of `graph
/// report`. Kept apart from [`handle_response`] because the payload is a
/// document, not a result to pretty-print — piping the JSON envelope into a
/// `.md` file would emit an escaped one-line string.
fn write_report(response: filigrio_protocol::Response, out: &Path) -> Result<()> {
    let data = match response {
        filigrio_protocol::Response::QueryResult { data } => data,
        filigrio_protocol::Response::Error { message } => {
            eprintln!("Error: {}", message);
            std::process::exit(1);
        }
        other => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
    };

    let markdown = data
        .get("markdown")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("GraphReport response carried no `markdown` field"))?;

    if out.as_os_str() == "-" {
        print!("{markdown}");
    } else {
        std::fs::write(out, markdown).with_context(|| format!("write {}", out.display()))?;
        println!("report → {}", out.display());
    }
    Ok(())
}

/// The F8 tail of a `changed: N` line (ADR-0042 Phase 1c). Empty when nothing
/// vanished, so the common line is unchanged; present whenever the apply
/// converged a file away, because "silently converged" must not become the new
/// silent failure (ADR-0029).
fn vanished_suffix(vanished: usize) -> String {
    if vanished == 0 {
        String::new()
    } else {
        format!(", {vanished} vanished (deleted mid-apply, converged as removals)")
    }
}

/// One human line per typed outcome (ADR-0042 F6c).
fn describe_outcome(outcome: &filigrio_protocol::CommandOutcome) -> String {
    use filigrio_protocol::CommandOutcome as O;
    match outcome {
        O::Indexed {
            project,
            changed,
            vanished,
        } if *changed == 0 => {
            format!(
                "✅ {project}: already up to date (0 files changed){}",
                vanished_suffix(*vanished)
            )
        }
        O::Indexed {
            project,
            changed,
            vanished,
        } => format!(
            "✅ {project}: indexed, {changed} file(s) changed{}",
            vanished_suffix(*vanished)
        ),
        O::Applied {
            project,
            changed,
            vanished,
        } if *changed == 0 => {
            format!(
                "✅ {project}: no effective change after the scope+dedup gate{}",
                vanished_suffix(*vanished)
            )
        }
        O::Applied {
            project,
            changed,
            vanished,
        } => format!(
            "✅ {project}: applied, {changed} file(s) changed{}",
            vanished_suffix(*vanished)
        ),
        O::Exported { project, path } => format!("✅ {project}: exported → {path}"),
        // ADR-0042 F4 — `wrote: false` is the honest idempotent case (the
        // project's resident state already matched its store), not a failure.
        O::Flushed {
            project,
            wrote: true,
        } => format!("✅ {project}: flushed to disk"),
        O::Flushed {
            project,
            wrote: false,
        } => {
            format!("✅ {project}: already persisted (nothing to flush)")
        }
        O::Watch {
            project,
            watching,
            changed,
            vanished,
            note,
        } => {
            let state = if *watching {
                "watching"
            } else {
                "not watching"
            };
            let converged = changed
                .map(|c| {
                    format!(
                        " (converged: {c} file(s) changed{})",
                        vanished_suffix(vanished.unwrap_or(0))
                    )
                })
                .unwrap_or_default();
            let note = note.as_ref().map(|n| format!(" — {n}")).unwrap_or_default();
            format!("✅ {project}: {state}{converged}{note}")
        }
        O::Registered { project, path } => format!("✅ Project registered: {project} ({path})"),
        O::Removed { project } => format!("✅ Project removed: {project}"),
        O::Stopping => "✅ Daemon is shutting down".to_string(),
    }
}

/// The ADR-0032b hook entry point: `filigrio hooks run <event> [git's argv…]`.
///
/// Returns `()`, not `Result`, and the caller cannot make it fail — that is the
/// §4 contract in the type. Everything it has to say goes to **stderr**
/// (git interleaves hook output into the command's own, and stdout belongs to
/// `filigrio graph … | jq`).
///
/// Synchronous inside an async `main`: the hook does one `git diff` and one
/// socket write, neither of which is async here, and the whole point is that it
/// returns before git notices.
fn run_hook(event: &str, args: &[String], socket: &Path) {
    use filigrio_client_cli::hook::{self, HookEnv, HookEvent};

    // Only `post-rewrite` is fed on stdin. Reading it unconditionally would
    // block on whatever pipe the invoking shell left open.
    let stdin = match HookEvent::parse(event) {
        Some(e) if e.reads_stdin() => {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
            buf
        }
        _ => String::new(),
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let outcome = hook::run(event, args, &stdin, &cwd, socket, &HookEnv::from_env());
    for line in outcome.report() {
        // The prefix names the verb a reader can actually type. Left as
        // `[filigrio hook]` it would point, from inside git's own output, at
        // the one spelling this CLI no longer has.
        eprintln!("[filigrio hooks run] {line}");
    }
}

async fn run_daemon(cmd: DaemonCmd, socket_path: PathBuf) -> Result<()> {
    match cmd {
        DaemonCmd::Start { idle_timeout } => {
            info!("Starting daemon on {}", socket_path.display());

            let daemon_exe = std::env::current_exe()
                .map(|exe| exe.with_file_name("filigrio-daemon"))
                .unwrap_or_else(|_| PathBuf::from("filigrio-daemon"));

            // Spawn daemon process directly
            println!("Starting daemon...");
            println!("  Socket: {}", socket_path.display());
            println!("  Idle timeout: {} seconds", idle_timeout);

            let daemon = std::process::Command::new(&daemon_exe)
                .arg("--socket")
                .arg(&socket_path)
                .arg("start")
                .arg("--idle-timeout")
                .arg(idle_timeout.to_string())
                .spawn()
                .context("failed to start daemon process")?;

            println!("Daemon started with PID: {}", daemon.id());
            Ok(())
        }
        DaemonCmd::Stop => {
            info!("Stopping daemon on {}", socket_path.display());
            // Send `DaemonStop` over THIS socket — never pgrep/kill a PID. A PID hunt
            // races and can terminate an unrelated daemon on a different socket (it
            // did exactly that in benchmarking). No connection here = no daemon here.
            match attach_to_daemon(&socket_path) {
                Some(client) => match client.stop() {
                    Ok(_) => println!("Stop signal sent to daemon on {}", socket_path.display()),
                    Err(e) => eprintln!("Failed to send stop: {e}"),
                },
                None => println!("No daemon running on {}", socket_path.display()),
            }
            Ok(())
        }
        DaemonCmd::Status => {
            info!("Checking daemon status on {}", socket_path.display());

            match attach_to_daemon(&socket_path) {
                Some(client) => {
                    let health = client.health()?;
                    println!("Daemon Status:");
                    println!("  State: running");
                    println!("  Uptime: {} seconds", health.uptime_secs);
                    println!("  Projects: {}", health.project_count);
                    println!("  Queue depth: {}", health.queue_depth);
                    println!("  Applies in flight: {}", health.applies_inflight);
                    println!("  Memory: {} MB", health.memory_bytes / (1024 * 1024));
                    println!("  Last activity: {}", health.last_activity);
                }
                None => {
                    println!("Daemon Status:");
                    println!("  State: stopped");
                    println!("  Use 'filigrio daemon start' to launch");
                }
            }

            Ok(())
        }
    }
}

async fn run_project(cmd: ProjectCmd, socket_path: PathBuf, use_daemon: bool) -> Result<()> {
    // For write commands, we need to handle --no-daemon mode properly
    if !use_daemon {
        info!("Using one-shot mode for project command (requires the resident daemon's registry)");
        // Project commands need the daemon's persistent registry/lifecycle.
        eprintln!("⚠️  Project commands require the resident daemon");
        eprintln!("   Start the daemon with: filigrio daemon start");
        eprintln!("   Or run without --no-daemon flag");
        std::process::exit(1);
    }

    let client = connect_to_daemon(&socket_path).await?;

    match cmd {
        ProjectCmd::Register { force } => {
            let cwd = resolve_project_path()?;
            let project_path = cwd.to_string_lossy().to_string();

            println!("Registering project: {}", cwd.display());

            if force {
                println!("Forced registration requested");
            }

            // Send project registration command to daemon. Registration is
            // exactly registration (ADR-0042 F6b): it neither indexes nor
            // starts a watcher — both are explicit follow-up verbs.
            match client.project_register(project_path.clone()) {
                Ok(()) => {
                    println!("✅ Project registered successfully: {}", project_path);
                    println!(
                        "   Use 'filigrio project index {}' to build the initial graph",
                        project_path
                    );
                    println!("   Use 'filigrio project watch on' to converge and then follow changes live");
                }
                Err(e) => {
                    eprintln!("❌ Failed to register project: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        ProjectCmd::Remove { project } => {
            println!("Removing project: {}", project);

            // Send project removal command to daemon
            match client.project_remove(project.clone()) {
                Ok(()) => {
                    println!("✅ Project removed successfully: {}", project);
                }
                Err(e) => {
                    eprintln!("❌ Failed to remove project: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        ProjectCmd::List => {
            println!("Registered projects:");

            // Query daemon health to get project count
            match client.health() {
                Ok(health) => {
                    if health.project_count == 0 {
                        println!("  No projects registered");
                        println!("  Use 'filigrio project register' to add a project");
                    } else {
                        println!("  {} project(s) registered", health.project_count);
                        // In a full implementation, we'd have a dedicated projects list command
                        println!("  (Detailed project list requires additional protocol support)");
                    }
                }
                Err(e) => {
                    eprintln!("❌ Failed to get project list: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        ProjectCmd::Status { project } => {
            let project_name = project.unwrap_or_else(|| {
                // Default to current directory
                resolve_project_path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            });

            println!("Project status: {}", project_name);

            // Get project status from daemon
            match client.project_status(project_name.clone()) {
                Ok(status) => {
                    println!("✅ Project Status:");
                    println!("   State: {:?}", status);
                    // In a full implementation, we'd display more detailed status information
                }
                Err(e) => {
                    eprintln!("❌ Failed to get project status: {}", e);
                    println!("   Make sure the project is registered: 'filigrio project register'");
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        ProjectCmd::Index { project, clean } => {
            let project_name = project.unwrap_or_else(|| {
                resolve_project_path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            });

            println!("Indexing project: {}", project_name);

            // Send project index command to daemon. `clean` (wipe-and-reindex)
            // is reserved (ADR-0042 F5): the daemon rejects it with an explicit
            // "not implemented yet" error rather than silently ignoring it.
            // Reconcile depth is not a wire concept (ADR-0042 F6b) — a wire
            // index is always the full reconcile; the shallow fast-path lives
            // on the daemon-internal `Op`, ridden only by the watcher lane.
            // The call blocks until the index completes (F6c).
            let request =
                filigrio_protocol::Request::command(filigrio_protocol::Command::ProjectIndex {
                    project: project_name.clone(),
                    clean,
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("❌ Indexing failed: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        ProjectCmd::Watch { state, project } => {
            let project_name = project.unwrap_or_else(|| {
                resolve_project_path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            });
            let on = state == WatchState::On;

            if on {
                println!(
                    "Watching project (re-checking the index first): {}",
                    project_name
                );
            } else {
                println!("Stopping watch for project: {}", project_name);
            }

            // ADR-0042 F6b: `watch on` starts the watcher first (events begin
            // buffering, so nothing is lost during the converge), then runs a
            // synchronous deep reconcile before responding.
            let request =
                filigrio_protocol::Request::command(filigrio_protocol::Command::ProjectWatch {
                    project: project_name.clone(),
                    on,
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("❌ Watch {} failed: {}", state, e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        ProjectCmd::Flush { project } => {
            let project_name = project.unwrap_or_else(|| {
                resolve_project_path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            });

            println!("Flushing project to disk: {}", project_name);

            // ADR-0042 F4/B12: the explicit persistence trigger.
            let request =
                filigrio_protocol::Request::command(filigrio_protocol::Command::ProjectFlush {
                    project: project_name.clone(),
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("❌ Flush failed: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        ProjectCmd::Export { project } => {
            let project_name = project.unwrap_or_else(|| {
                resolve_project_path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            });

            println!("Exporting graph.json for project: {}", project_name);

            // Send project export command to daemon (ADR-0042 F2: the explicit
            // export verb — the only producer of graph.json).
            let request =
                filigrio_protocol::Request::command(filigrio_protocol::Command::ProjectExport {
                    project: project_name.clone(),
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("❌ Export failed: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
    }
}

async fn run_graph(cmd: GraphCmd, socket_path: PathBuf, use_daemon: bool) -> Result<()> {
    // Read before `cmd` is consumed building the request, and read once for both
    // lifecycles below — the same reason `report_out` is read here. Every verb
    // carries the flag, so the match is exhaustive rather than defaulted: a verb
    // added without a `GraphArgs` fails to compile instead of silently answering
    // for the current directory whatever the user passed.
    let project = match &cmd {
        GraphCmd::Query { args, .. }
        | GraphCmd::God { args, .. }
        | GraphCmd::Path { args, .. }
        | GraphCmd::Stats { args, .. }
        | GraphCmd::GetNode { args, .. }
        | GraphCmd::Neighbors { args, .. }
        | GraphCmd::ProjectGraph { args, .. }
        | GraphCmd::Communities { args, .. }
        | GraphCmd::Community { args, .. }
        | GraphCmd::Report { args, .. } => args.project.clone(),
    }
    // No `--project` is exactly the behavior that predates the flag, down to the
    // literal `unknown` on an unresolvable directory: the daemon answers that
    // with "no such project", which is a legible failure, and the alternative —
    // refusing here — would break every `graph` call made from a directory this
    // client cannot canonicalise but the daemon knows perfectly well.
    .unwrap_or_else(|| {
        resolve_project_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    });

    info!("CLI mode decision: use_daemon={}", use_daemon);

    // `graph report` writes a document rather than printing a result, and both
    // lifecycles below share that tail — so the destination is read once, here,
    // before `cmd` is consumed building the request.
    let report_out = match &cmd {
        GraphCmd::Report { out, .. } => Some(out.clone()),
        _ => None,
    };

    if !use_daemon {
        info!("Using proper one-shot mode with active socket connection");

        // One-shot mode: Spawn daemon and actively communicate via socket
        let daemon_exe = std::env::current_exe()
            .map(|exe| exe.with_file_name("filigrio-daemon"))
            .unwrap_or_else(|_| PathBuf::from("filigrio-daemon"));

        // Build the request-based command
        let request = match cmd {
            GraphCmd::Query {
                query,
                depth,
                budget,
                ..
            } => filigrio_protocol::Request::data(filigrio_protocol::DataQuery::Query {
                project,
                params: filigrio_protocol::QueryParams {
                    query,
                    mode: filigrio_protocol::TraversalMode::default(),
                    depth: depth as u8,
                    budget,
                    token_budget: 4000,
                    relations: vec![],
                    include_unresolved: false,
                },
            }),
            GraphCmd::God { top, .. } => {
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::GodNodes {
                    project,
                    limit: top,
                })
            }
            GraphCmd::Path {
                from, to, max_hops, ..
            } => filigrio_protocol::Request::data(filigrio_protocol::DataQuery::Path {
                project,
                from: filigrio_protocol::NodeAddress::parse(&from),
                to: filigrio_protocol::NodeAddress::parse(&to),
                max_hops: max_hops as u8,
            }),
            GraphCmd::Stats { .. } => {
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::GraphStats {
                    project: project.clone(),
                })
            }
            GraphCmd::GetNode { address, .. } => {
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::GetNode {
                    project,
                    node_address: filigrio_protocol::NodeAddress::parse(&address),
                })
            }
            GraphCmd::Neighbors {
                address, direction, ..
            } => filigrio_protocol::Request::data(filigrio_protocol::DataQuery::Neighbors {
                project,
                node: filigrio_protocol::NodeAddress::parse(&address),
                direction: direction.into(),
                relations: vec![],
                include_unresolved: false,
            }),
            GraphCmd::ProjectGraph { .. } => {
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::ProjectGraph {
                    project,
                })
            }
            // `--top` onto the wire's `limit`: the CLI spells every ranked cut
            // `--top` (god, report) and the contract spells it `limit`
            // (`GodNodes`, `ListCommunities`), so the two names meet here and
            // nowhere else.
            GraphCmd::Communities { top, .. } => {
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::ListCommunities {
                    project,
                    limit: top,
                })
            }
            GraphCmd::Community { id, .. } => {
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::Community {
                    project,
                    community_id: id,
                })
            }
            GraphCmd::Report { top, .. } => {
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::GraphReport {
                    project,
                    top,
                })
            }
        };

        // Spawn one-shot daemon with socket path. The execution budget comes
        // from `OneShotConfig::default()` (≈10 min, in step with the client's
        // command read timeout — ADR-0042 F6c) rather than a local literal, so
        // the two can't drift apart.
        let config = filigrio_client_core::OneShotConfig {
            daemon_exe,
            forward_io: true,
            ..Default::default()
        };

        let runner = filigrio_client_core::OneShotRunner::new(config)?;
        runner.start().await?;

        // Connect with exponential backoff retry (handles daemon startup latency)
        info!(
            "Connecting to one-shot daemon at {}",
            runner.socket_path().display()
        );
        let client = runner.client_when_ready().await.context(
            "failed to connect to one-shot daemon (daemon may not be starting properly)",
        )?;

        info!("Sending request to one-shot daemon");
        let response = client
            .send(request)
            .context("failed to send request to one-shot daemon")?;

        info!("Got response from one-shot daemon");
        match &report_out {
            Some(out) => write_report(response, out)?,
            None => handle_response(response)?,
        }

        let exit_code = runner.wait_for_completion_with_timeout().await?;

        if exit_code != 0 {
            std::process::exit(exit_code);
        }

        return Ok(());
    }

    // Daemon mode: connect to existing or spawn daemon
    info!("CLI_MODE: DAEMON - Using daemon mode (per ADR-0032f §5)");
    let client = connect_to_daemon(&socket_path).await?;

    match cmd {
        GraphCmd::Query {
            query,
            depth,
            budget,
            ..
        } => {
            let request = filigrio_protocol::Request::data(filigrio_protocol::DataQuery::Query {
                project,
                params: filigrio_protocol::QueryParams {
                    query,
                    mode: filigrio_protocol::TraversalMode::default(),
                    depth: depth as u8,
                    budget,
                    token_budget: 4000,
                    relations: vec![],
                    include_unresolved: false,
                },
            });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("Query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::God { top, .. } => {
            let request =
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::GodNodes {
                    project,
                    limit: top,
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("God nodes query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::Path {
            from, to, max_hops, ..
        } => {
            let request = filigrio_protocol::Request::data(filigrio_protocol::DataQuery::Path {
                project,
                from: filigrio_protocol::NodeAddress::parse(&from),
                to: filigrio_protocol::NodeAddress::parse(&to),
                max_hops: max_hops as u8,
            });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("Path query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::Stats { .. } => {
            let request =
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::GraphStats {
                    project: project.clone(),
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("Graph stats query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::GetNode { address, .. } => {
            let request = filigrio_protocol::Request::data(filigrio_protocol::DataQuery::GetNode {
                project,
                node_address: filigrio_protocol::NodeAddress::parse(&address),
            });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("Get node query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::Neighbors {
            address, direction, ..
        } => {
            println!("Neighbors of {} ({} direction):", address, direction);

            let request =
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::Neighbors {
                    project,
                    node: filigrio_protocol::NodeAddress::parse(&address),
                    direction: direction.into(),
                    relations: vec![], // No relation filtering by default
                    include_unresolved: false,
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("Neighbors query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::Report { out, top, .. } => {
            let request =
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::GraphReport {
                    project,
                    top,
                });

            match client.send(request) {
                Ok(response) => write_report(response, &out)?,
                Err(e) => {
                    eprintln!("Graph report query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::Communities { top, .. } => {
            let request =
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::ListCommunities {
                    project,
                    limit: top,
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("List communities query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::Community { id, .. } => {
            let request =
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::Community {
                    project,
                    community_id: id,
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("Community query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
        GraphCmd::ProjectGraph { .. } => {
            println!("Project graph (monorepo architecture):");

            // Build project graph query request
            let request =
                filigrio_protocol::Request::data(filigrio_protocol::DataQuery::ProjectGraph {
                    project,
                });

            match client.send(request) {
                Ok(response) => {
                    handle_response(response)?;
                }
                Err(e) => {
                    eprintln!("Project graph query error: {}", e);
                    std::process::exit(1);
                }
            }

            Ok(())
        }
    }
}

/// A client for `socket_path`, **auto-starting** the daemon if nothing is
/// listening (ADR-0032 §1: first use in a repo starts or attaches to it,
/// ssh-agent style).
///
/// Hangs off the real [`DaemonClient::is_reachable`] probe, not off a
/// constructor's error arm. `async` deliberately: building a *second*
/// `tokio::runtime::Runtime` and `block_on`-ing it from inside `#[tokio::main]`
/// panics with "Cannot start a runtime from within a runtime" — verified by
/// running it, so this must never be made synchronous.
async fn connect_to_daemon(socket_path: &Path) -> Result<DaemonClient> {
    let client = DaemonClient::new(socket_path);
    if client.is_reachable() {
        return Ok(client);
    }

    info!("Daemon not running, attempting auto-start");

    let daemon_exe = std::env::current_exe()
        .map(|exe| exe.with_file_name("filigrio-daemon"))
        .unwrap_or_else(|_| PathBuf::from("filigrio-daemon"));

    let config = filigrio_client_core::AutoStartConfig {
        daemon_exe,
        socket_path: socket_path.to_path_buf(),
        ..Default::default()
    };

    let ready = filigrio_client_core::AutoStartHandshake::new(config)
        .ensure_daemon_ready()
        .await
        .context("Auto-start handshake failed")?;

    if !ready {
        return Err(anyhow::anyhow!("Failed to start daemon automatically"));
    }

    Ok(DaemonClient::new(socket_path))
}

/// A client for an **already-running** daemon, or `None` if nothing is
/// listening. Deliberately never auto-starts: the lifecycle verbs
/// (`daemon status`, `daemon stop`) ask *whether* a daemon is running, and a
/// question that starts one to answer it is not a question.
fn attach_to_daemon(socket_path: &Path) -> Option<DaemonClient> {
    let client = DaemonClient::new(socket_path);
    client.is_reachable().then_some(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_parsing() {
        Cli::command().debug_assert();
    }

    /// `graph report` kept classic's flag surface verbatim when it moved here
    /// (`--out GRAPH_REPORT.md`, `--top 20`) — the verb changed *lifecycle*, not
    /// spelling, so a CI job that called the old one keeps working.
    #[test]
    fn graph_report_keeps_classics_flags_and_defaults() {
        let cli = Cli::try_parse_from(["filigrio", "graph", "report"]).unwrap();
        let AppCommand::Graph {
            cmd: GraphCmd::Report { out, top, .. },
        } = cli.command
        else {
            panic!("expected graph report");
        };
        assert_eq!(out, PathBuf::from("GRAPH_REPORT.md"));
        assert_eq!(top, 20);

        let cli =
            Cli::try_parse_from(["filigrio", "graph", "report", "--out", "R.md", "--top", "7"])
                .unwrap();
        let AppCommand::Graph {
            cmd: GraphCmd::Report { out, top, .. },
        } = cli.command
        else {
            panic!("expected graph report");
        };
        assert_eq!(out, PathBuf::from("R.md"));
        assert_eq!(top, 7);
    }

    /// The report response is a *document*: `write_report` must emit the raw
    /// markdown, never the JSON envelope `handle_response` would pretty-print
    /// (which would write an escaped one-line string into a `.md` file).
    #[test]
    fn write_report_writes_the_markdown_not_the_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("R.md");
        let response = filigrio_protocol::Response::QueryResult {
            data: serde_json::json!({"markdown": "# Graph Report\n\n**1 nodes**\n"}),
        };

        write_report(response, &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "# Graph Report\n\n**1 nodes**\n"
        );
    }

    /// A response with no `markdown` field is an error, not an empty report —
    /// silently writing a zero-byte `GRAPH_REPORT.md` is the failure mode that
    /// looks like success.
    #[test]
    fn write_report_refuses_a_response_without_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("R.md");
        let response = filigrio_protocol::Response::QueryResult {
            data: serde_json::json!({"project": "p"}),
        };

        let err = write_report(response, &out).unwrap_err();
        assert!(err.to_string().contains("markdown"), "got: {err}");
        assert!(
            !out.exists(),
            "nothing may be written on a malformed response"
        );
    }

    #[test]
    fn test_daemon_start_command() {
        let cli =
            Cli::try_parse_from(["filigrio", "daemon", "start", "--idle-timeout", "600"]).unwrap();

        assert!(matches!(
            cli.command,
            AppCommand::Daemon {
                cmd: DaemonCmd::Start { idle_timeout: 600 }
            }
        ));
    }

    #[test]
    fn test_graph_query_command() {
        let cli = Cli::try_parse_from([
            "filigrio",
            "graph",
            "query",
            "test query",
            "--depth",
            "3",
            "--budget",
            "50",
        ])
        .unwrap();

        if let AppCommand::Graph {
            cmd:
                GraphCmd::Query {
                    query,
                    depth,
                    budget,
                    ..
                },
        } = cli.command
        {
            assert_eq!(query, "test query");
            assert_eq!(depth, 3);
            assert_eq!(budget, 50);
        } else {
            panic!("Expected Graph Query command");
        }
    }

    /// **Every** `graph` verb takes `--project`, and it takes a name *or* a path.
    ///
    /// The daemon federates registered projects and resolves either spelling, so
    /// the CLI limiting itself to the current directory was a limit of the client
    /// alone: asking about a second repository meant a `cd`. Asserted across
    /// *every* verb rather than on one, because the flag is only useful if it is
    /// everywhere — a verb that quietly lacks it is the one a user finds at the
    /// moment they need it. The list is checked against the parser's own
    /// subcommand set below, so a verb added without a `GraphArgs` cannot slip
    /// past by simply being absent from this array.
    #[test]
    fn every_graph_verb_takes_a_project_that_is_a_name_or_a_path() {
        let verbs = [
            vec!["query", "auth"],
            vec!["god"],
            vec!["path", "a", "b"],
            vec!["stats"],
            vec!["get-node", "x"],
            vec!["neighbors", "x"],
            vec!["project-graph"],
            vec!["communities"],
            vec!["community", "7"],
            vec!["report"],
        ];

        // The array above is a list of *invocations* (some verbs need
        // positionals), so it cannot be derived from clap — but it can be held
        // to clap's count, which is what stops it from silently covering nine of
        // ten verbs.
        let command = Cli::command();
        let graph = command
            .get_subcommands()
            .find(|c| c.get_name() == "graph")
            .expect("`graph` must be a subcommand");
        assert_eq!(
            verbs.len(),
            graph.get_subcommands().count(),
            "a `graph` verb was added without a row here — every one of them takes --project"
        );

        for verb in verbs {
            let mut argv = vec!["filigrio", "graph"];
            argv.extend(verb.iter().copied());
            argv.extend(["--project", "myproj"]);
            assert!(
                Cli::try_parse_from(&argv).is_ok(),
                "{argv:?} must parse — --project is not optional per verb"
            );
        }

        // A registry id, which a `PathBuf` would have accepted but nothing else
        // in this CLI would then have treated as one.
        let cli =
            Cli::try_parse_from(["filigrio", "graph", "query", "auth", "--project", "myproj"])
                .unwrap();
        let AppCommand::Graph {
            cmd: GraphCmd::Query { ref args, .. },
        } = cli.command
        else {
            panic!("expected graph query");
        };
        assert_eq!(args.project.as_deref(), Some("myproj"));

        // …and omitting it is not "no project": `run_graph` reads it as the
        // current directory, which is what this CLI did before the flag existed.
        let cli = Cli::try_parse_from(["filigrio", "graph", "stats"]).unwrap();
        let AppCommand::Graph {
            cmd: GraphCmd::Stats { ref args },
        } = cli.command
        else {
            panic!("expected graph stats");
        };
        assert!(args.project.is_none());
    }

    /// The communities pair parses: `communities` lists (bounded, default 20),
    /// `community <id>` shows one, and both take `--project` like every other
    /// `graph` verb.
    ///
    /// **20 is the MCP bridge's number**, not a fresh choice — `list_communities`
    /// has defaulted there since it shipped, and a human and an agent asking the
    /// same daemon the same question should not get differently-sized answers
    /// because of which transport they reached it through.
    #[test]
    fn the_communities_pair_parses_with_a_bounded_default_and_a_project() {
        let cli = Cli::try_parse_from(["filigrio", "graph", "communities"]).unwrap();
        let AppCommand::Graph {
            cmd: GraphCmd::Communities { top, ref args },
        } = cli.command
        else {
            panic!("expected graph communities");
        };
        assert_eq!(top, 20, "the bridge's `list_communities` default");
        assert!(args.project.is_none(), "no --project means the cwd");

        let cli = Cli::try_parse_from([
            "filigrio",
            "graph",
            "communities",
            "--top",
            "5",
            "--project",
            "myproj",
        ])
        .unwrap();
        let AppCommand::Graph {
            cmd: GraphCmd::Communities { top, ref args },
        } = cli.command
        else {
            panic!("expected graph communities");
        };
        assert_eq!(top, 5);
        assert_eq!(args.project.as_deref(), Some("myproj"));

        // The id is positional and typed: `community` without one, or with a
        // word, is a parse error rather than a query for community 0.
        let cli =
            Cli::try_parse_from(["filigrio", "graph", "community", "7", "--project", "myproj"])
                .unwrap();
        let AppCommand::Graph {
            cmd: GraphCmd::Community { id, ref args },
        } = cli.command
        else {
            panic!("expected graph community");
        };
        assert_eq!(id, 7);
        assert_eq!(args.project.as_deref(), Some("myproj"));

        for bad in [
            vec!["filigrio", "graph", "community"],
            vec!["filigrio", "graph", "community", "seven"],
            vec!["filigrio", "graph", "community", "-1"],
            vec!["filigrio", "graph", "communities", "--limit", "5"],
        ] {
            assert!(
                Cli::try_parse_from(&bad).is_err(),
                "{bad:?} must not parse — an id is required and typed, and the flag is --top"
            );
        }
    }

    /// **Every tool the MCP bridge serves is also a `graph` verb**, so the two
    /// transports over one daemon cannot drift back into two surfaces.
    ///
    /// They did: `list_communities` and `get_community` were bridge-only, an
    /// agent was taught to call them by the capability doc, and a human could
    /// reach communities only by writing a whole `GRAPH_REPORT.md`. That gap
    /// opened silently because nothing compared the two lists, and this is the
    /// comparison.
    ///
    /// **It is a written-down table and not reflection, because there is
    /// nothing here to reflect over.** `DataQuery` carries no record of who
    /// reaches which variant; the bridge is a separate binary this crate must
    /// not link (ADR-0032f §1 — the dependency would drag `filigrio-core` into
    /// the engine-free client); and the two surfaces spell the same query
    /// differently on purpose, so even a shared list would need this mapping
    /// (`god_nodes` is `graph god`, `query_graph` is `graph query`).
    ///
    /// What keeps the table from going stale is that **both of its columns are
    /// checked against something live**. The tool names must be exactly
    /// `filigrio_install::capability::TOOLS`, which
    /// `filigrio-client-mcp/tests/capability_doc_drift.rs` pins to the binary's
    /// own `tools/list` over the wire; the verb names must exist in the real
    /// `Cli::command()`. A tool added to the bridge therefore fails here for
    /// want of a row, and a row naming a verb this CLI does not have fails here
    /// too — neither can be satisfied by editing this file alone.
    #[test]
    fn every_mcp_tool_is_reachable_as_a_graph_verb() {
        // (bridge tool, `filigrio graph` verb).
        const PAIRS: &[(&str, &str)] = &[
            ("graph_stats", "stats"),
            ("project_graph", "project-graph"),
            ("god_nodes", "god"),
            ("query_graph", "query"),
            ("get_node", "get-node"),
            ("get_neighbors", "neighbors"),
            ("shortest_path", "path"),
            ("list_communities", "communities"),
            ("get_community", "community"),
        ];

        let command = Cli::command();
        let graph = command
            .get_subcommands()
            .find(|c| c.get_name() == "graph")
            .expect("`graph` must be a subcommand");
        let verbs: Vec<&str> = graph.get_subcommands().map(|c| c.get_name()).collect();

        for (tool, verb) in PAIRS {
            assert!(
                verbs.contains(verb),
                "the bridge serves `{tool}` but `filigrio graph {verb}` does not exist — \
                 the two transports would expose different surfaces over one daemon"
            );
        }

        let served: Vec<&str> = filigrio_install::capability::TOOLS
            .iter()
            .map(|t| t.name)
            .collect();
        for tool in &served {
            assert!(
                PAIRS.iter().any(|(t, _)| t == tool),
                "the bridge serves `{tool}` and no row pairs it with a `graph` verb — \
                 add the verb, then the row"
            );
        }
        assert_eq!(
            PAIRS.len(),
            served.len(),
            "a row names a tool the bridge no longer serves: {PAIRS:?} vs {served:?}"
        );
    }

    /// `--direction` is a value set the **parser** checks, and `both` is still
    /// the default.
    ///
    /// It was a `String` matched into `Direction` at use, which meant a typo was
    /// caught after the daemon had been connected to — and, under `--no-daemon`,
    /// after one had been spawned. The three words are unchanged, so a command
    /// line anyone already typed keeps working.
    #[test]
    fn neighbors_direction_is_a_checked_value_set_defaulting_to_both() {
        let cli = Cli::try_parse_from(["filigrio", "graph", "neighbors", "x"]).unwrap();
        let AppCommand::Graph {
            cmd: GraphCmd::Neighbors { direction, .. },
        } = cli.command
        else {
            panic!("expected graph neighbors");
        };
        assert_eq!(direction, EdgeDirection::Both);

        for (word, expected) in [
            ("incoming", filigrio_protocol::Direction::In),
            ("outgoing", filigrio_protocol::Direction::Out),
            ("both", filigrio_protocol::Direction::Both),
        ] {
            let cli =
                Cli::try_parse_from(["filigrio", "graph", "neighbors", "x", "--direction", word])
                    .unwrap();
            let AppCommand::Graph {
                cmd: GraphCmd::Neighbors { direction, .. },
            } = cli.command
            else {
                panic!("expected graph neighbors");
            };
            assert_eq!(filigrio_protocol::Direction::from(direction), expected);
            assert_eq!(direction.to_string(), word);
        }

        for bad in ["in", "out", "sideways", "Both"] {
            assert!(
                Cli::try_parse_from(["filigrio", "graph", "neighbors", "x", "--direction", bad])
                    .is_err(),
                "`--direction {bad}` must be a parse error, not a query"
            );
        }
    }

    /// ADR-0042 F5/F7 — `--clean` (reindex from scratch) parses on
    /// `project index`, and its absence means `clean: false` (today's behavior,
    /// unchanged). `--force` is NOT this flag: it belongs to `project register`
    /// (F7 — one word, one meaning), so it must be rejected here.
    #[test]
    fn test_project_index_clean_flag() {
        let cli = Cli::try_parse_from(["filigrio", "project", "index", "--clean"]).unwrap();
        assert!(matches!(
            cli.command,
            AppCommand::Project {
                cmd: ProjectCmd::Index {
                    project: None,
                    clean: true
                }
            }
        ));

        let cli = Cli::try_parse_from(["filigrio", "project", "index"]).unwrap();
        assert!(matches!(
            cli.command,
            AppCommand::Project {
                cmd: ProjectCmd::Index {
                    project: None,
                    clean: false
                }
            }
        ));

        // F7 — the renamed flag is a dead flag on this verb: `--force` must be
        // a parse error, never silently accepted as the reserved wipe.
        assert!(
            Cli::try_parse_from(["filigrio", "project", "index", "--force"]).is_err(),
            "`--force` must not parse on `project index` (F7: it belongs to `project register`)"
        );
    }

    /// ADR-0042 F5 — `project build` is gone, collapsed into `project index`.
    #[test]
    fn test_project_build_no_longer_parses() {
        assert!(Cli::try_parse_from(["filigrio", "project", "build"]).is_err());
    }

    /// ADR-0042 F4 — the explicit persistence verb parses, defaults to the
    /// current directory, and is spelled **flush** (Phase 3 owns "checkpoint").
    #[test]
    fn test_project_flush_command() {
        let cli = Cli::try_parse_from(["filigrio", "project", "flush"]).unwrap();
        assert!(matches!(
            cli.command,
            AppCommand::Project {
                cmd: ProjectCmd::Flush { project: None }
            }
        ));

        let cli = Cli::try_parse_from(["filigrio", "project", "flush", "myproj"]).unwrap();
        let AppCommand::Project {
            cmd: ProjectCmd::Flush { ref project },
        } = cli.command
        else {
            panic!("expected project flush");
        };
        assert_eq!(project.as_deref(), Some("myproj"));

        assert!(
            Cli::try_parse_from(["filigrio", "project", "checkpoint"]).is_err(),
            "`checkpoint` is Phase 3's word for commit-keyed publication — it must not parse as flush"
        );
    }

    /// ADR-0042 F4 — the `Flushed` outcome renders both polarities, and the
    /// idempotent one must read as success, never as a failure.
    #[test]
    fn test_flush_outcome_rendering() {
        use filigrio_protocol::CommandOutcome;

        let wrote = describe_outcome(&CommandOutcome::Flushed {
            project: "proj".into(),
            wrote: true,
        });
        assert!(wrote.contains("flushed"), "got: {wrote}");

        let clean = describe_outcome(&CommandOutcome::Flushed {
            project: "proj".into(),
            wrote: false,
        });
        assert!(clean.contains("already persisted"), "got: {clean}");
    }

    #[test]
    fn test_project_export_command() {
        let cli = Cli::try_parse_from(["filigrio", "project", "export"]).unwrap();
        assert!(matches!(
            cli.command,
            AppCommand::Project {
                cmd: ProjectCmd::Export { project: None }
            }
        ));
    }

    #[test]
    fn test_no_daemon_flag() {
        let cli =
            Cli::try_parse_from(["filigrio", "--no-daemon", "graph", "query", "test"]).unwrap();

        assert!(!cli.is_daemon_mode());
    }

    /// ADR-0042 F6c — `--wait` is gone: commands execute synchronously and the
    /// response IS the outcome, so waiting is the only behavior. The flag must
    /// **fail to parse**, never be silently accepted-and-ignored.
    #[test]
    fn test_wait_flag_no_longer_parses() {
        assert!(
            Cli::try_parse_from(["filigrio", "--wait", "graph", "query", "test"]).is_err(),
            "--wait must be rejected (waiting is now the only behavior, ADR-0042 F6c)"
        );
    }

    /// ADR-0042 F6c (carried from the retired 0032g draft) — a failed command
    /// goes to **stderr** and exits **non-zero**. The pre-F6c CLI printed
    /// "Command accepted, job ID: …" and exited 0 for a command that later
    /// failed daemon-side, which is exactly how a broken CI index went green.
    #[test]
    fn test_error_response_is_a_stderr_failure_and_success_is_not() {
        use filigrio_protocol::{CommandOutcome, Response};

        let failed = render_response(Response::Error {
            message: "apply failed for proj: boom".to_string(),
        });
        let msg = failed.expect_err("an error response must be a failure (non-zero exit)");
        assert!(msg.contains("apply failed for proj: boom"), "got: {msg}");

        // A completed command is a success, and its text carries the real
        // outcome — not an ack.
        let ok = render_response(Response::CommandCompleted {
            outcome: CommandOutcome::Indexed {
                project: "proj".into(),
                changed: 3,
                vanished: 0,
            },
        })
        .expect("a completed command must exit 0");
        assert!(ok.contains("proj") && ok.contains('3'), "got: {ok}");

        // The clean-tree case must read as "up to date", never as a failure.
        let clean = render_response(Response::CommandCompleted {
            outcome: CommandOutcome::Indexed {
                project: "proj".into(),
                changed: 0,
                vanished: 0,
            },
        })
        .expect("an unchanged index is still a success");
        assert!(clean.contains("up to date"), "got: {clean}");
    }

    /// ADR-0042 F6b — the watch outcome renders both polarities, and surfaces
    /// the initial converge's result when there was one.
    #[test]
    fn test_watch_outcome_rendering() {
        use filigrio_protocol::CommandOutcome;

        let on = describe_outcome(&CommandOutcome::Watch {
            project: "proj".into(),
            watching: true,
            changed: Some(2),
            vanished: None,
            note: None,
        });
        assert!(on.contains("watching") && on.contains('2'), "got: {on}");

        let off = describe_outcome(&CommandOutcome::Watch {
            project: "proj".into(),
            watching: false,
            changed: None,
            vanished: None,
            note: Some("was not watching — nothing to stop".into()),
        });
        assert!(
            off.contains("not watching") && off.contains("nothing to stop"),
            "got: {off}"
        );
    }

    /// ADR-0042 F6b — `project watch on` / `project watch off` parse as the
    /// literal words, with the project defaulting to the current directory.
    #[test]
    fn test_project_watch_on_off_parses() {
        let cli = Cli::try_parse_from(["filigrio", "project", "watch", "on"]).unwrap();
        let AppCommand::Project {
            cmd:
                ProjectCmd::Watch {
                    ref state,
                    ref project,
                },
        } = cli.command
        else {
            panic!("expected project watch");
        };
        assert_eq!(*state, WatchState::On);
        assert!(
            project.is_none(),
            "project defaults to the current directory"
        );

        let cli = Cli::try_parse_from(["filigrio", "project", "watch", "off", "myproj"]).unwrap();
        let AppCommand::Project {
            cmd:
                ProjectCmd::Watch {
                    ref state,
                    ref project,
                },
        } = cli.command
        else {
            panic!("expected project watch");
        };
        assert_eq!(*state, WatchState::Off);
        assert_eq!(project.as_deref(), Some("myproj"));
    }

    /// ADR-0038 §1/§2b — the installer surface is **three** resource groups, and
    /// none of them is reachable as a flat verb.
    ///
    /// The flat-alias half is ADR-0038 §2's standing rule. The three-groups half
    /// is ADR-0034 §17's correction to it: a resource that exists so one command
    /// can do several unrelated things is the shape to be suspicious of.
    #[test]
    fn each_of_the_three_families_is_its_own_resource_group_with_no_flat_alias() {
        for resource in ["agent", "hooks", "completions"] {
            for verb in ["install", "uninstall", "status"] {
                assert!(
                    Cli::try_parse_from(["filigrio", resource, verb]).is_ok(),
                    "`filigrio {resource} {verb}` must parse"
                );
            }
        }
        for verb in ["install", "uninstall", "status"] {
            assert!(
                Cli::try_parse_from(["filigrio", verb]).is_err(),
                "`filigrio {verb}` must NOT parse — ADR-0038 §2 forbids flat aliases"
            );
        }
    }

    /// **The retired surface is gone from the parser, not merely unused.**
    ///
    /// ADR-0034 §17 retires the `integration` noun because `--target` and
    /// `--client` composed into a command that wrote sixteen artifacts by
    /// default. A parser that still accepted the old spelling — even mapped onto
    /// the new behaviour — would leave the muscle memory intact and the ADR's
    /// premise unenforced. A hard parse error is the only version of "retired"
    /// that a user finds out about at the moment it matters.
    #[test]
    fn the_retired_integration_noun_and_its_selectors_no_longer_parse() {
        assert!(Cli::try_parse_from(["filigrio", "integration", "install"]).is_err());
        assert!(Cli::try_parse_from(["filigrio", "integration", "status"]).is_err());
        for stale in [
            vec!["filigrio", "agent", "install", "--target", "clients"],
            vec!["filigrio", "agent", "install", "--client", "cursor"],
            vec!["filigrio", "hooks", "install", "--target", "hooks"],
            vec![
                "filigrio",
                "completions",
                "install",
                "--target",
                "completions",
            ],
        ] {
            assert!(
                Cli::try_parse_from(&stale).is_err(),
                "{stale:?} must not parse — `--target`/`--client` are retired (ADR-0034 §17)"
            );
        }
    }

    /// **There is no `--all`, and no command spans families** (ADR-0034 §17).
    ///
    /// Neither as a flag on any of the three resources, nor — since §17.2's
    /// correction — as a value of `--agent`. A command wide enough to want one is
    /// the thing this surface is shaped to make unrepresentable.
    #[test]
    fn there_is_no_all_flag_on_any_of_the_three_resources() {
        for resource in ["agent", "hooks", "completions"] {
            assert!(
                Cli::try_parse_from(["filigrio", resource, "install", "--all"]).is_err(),
                "`filigrio {resource} install --all` must not parse"
            );
        }
    }

    /// `--agent` takes a comma list or repeats, `--global` is a bare flag, and
    /// **omitting `--agent` parses to an empty list** — which the verb, not the
    /// parser, turns into the roster or into a sweep (§17.2). The parser cannot
    /// default it without making "no arguments" mean "all of them" again on the
    /// verb that writes.
    #[test]
    fn the_agent_selectors_parse_as_a_list_and_omitting_them_is_not_all() {
        let cli = Cli::try_parse_from(["filigrio", "agent", "install"]).unwrap();
        let AppCommand::Agent {
            cmd: AgentCmd::Install { ref args },
        } = cli.command
        else {
            panic!("expected agent install");
        };
        assert!(args.agents.is_empty(), "no --agent is an empty selection");
        assert!(!args.global, "--global is off unless asked for");
        assert!(args.repo.is_none(), "no --repo means the cwd");

        let cli = Cli::try_parse_from([
            "filigrio",
            "agent",
            "install",
            "--agent",
            "cursor,opencode",
            "--agent",
            "hermes",
            "--global",
            "--repo",
            "/tmp/repo",
            "--explain",
        ])
        .unwrap();
        let AppCommand::Agent {
            cmd: AgentCmd::Install { ref args },
        } = cli.command
        else {
            panic!("expected agent install");
        };
        assert_eq!(args.agents, ["cursor", "opencode", "hermes"]);
        assert!(args.global && args.explain);
        assert_eq!(args.repo.as_deref(), Some(Path::new("/tmp/repo")));
    }

    /// `hooks` and `completions` take their own member flag and **no
    /// `--global`**: completions live under `$HOME` by nature and nowhere else,
    /// so naming the command is the consent (§17.1), and the hooks are inside the
    /// repository, so there is nothing to consent to.
    #[test]
    fn hooks_and_completions_take_members_but_never_global() {
        let cli = Cli::try_parse_from([
            "filigrio",
            "hooks",
            "install",
            "--hook",
            "post-commit,post-merge",
        ])
        .unwrap();
        let AppCommand::Hooks {
            cmd: HooksCmd::Install { ref args },
        } = cli.command
        else {
            panic!("expected hooks install");
        };
        assert_eq!(args.hooks, ["post-commit", "post-merge"]);

        let cli =
            Cli::try_parse_from(["filigrio", "completions", "install", "--shell", "zsh,fish"])
                .unwrap();
        let AppCommand::Completions {
            cmd: CompletionsCmd::Install { ref args },
        } = cli.command
        else {
            panic!("expected completions install");
        };
        assert_eq!(args.shells, ["zsh", "fish"]);

        for cmd in [
            vec!["filigrio", "hooks", "install", "--global"],
            vec!["filigrio", "completions", "install", "--global"],
            vec!["filigrio", "completions", "install", "--repo", "/tmp/r"],
        ] {
            assert!(
                Cli::try_parse_from(&cmd).is_err(),
                "{cmd:?} must not parse — that flag belongs to `agent` alone"
            );
        }
    }

    /// **`docs` takes neither a scope nor a member** (ADR-0034 §18.2), and the
    /// two absences are asserted rather than assumed.
    ///
    /// `--global` not parsing is the whole scope argument made mechanical: the
    /// artifact's home is the repository root by the convention's own
    /// definition, so there is no second scope to select and therefore no gate
    /// to build. `--doc` not parsing is the member argument the same way — one
    /// artifact, so `docs install` just installs.
    #[test]
    fn docs_takes_a_project_and_an_explain_and_refuses_a_scope_or_a_member() {
        let cli = Cli::try_parse_from(["filigrio", "docs", "install"]).unwrap();
        let AppCommand::Docs {
            cmd: DocsCmd::Install { ref args },
        } = cli.command
        else {
            panic!("expected docs install");
        };
        assert!(args.repo.is_none() && !args.explain);

        let cli = Cli::try_parse_from([
            "filigrio",
            "docs",
            "status",
            "--repo",
            "/tmp/r",
            "--explain",
        ])
        .unwrap();
        let AppCommand::Docs {
            cmd: DocsCmd::Status { ref args },
        } = cli.command
        else {
            panic!("expected docs status");
        };
        assert_eq!(args.repo.as_deref(), Some(Path::new("/tmp/r")));
        assert!(args.explain);

        for cmd in [
            vec!["filigrio", "docs", "install", "--global"],
            vec!["filigrio", "docs", "install", "--doc", "agents-md"],
            vec!["filigrio", "docs", "install", "--agent", "cursor"],
        ] {
            assert!(
                Cli::try_parse_from(&cmd).is_err(),
                "{cmd:?} must not parse — `docs` has one artifact in one place"
            );
        }
    }

    /// **`agents-md` is not an agent any more** (ADR-0034 §18.2), and it fails
    /// exactly the way `all` does rather than in some gentler way of its own.
    ///
    /// The same error, listing the same seven real slugs, is the point: a name
    /// this build used to accept has to be *as unknown* as a name it never did,
    /// or `--agent agents-md` becomes a special case someone maintains. What the
    /// user needs is the seven names beside the rejection — and, one line up in
    /// the roster, the command that replaced it.
    #[test]
    fn agents_md_is_no_longer_an_agent_and_fails_like_any_other_unknown_name() {
        let msg = resolve_agents(&["agents-md".to_string()])
            .unwrap_err()
            .to_string();
        assert!(msg.contains("unknown agent `agents-md`"), "got: {msg}");
        for known in filigrio_install::ALL_CLIENTS {
            assert!(
                msg.contains(known.slug()),
                "`{}` unlisted: {msg}",
                known.slug()
            );
        }
        assert_eq!(
            filigrio_install::ALL_CLIENTS.len(),
            7,
            "seven agents, and AGENTS.md is not one of them"
        );
        assert!(
            !filigrio_install::ALL_CLIENTS
                .iter()
                .any(|c| c.slug() == "agents-md"),
            "it must be gone from the roster too, not just from the parser"
        );
    }

    /// **`--agent all` is retired, and `all` is now just a name nobody has**
    /// (ADR-0034 §17.2).
    ///
    /// Installing eight agents from one flag value is the oversupply §17 exists
    /// to prevent, one flag deeper. It is not remapped onto the roster or onto a
    /// sweep, because either would leave the muscle memory intact: it falls
    /// through to the unknown-agent error, which prints the seven real slugs —
    /// the answer the user needs.
    #[test]
    fn the_word_all_is_not_a_selector_it_is_an_unknown_agent() {
        let msg = resolve_agents(&["all".to_string()])
            .unwrap_err()
            .to_string();
        assert!(msg.contains("unknown agent `all`"), "got: {msg}");
        for known in filigrio_install::ALL_CLIENTS {
            assert!(
                msg.contains(known.slug()),
                "`{}` unlisted: {msg}",
                known.slug()
            );
        }
        assert!(resolve_agents(&["all".to_string(), "hermes".to_string()]).is_err());
    }

    /// An empty `--agent` is **not a selection**, and which of the three verbs
    /// asked is what decides what that means: `install` gets the roster and a
    /// usage error, `uninstall` and `status` get every agent (see
    /// [`super::run_agent`] for why the asymmetry is deliberate).
    #[test]
    fn no_agent_flag_is_not_a_selection_and_the_verb_decides_what_that_means() {
        assert!(resolve_agents(&[]).unwrap().is_none());
        assert_eq!(
            resolve_agents(&["cursor".to_string()]).unwrap().unwrap(),
            vec![ClientId::Cursor]
        );
    }

    /// A misspelt agent is a hard error naming the known ones, resolved **before**
    /// anything is written. A typo that silently selects the empty set writes no
    /// artifacts, reports no failures, and exits 0.
    #[test]
    fn an_unknown_agent_is_an_error_that_lists_the_known_ones() {
        let err = resolve_agents(&["claud-code".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("claud-code"), "got: {msg}");
        for known in filigrio_install::ALL_CLIENTS {
            assert!(
                msg.contains(known.slug()),
                "`{}` unlisted: {msg}",
                known.slug()
            );
        }
    }

    /// Only the two literal polarities are valid — a typo must be a parse
    /// error, never a silently-ignored "off".
    #[test]
    fn test_project_watch_rejects_other_words() {
        for word in ["true", "yes", "ON", "enable", ""] {
            assert!(
                Cli::try_parse_from(["filigrio", "project", "watch", word]).is_err(),
                "`project watch {word}` must not parse"
            );
        }
        assert!(
            Cli::try_parse_from(["filigrio", "project", "watch"]).is_err(),
            "`project watch` needs an explicit on/off"
        );
    }

    /// There is **no top-level `hook`** — the entry point exists only as
    /// `hooks run` — and that absence is a stronger guarantee than the `hide`
    /// it replaced.
    ///
    /// A hidden subcommand is hidden from `--help` and from nothing else:
    /// `clap_complete`'s generators enumerate `get_subcommands()` without
    /// consulting `is_hide_set()`, so the singular verb was still offered by
    /// tab-completion in all three shells, and `clap::Command` exposes no way to
    /// remove a subcommand once declared. A subcommand that does not exist
    /// cannot be emitted into a completion script, cannot sit one letter from
    /// `hooks` in the help, and needs no filter to keep it out of either.
    ///
    /// The `run` half is asserted alongside it because the installed
    /// `.git/hooks/*` call `filigrio hooks run <event>`: an absence that ever
    /// became an absence of *both* would break every repository with hooks
    /// installed, silently, at commit time.
    #[test]
    fn the_git_hook_entry_point_lives_under_hooks_and_has_no_top_level_twin() {
        let cli =
            Cli::try_parse_from(["filigrio", "hooks", "run", "post-commit", "HEAD~1"]).unwrap();
        let AppCommand::Hooks {
            cmd: HooksCmd::Run {
                ref event,
                ref args,
            },
        } = cli.command
        else {
            panic!("expected hooks run");
        };
        assert_eq!(event, "post-commit");
        assert_eq!(args, &["HEAD~1"]);

        let command = Cli::command();
        assert!(
            command.get_subcommands().all(|c| c.get_name() != "hook"),
            "a top-level `hook` is what the completion scripts would leak"
        );
        let hooks = command
            .get_subcommands()
            .find(|c| c.get_name() == "hooks")
            .expect("`hooks` must be a subcommand");
        assert!(
            hooks.get_subcommands().any(|c| c.get_name() == "run"),
            "the scripts call `hooks run`; losing it breaks them at commit time"
        );

        // The reader who found the string inside `.git/hooks/post-commit` gets
        // the whole explanation, which is now a listed surface rather than a
        // hidden one.
        let Err(help) = Cli::try_parse_from(["filigrio", "hooks", "run", "--help"]) else {
            panic!("`--help` is reported as an Err carrying the rendered help");
        };
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(
            help.to_string().contains("FILIGRIO_SKIP_HOOK"),
            "`hooks run --help` must still print its own documentation: {help}"
        );
    }
}
