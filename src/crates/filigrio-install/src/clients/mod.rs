//! Per-agent wiring adapters (ADR-0034 §1–§3, §18).
//!
//! One MCP substrate, N small adapters. Each adapter wires at most three things
//! — an MCP server registration pointing at the `filigrio-mcp` bridge, the
//! capability doc in that agent's own format, and the reverse of both — and
//! adding an agent is adding an adapter plus a template, never touching the
//! engine or the bridge.
//!
//! ## `--agent X` writes X's files and nobody else's (§18.1)
//!
//! | agent | writes |
//! |---|---|
//! | `claude-code` | `.mcp.json` + `.claude/skills/filigrio/SKILL.md` |
//! | `opencode` | `opencode.json` + `.opencode/skills/filigrio/SKILL.md` |
//! | `cursor`, `windsurf`, `codex`, `openclaw`, `hermes` | a registration only |
//!
//! What an install writes varies **by agent, because vendors differ — never by
//! what the user asked for**. §7 split the capability-doc writer from the
//! registration writer and called the split free; it was not, because it let one
//! adapter write a file on another adapter's behalf, and `--agent opencode`
//! ended up documented by a `.claude/` directory in repositories where nobody
//! ran Claude Code. Two agents now render the *same* `SKILL.md` template
//! ([`crate::capability::skill`]) into their *own* namespaces.
//!
//! ## A registration follows `--global`; a skill never does (§18.1)
//!
//! The two artifacts are different kinds of claim, so one scope cannot govern
//! both. See [`skill_root`], which is where the rule lives so that the next
//! adapter with a global scope *and* a skill inherits it instead of re-deriving
//! it from the registration's `match scope`.
//!
//! The five registration-only adapters read `AGENTS.md`, which is no longer one
//! of them: it is a repository artifact with a resource of its own
//! ([`crate::docs`], `filigrio docs install`).
//!
//! ## Why those five write no skill is not one answer (§18.1)
//!
//! `docs/vendor-path-verification.md` splits the row:
//!
//! | adapter | is there a repo-local skill root? | so the decline is |
//! |---|---|---|
//! | [`openclaw`] | **no** — every root is `<workspace>`- or state-dir-relative, and `<workspace>` defaults to `~/.openclaw/workspace` | a vendor fact |
//! | [`hermes`] | **no** — one root, `~/.hermes/skills/`, extended only by `external_dirs` in `~/.hermes/config.yaml` | a vendor fact |
//! | [`cursor`] | yes, `.cursor/rules/*.mdc` | our judgement (§12) |
//! | [`windsurf`] | yes, three of them: `.devin/skills/`, `.windsurf/skills/`, `.agents/skills/` | our judgement (§12) |
//! | [`codex`] | yes, `.agents/skills/` at `$CWD`, `$CWD/..` and `$REPO_ROOT` — **not** `.codex/skills/`, which OpenAI documents nowhere | our judgement (§12) |
//!
//! Each adapter carries its own reasoning, because that is the file the next
//! person opens. The three judgements rest on Cursor, Windsurf and Codex each
//! documenting `AGENTS.md` themselves, so a per-vendor copy would be a second
//! copy of one doc in one repository, free to drift from the first (§4).
//!
//! ## `.agents/skills/` has four claimants, and that decision is deferred
//!
//! Said once here rather than four times over. `.agents/skills/<name>/SKILL.md`
//! is documented by **Codex** (its *only* repository root), **OpenCode** (one of
//! six), **OpenClaw** (as `<workspace>/.agents/skills`, which is why that one is
//! not repo-local) and **Windsurf/Devin**. One file there would serve four of
//! the seven agents this crate wires, from one template with one managed
//! lifetime.
//!
//! ADR-0034 §18.1 defers it, and the reason travels: §18's rule is that
//! `--agent X` writes in X's own namespace, which is plainly right for
//! `.claude/` and `.opencode/` — one product's private directories. `.agents/`
//! is not one. It is a neutral cross-vendor convention, closer in kind to
//! `AGENTS.md` (§18.2) than to either, so the namespace rule does not reach it.
//! It is also the path likeliest to gain claimants, so a sweep that finds a
//! fifth is the signal to settle it.
//!
//! ## §3: the config paths here were verified against live documentation
//!
//! ADR-0034 §3 is binding — the exact path and format per client is a fact to
//! check at implementation, not to recall. Verified 2026-07-29 and swept again
//! on 2026-08-08; the sweep is `docs/vendor-path-verification.md`, which records
//! the URL, the date and the quote behind every line below and is re-runnable in
//! order, which this list is not. Where the two disagree the sweep is later.
//!
//! - **Claude Code MCP registration** — `code.claude.com/docs/en/mcp`. Project
//!   scope is `.mcp.json` at the project root, shape
//!   `{"mcpServers": {"<name>": {"type": "stdio", "command": …, "args": […],
//!   "env": {…}}}}`. User scope is `~/.claude.json`, which is *also* where
//!   Claude Code keeps per-project session state — so we do not write it (see
//!   [`claude_code`]).
//! - **Claude Code skill** — `code.claude.com/docs/en/skills`. A skill is a
//!   directory whose entrypoint is `SKILL.md`: `.claude/skills/<name>/SKILL.md`
//!   for a project, `~/.claude/skills/<name>/SKILL.md` for a user. Frontmatter
//!   fields are all optional; `description` is the recommended one and is what
//!   Claude uses to decide when to load the skill.
//! - **OpenCode skill** — `https://opencode.ai/docs/skills`, verified
//!   2026-08-08. Six scan directories, three project-local
//!   (`.opencode/skills/`, `.claude/skills/`, `.agents/skills/`) and three
//!   global (`~/.config/opencode/skills/`, `~/.claude/skills/`,
//!   `~/.agents/skills/`); `.opencode/skills/<name>/SKILL.md` is the first, and
//!   is where [`opencode`] writes. Unlike Claude Code, OpenCode **validates** the
//!   frontmatter — `name` matching `^[a-z0-9]+(-[a-z0-9]+)*$`, 1–64 chars and
//!   equal to the containing directory; `description` 1–1024 chars — and drops
//!   the skill silently when it does not.
//! - **`AGENTS.md`** — `agents.md`. A plain-markdown file at the repository
//!   root, no required headings and no frontmatter; nested files are allowed in
//!   a monorepo with the closest one winning. Read by Codex, Jules, Aider, Zed,
//!   VS Code, Cursor, Copilot, Warp, Junie and others. **Not an adapter**: it is
//!   [`crate::docs`], a resource of its own (§18.2).
//! - **Cursor MCP registration** — Cursor's docs. `.cursor/mcp.json` for a
//!   project, `~/.cursor/mcp.json` for the user; top-level `mcpServers`, and a
//!   local server is `{"command": …, "args": […], "env": {…}}`.
//! - **Windsurf MCP registration** — `docs.devin.ai`, re-verified 2026-08-08
//!   (`docs.windsurf.com` 307s there). `<project>/.devin/mcp_config.json` for a
//!   project and `~/.config/devin/mcp_config.json` for the user; same
//!   `mcpServers` container and same `{command, args, env}` server shape as
//!   Cursor. **Not** `~/.codeium/windsurf/mcp_config.json`, which the vendor now
//!   documents as reaching "the legacy Cascade agent only" — see [`windsurf`]
//!   for the three-page chain that establishes which agent reads which file, and
//!   for what the 2026-07-29 reading got wrong.
//! - **OpenCode MCP registration** — `https://opencode.ai/config.json`, the
//!   `$schema` OpenCode's own config declares. `opencode.json` at the project
//!   root or `~/.config/opencode/opencode.json`; top-level **`mcp`** (not
//!   `mcpServers`), and `$defs/McpLocalConfig` is
//!   `{"type": "local", "command": [bin, …args], "environment": {…}}` with
//!   `additionalProperties: false`. See [`opencode`] for why every one of those
//!   four differences is a silent failure rather than a loud one.
//! - **Codex MCP registration** — OpenAI's configuration reference,
//!   `https://learn.chatgpt.com/docs/config-file/config-reference` (a 308 from
//!   `https://developers.openai.com/codex/config-reference`; the `openai/codex`
//!   repo's `docs/config.md` is now a stub pointing there), cross-checked
//!   against a live `codex-cli 0.146.0`. `~/.codex/config.toml` for the user and
//!   `.codex/config.toml` for a project — the latter loaded **only when the
//!   project is trusted**; section `[mcp_servers.<name>]`; format **TOML**, so
//!   [`crate::toml_entry`] rather than [`crate::json_entry`]. See [`codex`] for
//!   the scope decision and for why the scriptable `codex mcp add` was tested
//!   and rejected.
//! - **OpenClaw MCP registration** — the live binary, `OpenClaw 2026.7.1-2
//!   (0790d9f)`, exercised against a scratch config via `OPENCLAW_CONFIG_PATH`.
//!   `~/.openclaw/openclaw.json`, **global scope only**, and the entry sits
//!   **two levels deep** at `mcp.servers.<name>` — every other JSON adapter
//!   nests once. `{command, args}`. See [`openclaw`] for the depth trap, the
//!   credential-mode handling, and why `openclaw mcp add` was tested and
//!   rejected too.
//! - **Hermes (Nous) MCP registration** — the live binary, `Hermes Agent
//!   v0.19.0 (2026.7.20)`, exercised against a scratch `HERMES_HOME`.
//!   `~/.hermes/config.yaml`, **global scope only**, top-level `mcp_servers:`
//!   with `{command, args}` — and it is **YAML**, the one format with no
//!   format-preserving editor in Rust, so the entry is *spliced* as a managed
//!   block ([`crate::yaml_block`]) rather than parsed and re-emitted. See
//!   [`hermes`] for that decision and for the measurement that forced its
//!   marker-independent uninstall.
//!
//! **Every client ADR-0034 named is now shipped.** The list is closed only in
//! the sense that nothing is outstanding — §3's rule stands for the next one.

pub mod claude_code;
pub mod codex;
pub mod cursor;
pub mod hermes;
pub mod openclaw;
pub mod opencode;
pub mod windsurf;

use crate::{Action, EntryState, Environment, InstallError, NoteKind, Report, STALE};
use std::path::Path;

/// The container key three of these registrations share. Claude Code, Cursor
/// and Windsurf each document `mcpServers` independently; they are not one
/// fact, but they are one string, and a typo in one of three copies is a server
/// that never appears. OpenCode's is `mcp`, Codex's and Hermes' are
/// `mcp_servers` in two different file formats, and OpenClaw's is `mcp` →
/// `servers` two levels down — near misses, each spelled out in its own
/// adapter.
pub(crate) const MCP_SERVERS: &[&str] = &["mcpServers"];

/// The arguments the `filigrio-mcp` bridge is spawned with.
///
/// One definition because this is *our* fact, not a vendor's: every adapter
/// shapes the surrounding config the way its own docs show — five in JSON, one
/// in TOML, one in YAML — but they all invoke the same binary the same way, and
/// a change to the bridge's flags must not have to be found in seven files.
pub(crate) fn bridge_args(env: &Environment) -> Vec<String> {
    vec!["--socket".into(), env.socket_path.display().to_string()]
}

/// The clients this build can wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientId {
    /// Claude Code — `.mcp.json` + a `.claude/skills/filigrio/SKILL.md`.
    ClaudeCode,
    /// Cursor — `.cursor/mcp.json`; its capability doc is `AGENTS.md`.
    Cursor,
    /// Windsurf / Devin Desktop — `.devin/mcp_config.json` (or
    /// `~/.config/devin/mcp_config.json` on `--global`); its capability doc is
    /// `AGENTS.md`.
    Windsurf,
    /// OpenCode — `opencode.json`'s `mcp` key + a
    /// `.opencode/skills/filigrio/SKILL.md` of its own (ADR-0034 §18.1).
    OpenCode,
    /// Codex — `~/.codex/config.toml`'s `[mcp_servers.<name>]` (user scope,
    /// **TOML**); its capability doc is `AGENTS.md`.
    Codex,
    /// OpenClaw — `~/.openclaw/openclaw.json`'s `mcp.servers.<name>` (user
    /// scope, two levels deep); its capability doc is `AGENTS.md`.
    OpenClaw,
    /// Hermes (Nous) — `~/.hermes/config.yaml`'s `mcp_servers.<name>` (user
    /// scope, **YAML**, spliced as a managed block); its doc is `AGENTS.md`.
    Hermes,
}

impl ClientId {
    pub fn slug(self) -> &'static str {
        match self {
            ClientId::ClaudeCode => "claude-code",
            ClientId::Cursor => "cursor",
            ClientId::Windsurf => "windsurf",
            ClientId::OpenCode => "opencode",
            ClientId::Codex => "codex",
            ClientId::OpenClaw => "openclaw",
            ClientId::Hermes => "hermes",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude-code" => Some(ClientId::ClaudeCode),
            "cursor" => Some(ClientId::Cursor),
            "windsurf" => Some(ClientId::Windsurf),
            "opencode" => Some(ClientId::OpenCode),
            "codex" => Some(ClientId::Codex),
            "openclaw" => Some(ClientId::OpenClaw),
            "hermes" => Some(ClientId::Hermes),
            _ => None,
        }
    }
}

/// Every client this build knows how to wire.
///
/// **Seven, not eight**: `agents-md` left this list with ADR-0034 §18.2, because
/// `AGENTS.md` is a repository artifact rather than an agent's. It is
/// [`crate::docs`] now, reached by `filigrio docs`, and `agents-md` is an unknown
/// `--agent` value like any other typo.
pub const ALL_CLIENTS: &[ClientId] = &[
    ClientId::ClaudeCode,
    ClientId::Cursor,
    ClientId::Windsurf,
    ClientId::OpenCode,
    ClientId::Codex,
    ClientId::OpenClaw,
    ClientId::Hermes,
];

/// Where an adapter's artifacts land (ADR-0034 §17.1).
///
/// Two values, not a path: the *path* is each adapter's own vendor fact, and
/// this is the question the CLI has to answer before it calls one — **is what
/// this run is about to write inside the repository, or outside it?**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Inside the repository being wired. Travels with the checkout.
    Project,
    /// Under `$HOME`. Machine-local, and the thing `--global` consents to.
    Global,
}

impl Scope {
    pub fn slug(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

/// The scope that is *not* an adapter's default, and what this crate will do
/// about it (ADR-0034 §17.1).
///
/// Three variants and not a `supports_global() -> bool`, because a bool collapses
/// "the vendor offers nothing here" and "the vendor offers it and we decline, for
/// this reason" into the same silent no — and reporting rather than swallowing
/// that distinction is what the rest of this crate is for. The two refusals in
/// §17.1's table are made for *opposite* reasons (`~/.claude.json` is too live to
/// touch; `.codex/config.toml` is too easily ignored to trust), and a user who
/// hits one deserves the one that applies.
///
/// Both non-`Available` variants carry their reason as a `&'static str`. §17.1
/// sketched `NotOffered` without a payload; that could not hold its own half of
/// the promise, since the sentence explaining "OpenClaw documents no
/// project-scoped file" already exists in [`openclaw`]'s `SCOPE_NOTE` and inventing
/// a second copy at the CLI is exactly the drift [`crate::NoteKind`] was built to
/// prevent. So the adapters hand over the sentence they already print.
///
/// The two `NotOffered`s — [`openclaw`] and [`hermes`] — are absences
/// established by reading every documented config root, not by failing to find
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtherScope {
    /// The vendor documents it and this crate will write it on request.
    Available,
    /// The vendor documents no such file. Nothing is being declined — there is
    /// nowhere to decline.
    NotOffered(&'static str),
    /// The vendor documents it and this crate declines, for the reason carried.
    Refused(&'static str),
}

/// The two facts an adapter declares about scope, so the CLI branches **once**
/// rather than per adapter (ADR-0034 §17.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeSupport {
    /// Where this adapter writes when it is not asked for anything else.
    pub default: Scope,
    /// What happens when the other scope is asked for.
    pub other: OtherScope,
}

impl ScopeSupport {
    /// Can this adapter be installed at `requested`, and if not, why not?
    ///
    /// The **whole** scope decision, in one place, for all seven adapters. The
    /// invariant it exists to make true — *`filigrio agent install` without
    /// `--global` cannot write outside the repository* — is a property of this
    /// function and of nothing else: a caller that passes [`Scope::Project`] gets
    /// either an adapter whose default is `Project`, or an `Err`.
    pub fn resolve(self, requested: Scope) -> Result<Scope, &'static str> {
        if requested == self.default {
            return Ok(requested);
        }
        match self.other {
            OtherScope::Available => Ok(requested),
            OtherScope::NotOffered(why) | OtherScope::Refused(why) => Err(why),
        }
    }
}

/// Whether the client appears to be present on this machine.
///
/// Only ever a *note*, never a reason to skip: the project-scoped artifacts are
/// files in the user's repository and are meaningful whether or not the client
/// happens to be installed on the machine doing the installing (a teammate's
/// checkout, CI, a container). What it must never do is stay quiet — ADR-0034's
/// honest-failure rule is "say which and why", and "your editor is not on this
/// box" is exactly the thing a user needs told.
///
/// The three adapters this argument does *not* cover are [`openclaw`] and
/// [`hermes`], whose only documented configs are user-scoped, and [`codex`],
/// whose project-scoped config is read only when the project is trusted; all
/// three write to `$HOME` anyway, for the same never-silently-skip reason, and
/// all three say so in their own output.
#[derive(Debug, Clone)]
pub enum Detection {
    Present(String),
    Absent(String),
    /// The client leaves no detectable trace; saying so beats guessing.
    Unknown(String),
}

/// One client's wiring. Three operations, the same three for every client.
///
/// Every operation takes the [`Scope`] it is to act at. It is a **parameter and
/// not a field of [`Environment`]** deliberately: the environment is a set of
/// resolved facts about the machine, and the scope is a decision the *caller*
/// made about this run. A test that builds an `Environment` and forgets the scope
/// would silently exercise the wrong destination; a test that calls
/// `install(&env, …)` and forgets the scope does not compile.
///
/// Four of the seven adapters have exactly one writable scope — three of them
/// (codex, openclaw, hermes) never read the argument at all — and the CLI never
/// hands any of them the other one, because [`ScopeSupport::resolve`] refuses
/// it first.
///
/// The scope an adapter is handed governs its **registration**. It does not
/// govern a `SKILL.md`: see [`skill_root`].
pub trait ClientInstaller {
    fn id(&self) -> ClientId;
    fn display_name(&self) -> &'static str;

    /// Where this adapter writes, and what it does when asked for the other
    /// scope. See [`ScopeSupport`].
    fn scope_support(&self) -> ScopeSupport;

    /// Is the client on this machine? Reported, never acted on.
    fn detect(&self, env: &Environment) -> Detection;

    fn install(&self, env: &Environment, scope: Scope, report: &mut Report);
    fn uninstall(&self, env: &Environment, scope: Scope, report: &mut Report);
    fn status(&self, env: &Environment, scope: Scope, report: &mut Report);
}

pub fn installer_for(id: ClientId) -> Box<dyn ClientInstaller> {
    match id {
        ClientId::ClaudeCode => Box::new(claude_code::ClaudeCode),
        ClientId::Cursor => Box::new(cursor::Cursor),
        ClientId::Windsurf => Box::new(windsurf::Windsurf),
        ClientId::OpenCode => Box::new(opencode::OpenCode),
        ClientId::Codex => Box::new(codex::Codex),
        ClientId::OpenClaw => Box::new(openclaw::OpenClaw),
        ClientId::Hermes => Box::new(hermes::Hermes),
    }
}

/// The root a rendered `SKILL.md` is written under — **always the repository**,
/// whatever scope the run requested (ADR-0034 §18.1).
///
/// A registration and a skill are different kinds of claim, and one requested
/// scope cannot honestly govern both. A registration is an **availability
/// declaration** — this server exists, here is how to reach it — and it is true
/// on the machine wherever you stand, so it follows `--global` into the vendor's
/// user-scope config quite correctly. The skill opens by asserting *"Answer
/// questions about **this codebase** by querying **its** knowledge graph"*: a
/// claim about the repository you are standing in, and false in every project
/// that has never been indexed. A copy under `$HOME` tells every future project
/// that it has a graph. Global plumbing, local claim.
///
/// §17.1's "one requested scope, resolved uniformly" was written about the
/// registration and read as though it governed every artifact an adapter writes.
/// [`opencode`] is the only agent today with both a global scope and a skill,
/// which is exactly why the over-application was invisible — so the rule lives
/// *here*, above the adapters, rather than in the one adapter that can currently
/// get it wrong. The next agent with both inherits it by calling this function.
///
/// `requested` is taken and discarded on purpose: the discarding is the rule, and
/// a call site that passes the scope it was handed reads as a decision rather
/// than as an omission.
pub(crate) fn skill_root(env: &Environment, _requested: Scope) -> &Path {
    &env.project_root
}

/// The boundary `prune_empty_dirs` must not walk past, per scope: the root this
/// scope's registration was written under — never the other one, which the run
/// has no business pruning. Shared by [`cursor`] and [`windsurf`], the two
/// adapters whose registration moves between a repository directory and `$HOME`.
pub(crate) fn prune_root(env: &Environment, scope: Scope) -> &Path {
    match scope {
        Scope::Project => &env.project_root,
        Scope::Global => &env.home,
    }
}

/// Fold one MCP registration's [`EntryState`] into a report.
///
/// Shared for the same reason [`bridge_args`] is: seven adapters, three config
/// formats, one question. "Registered" was the answer here until it was noticed
/// that it could only ever mean *the key exists* — so a user who changed
/// `--socket`, or moved the install directory, was told all seven clients were
/// fine while every one of them pointed at something that was not there. The
/// project's README worked around it in prose ("re-run `integration install` if
/// you change `--socket`"), which is how long the gap was known.
///
/// The three verdicts are worded exactly as [`crate::hooks::status`],
/// [`crate::completions::status`] and [`crate::docs::status`] word theirs. A user reading
/// one report should not have to learn that `registered` and `current` are the
/// same claim with different confidence.
pub(crate) fn report_registration(
    target: &str,
    path: &Path,
    state: Result<EntryState, InstallError>,
    report: &mut Report,
) {
    match state {
        Ok(EntryState::Absent) => report.step(target, path, Action::Absent, "not registered"),
        Ok(EntryState::Current) => report.step(target, path, Action::Present, "current"),
        Ok(EntryState::Stale) => report.step(target, path, Action::Present, STALE),
        Err(e) => report.fail(target, path, e.to_string()),
    }
}

/// Write a rendered `SKILL.md`, whole file — the file is ours, so there is no
/// managed block. Shared by [`claude_code`] and [`opencode`] for the same
/// reason [`report_registration`] is: one artifact kind, one wording, and a
/// third skill-writing adapter inherits it instead of re-typing the closure.
pub(crate) fn install_skill(
    target: &str,
    path: &Path,
    body: Result<String, InstallError>,
    report: &mut Report,
) {
    let outcome = body.and_then(|b| crate::write_if_changed(path, &b).map(|a| (a, String::new())));
    report.record(target, path, outcome);
}

/// Remove a `SKILL.md` and prune its wholly-ours directory — but only if the
/// directory is then empty, because a `reference.md` the user dropped beside
/// our file is theirs and keeps the directory alive. The prune is bounded by
/// [`skill_root`], which is also where the file was written at either scope.
pub(crate) fn remove_skill(
    env: &Environment,
    scope: Scope,
    target: &str,
    path: &Path,
    report: &mut Report,
) {
    let outcome = match crate::read_opt(path) {
        Ok(None) => Ok((Action::Absent, "no such file".to_string())),
        Ok(Some(_)) => crate::remove_file(path).map(|removal| {
            if let Some(dir) = path.parent() {
                crate::prune_empty_dirs(dir, skill_root(env, scope));
            }
            (Action::Removed, removal.detail("skill"))
        }),
        Err(e) => Err(e),
    };
    report.record(target, path, outcome);
}

/// Fold a `SKILL.md`'s status into a report, worded exactly as
/// [`report_registration`] words a registration's — absent, current, or
/// [`STALE`] — so the two artifact kinds a run prints read as one vocabulary.
pub(crate) fn report_skill_status(
    target: &str,
    path: &Path,
    current: Result<String, InstallError>,
    report: &mut Report,
) {
    match (crate::read_opt(path), current) {
        (Ok(Some(on_disk)), Ok(want)) if on_disk == want => {
            report.step(target, path, Action::Present, "current")
        }
        (Ok(Some(_)), Ok(_)) => report.step(target, path, Action::Present, STALE),
        (Ok(None), _) => report.step(target, path, Action::Absent, "not installed"),
        (Err(e), _) | (_, Err(e)) => report.fail(target, path, e.to_string()),
    }
}

/// Tighten a credential-store config *we* just created to owner-only (0600).
///
/// Shared by [`openclaw`] and [`hermes`], whose vendors both write their config
/// 0600 because it holds API credentials; our writer creates files at the umask
/// default, so the one moment this crate may narrow a mode is when the file did
/// not exist an instant ago — there is no user intent to override. An existing
/// file's mode is the user's and is never touched (that promise lives in
/// `crate::write_all`). A failure is a note, not a failed install: the
/// registration is written and correct either way, and the thing the user needs
/// is to be told.
#[cfg(unix)]
pub(crate) fn restrict_to_owner(path: &Path, vendor: &str, report: &mut Report) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        report.note(format!(
            "could not restrict {} to owner-only (0600): {e} — {vendor} keeps credentials in \
             that file, so check its mode yourself",
            path.display()
        ));
    }
}

#[cfg(not(unix))]
pub(crate) fn restrict_to_owner(_path: &Path, _vendor: &str, _report: &mut Report) {}

/// The one sentence a registration-only adapter says about its documentation
/// (ADR-0034 §18.3): *this repository's `AGENTS.md` is what documents me, and
/// it is installed by its own command.*
///
/// A **function over the vendor's own fact** rather than five constants — the
/// same argument [`bridge_args`] and [`report_registration`] are made of: the
/// five sentences differ only in one clause, and five copies of one sentence is
/// five places for one of them to rot. `reads` names *where that vendor
/// documents reading `AGENTS.md` from*, which is the one thing this crate
/// cannot derive.
pub(crate) fn registration_only_note(vendor: &str, reads: &str) -> String {
    format!(
        "{vendor} reads AGENTS.md at the repository root ({reads}); this adapter writes the MCP \
         registration only, and `filigrio docs install` writes AGENTS.md — it is the \
         repository's documentation, not this agent's"
    )
}

/// Fold a [`Detection`] into a report as a note. Shared so no adapter can
/// decide to stay silent about it.
///
/// The note is **kinded** ([`crate::NoteKind`]): seven of these fire on a plain
/// install and they are two sentences repeated seven times, so the default
/// rendering prints one line per outcome naming the clients. `id` supplies the
/// `--agent` slug the aggregate names; `name` stays the display name, because
/// the sentence itself reads better as "Claude Code" and is what `--explain`
/// prints.
///
/// Absent and Unknown carry **different kinds** rather than one. They are not
/// the same claim: `Absent` means we looked at a documented path and it was not
/// there, `Unknown` means this crate declines to probe because the only paths it
/// verified are ones it writes itself. Merging them would produce an aggregate
/// line that is false of one half whichever way it is worded.
pub(crate) fn note_detection(id: ClientId, name: &str, detection: Detection, report: &mut Report) {
    match detection {
        Detection::Present(_) => {}
        Detection::Absent(why) => report.note_kind(
            NoteKind::NotDetected,
            id.slug(),
            format!(
                "{name} was not detected on this machine ({why}); the artifacts were still \
                 written — they are inert without the client, and `filigrio agent uninstall` \
                 removes them"
            ),
        ),
        Detection::Unknown(why) => report.note_kind(
            NoteKind::PresenceUnknown,
            id.slug(),
            format!("{name} presence could not be determined ({why})"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn every_client_round_trips_through_its_slug() {
        for id in ALL_CLIENTS {
            assert_eq!(ClientId::parse(id.slug()), Some(*id));
            assert_eq!(installer_for(*id).id(), *id);
        }
        assert_eq!(ClientId::parse("nope"), None);
    }

    /// **A skill is written into the repository at either requested scope**
    /// (ADR-0034 §18.1).
    ///
    /// Asserted on the shared rule rather than only on the one adapter that can
    /// exercise it, because the failure this guards is a *future* adapter with a
    /// global scope and a skill copying the registration's `match scope` and
    /// putting a "this codebase has a knowledge graph" claim under `$HOME`. The
    /// registration's own scope resolution is untouched and lives in
    /// [`ScopeSupport::resolve`]; these are two decisions, and this is the one
    /// that has exactly one answer.
    #[test]
    fn a_skill_stays_in_the_repository_whichever_scope_the_run_requested() {
        let env = Environment {
            project_root: PathBuf::from("/repo"),
            home: PathBuf::from("/home/dev"),
            cli_bin: PathBuf::from("/opt/g/bin/filigrio"),
            bridge_bin: PathBuf::from("/opt/g/bin/filigrio-mcp"),
            socket_path: PathBuf::from("/run/filigrio.sock"),
            version: "0.0.1".into(),
        };
        for requested in [Scope::Project, Scope::Global] {
            assert_eq!(
                skill_root(&env, requested),
                Path::new("/repo"),
                "a {} install must still leave the skill in the checkout",
                requested.slug()
            );
        }
    }

    #[test]
    fn an_absent_client_is_noted_never_silently_skipped() {
        let mut r = Report::default();
        note_detection(
            ClientId::ClaudeCode,
            "Claude Code",
            Detection::Absent("no ~/.claude directory".into()),
            &mut r,
        );
        assert_eq!(r.notes.len(), 1);
        assert!(r.notes[0].text().contains("no ~/.claude directory"));
    }

    /// The two detection outcomes are two claims, so they carry two kinds — and
    /// the aggregate lines they produce are each true of every client they name.
    /// One merged `Detection` kind would have to say either "not detected" of a
    /// client nothing probed for, or "could not be determined" of one whose
    /// documented directory was checked and was absent.
    #[test]
    fn an_undetectable_client_and_an_absent_one_do_not_share_a_kind() {
        let mut r = Report::default();
        note_detection(
            ClientId::Cursor,
            "Cursor",
            Detection::Absent("no ~/.cursor".into()),
            &mut r,
        );
        note_detection(
            ClientId::Windsurf,
            "Windsurf",
            Detection::Unknown("we only know the path we write".into()),
            &mut r,
        );

        let kinds: Vec<_> = r
            .notes
            .iter()
            .filter_map(|n| match n {
                crate::Note::Kinded { kind, .. } => Some(*kind),
                crate::Note::Plain { .. } => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![NoteKind::NotDetected, NoteKind::PresenceUnknown],
            "a probe that failed and a probe that was declined are different facts"
        );
    }
}
