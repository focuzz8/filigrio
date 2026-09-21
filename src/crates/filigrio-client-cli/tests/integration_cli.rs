//! `filigrio agent` / `hooks` / `completions` — the **dispatch layer**, exercised
//! through the real binary (ADR-0034 §17; the verbs live in `main.rs`).
//!
//! `filigrio-install` has ~200 unit tests proving each *artifact* is written and
//! removed correctly. None of them can see the layer above: which adapters an
//! `--agent` selects, whether `--global` is the gate §17.1 says it is, whether an
//! unknown name is a hard error, and whether a failed artifact reaches the
//! **process exit status**. That is where a wiring bug lives — an adapter dropped
//! from the `for` loop, a scope check that stopped being consulted, a
//! `report.is_ok()` that no longer reaches `$?` — and every one of those is
//! invisible to a library test and invisible to a code read that already believes
//! the wiring.
//!
//! So these run the built binary end to end, over a seeded repository and a
//! seeded `$HOME`, and assert on what is on disk afterwards.
//!
//! ## What the retirement of `integration` changed here
//!
//! This file was written against `filigrio integration <verb> --target … --client
//! …`. Every property it pinned still matters and is still pinned; what moved is
//! that **family confinement is no longer a flag to test but a command boundary**
//! — `agent install` cannot write a git hook because no code path from that verb
//! reaches `hooks::install`, which is a stronger claim than the old
//! `--target clients` made and is asserted the same way: by the file that must
//! not appear.
//!
//! And one property is new, because §17.1 invented it:
//! [`without_global_nothing_is_written_outside_the_repository`] — the sentence the
//! whole scope mechanism exists to make true.
//!
//! ## The hard requirement: the developer's real `$HOME` is not a test fixture
//!
//! `agent install --global` writes to `~/.codex/config.toml`,
//! `~/.hermes/config.yaml`, `~/.openclaw/openclaw.json`,
//! `~/.config/devin/mcp_config.json`, `~/.cursor/mcp.json` and
//! `~/.config/opencode/opencode.json`; `completions install` writes three files
//! under XDG paths. Every one of those is a real file on a real developer's
//! machine holding real credentials.
//!
//! `HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME` and `XDG_CACHE_HOME` are therefore
//! set on the **child process** ([`Command::env`]) and never with
//! `std::env::set_var`, which is process-global and would race every other test
//! in this binary. `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` are pinned at
//! `/dev/null` for the same reason one layer down: `hooks::hooks_dir` asks
//! `git rev-parse --git-path hooks`, and a developer with `core.hooksPath` in
//! their global config would otherwise have the hooks written outside the
//! sandbox entirely.
//!
//! That is not a theoretical hazard.
//! [`every_path_a_run_touches_lies_inside_the_sandbox`] exists because an
//! unsandboxed run during the authoring of this file wrote a `filigrio` entry
//! into the author's own `~/.codex`, `~/.hermes` and `~/.openclaw` — recovered
//! only because `uninstall` is genuinely byte-exact. It is the first test in the
//! file on purpose: if it fails, no other result here means anything.
//!
//! Not duplicated here, because `tests/hook_cli_contract.rs` already holds it:
//! `hooks status` and the daemon/hook-target probe.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// The sandbox: one temp tree that is a repository, a home, and an XDG root.
// ---------------------------------------------------------------------------

struct Sandbox {
    _dir: TempDir,
    /// The canonical root every artifact must stay under.
    root: PathBuf,
    /// `$HOME` for the child.
    home: PathBuf,
    /// The repository being wired.
    repo: PathBuf,
    xdg_data: PathBuf,
    xdg_config: PathBuf,
    xdg_cache: PathBuf,
    /// A socket path with nothing listening on it — no verb here needs a daemon.
    dead_socket: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonicalised once: `--project` is canonicalised by the CLI, so a
        // `$TMPDIR` behind a symlink would otherwise make every reported path
        // fail a `starts_with(root)` check for the wrong reason.
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize tempdir");
        let s = Sandbox {
            home: root.join("home"),
            repo: root.join("repo"),
            xdg_data: root.join("home/.xdg-data"),
            xdg_config: root.join("home/.xdg-config"),
            xdg_cache: root.join("home/.xdg-cache"),
            dead_socket: root.join("no-daemon.sock"),
            root,
            _dir: dir,
        };
        std::fs::create_dir_all(&s.home).expect("mkdir home");
        std::fs::create_dir_all(&s.repo).expect("mkdir repo");
        s.git(&["init", "-q", "-b", "main", "."]);
        s.git(&["config", "user.email", "integration@test"]);
        s.git(&["config", "user.name", "Integration Test"]);
        s
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?}:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `filigrio --socket <dead> <resource> <args…>`, with every
    /// destination-deciding variable pinned inside the sandbox.
    ///
    /// `--project` is appended for the three resources that take it — and the cwd
    /// is inside the repo as well, which is belt and braces against a future code
    /// path that reads the working directory instead. `completions` takes no
    /// `--project` (ADR-0034 §17: a completion is a property of the user's shell,
    /// not of a repository), so passing one would be a parse error.
    fn run(&self, socket: &Path, resource: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_filigrio"));
        cmd.arg("--socket").arg(socket).arg(resource).args(args);
        if resource != "completions" {
            cmd.arg("--repo").arg(&self.repo);
        }
        cmd.current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.xdg_data)
            .env("XDG_CONFIG_HOME", &self.xdg_config)
            .env("XDG_CACHE_HOME", &self.xdg_cache)
            // See the module docs: a global `core.hooksPath` would move the
            // hook destination out of the sandbox.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap_or_else(|e| panic!("run filigrio {resource} {args:?}: {e}"))
    }

    fn agent(&self, args: &[&str]) -> Output {
        self.run(&self.dead_socket.clone(), "agent", args)
    }

    fn docs(&self, args: &[&str]) -> Output {
        self.run(&self.dead_socket.clone(), "docs", args)
    }

    fn hooks(&self, args: &[&str]) -> Output {
        self.run(&self.dead_socket.clone(), "hooks", args)
    }

    fn completions(&self, args: &[&str]) -> Output {
        self.run(&self.dead_socket.clone(), "completions", args)
    }

    /// The same run with a caller-chosen `--socket`. The flag is written into
    /// every MCP registration and into all four hook scripts, so it is how a run
    /// and the run before it can legitimately disagree about what "installed"
    /// means.
    fn agent_with_socket(&self, socket: &Path, args: &[&str]) -> Output {
        self.run(socket, "agent", args)
    }

    /// The one recipe that wires **all seven adapters exactly once**: the four
    /// whose default scope is this repository, then the three that live only
    /// under `$HOME`. Two commands, and the second is the one that needs consent
    /// — which is §17's "what this costs", made executable.
    ///
    /// `windsurf` moved from the second list to the first on 2026-08-08: the
    /// vendor documents `.devin/mcp_config.json` as a project file committed to
    /// version control, and the adapter's claim that no such file existed was
    /// ours rather than theirs.
    ///
    /// The two lists are also the only way to reach every agent now that
    /// `--agent all` is retired (ADR-0034 §17.2), and splitting them by scope is
    /// what a fixture wants anyway: a single sweep would install `cursor` and
    /// `opencode` at both scopes, so a bug that wrote the wrong scope would still
    /// find a file where it looked.
    ///
    /// **`agents-md` is not in either list**, because it is not an agent
    /// (ADR-0034 §18.2). `AGENTS.md` is the `docs` resource, and
    /// [`Sandbox::install_everything`] runs its command as a fourth line —
    /// which is the shape §18.2 argues for, visible in a fixture: wiring your
    /// agents and deciding that this repository carries agent documentation are
    /// two decisions, and they are two commands.
    const PROJECT_AGENTS: &'static str = "claude-code,cursor,opencode,windsurf";
    const GLOBAL_AGENTS: &'static str = "codex,openclaw,hermes";
    /// Every agent in one `--agent` value, for the tests whose subject is
    /// *breadth* rather than scope. Not a sweep — seven names, spelled out, which
    /// is the only way this command can be made wide.
    const EVERY_AGENT: &'static str = "claude-code,cursor,opencode,windsurf,codex,openclaw,hermes";

    fn install_everything(&self) {
        for out in [
            self.agent(&["install", "--agent", Self::PROJECT_AGENTS]),
            self.agent(&["install", "--agent", Self::GLOBAL_AGENTS, "--global"]),
            self.docs(&["install"]),
            self.hooks(&["install"]),
            self.completions(&["install"]),
        ] {
            assert!(
                out.status.success(),
                "a fixture install failed:\n{}{}",
                stdout(&out),
                stderr(&out)
            );
        }
    }

    fn repo_path(&self, rel: &str) -> PathBuf {
        self.repo.join(rel)
    }

    fn home_path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    /// The three completion destinations, in the order the report prints them.
    fn completion_files(&self) -> [PathBuf; 3] {
        [
            self.xdg_data.join("bash-completion/completions/filigrio"),
            self.xdg_data.join("zsh/site-functions/_filigrio"),
            self.xdg_config.join("fish/completions/filigrio.fish"),
        ]
    }

    fn seed(&self, rel_to: &Path, body: &str) {
        if let Some(parent) = rel_to.parent() {
            std::fs::create_dir_all(parent).expect("mkdir seed parent");
        }
        std::fs::write(rel_to, body).unwrap_or_else(|e| panic!("seed {}: {e}", rel_to.display()));
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// One line of `Report::render` as this file needs it: the action symbol, the
/// target name, the path, and the parenthesised detail.
///
/// The detail is not decoration. `current` and `stale — re-run install to
/// refresh` are both `Action::Present` and therefore both render `=`, so the
/// difference between a registration that works and one pointing at a socket
/// that moved lives *only* in this field.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    symbol: String,
    target: String,
    path: String,
    detail: String,
}

/// Parse the artifact lines out of a run's stdout.
///
/// Deliberately tolerant: `note:` lines, the banner, the daemon probe, the
/// roster and a failure's indented reason all fail the symbol test and are
/// dropped. The alternative — asserting on whole formatted strings — would make
/// every one of these tests a change-detector for `Report::render`'s column
/// widths.
fn lines_of(text: &str) -> Vec<Line> {
    // Every symbol `Action::symbol` can print, plus the `!` a failure gets. A
    // symbol missing from this list does not fail a test — it makes the line
    // *disappear*, which is how a test asserting "the run wrote five artifacts"
    // would go on passing while one of them reported something new.
    const SYMBOLS: &[&str] = &["+", "~", "=", "-", ".", "?", "!"];
    text.lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("  ")?.split_whitespace();
            let symbol = fields.next()?;
            if !SYMBOLS.contains(&symbol) {
                return None;
            }
            Some(Line {
                symbol: symbol.to_string(),
                target: fields.next()?.to_string(),
                path: fields.next()?.to_string(),
                detail: line
                    .rsplit_once("  (")
                    .and_then(|(_, tail)| tail.trim_end().strip_suffix(')'))
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

fn targets(text: &str) -> Vec<String> {
    lines_of(text).into_iter().map(|l| l.target).collect()
}

/// The `note:` lines of a run, in order and with the prefix stripped.
///
/// The counterpart to [`lines_of`], which drops them: the note block is the one
/// part of the report whose *density* is a decision (`--explain`), so it needs a
/// parser of its own rather than being filtered out.
fn notes_of(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim_start().strip_prefix("note: "))
        .map(str::to_string)
        .collect()
}

/// The parenthesised verdict `status` printed for one artifact.
fn detail_of(text: &str, target: &str) -> String {
    lines_of(text)
        .into_iter()
        .find(|l| l.target == target)
        .unwrap_or_else(|| panic!("no `{target}` line in:\n{text}"))
        .detail
}

/// Every MCP registration, one per adapter — five JSON files, one TOML and one
/// YAML. Named here because the property below has to hold for all seven or it
/// holds for none: the socket is one fact written into three config formats.
const MCP_TARGETS: &[&str] = &[
    "claude-code/mcp",
    "cursor/mcp",
    "windsurf/mcp",
    "opencode/mcp",
    "codex/mcp",
    "openclaw/mcp",
    "hermes/mcp",
];

// ---------------------------------------------------------------------------
// 0. Isolation. Nothing below this line is trustworthy without it.
// ---------------------------------------------------------------------------

/// **Every path any verb reports lies inside the sandbox.**
///
/// This is the guard on the whole file. A run with `--global` writes six client
/// configs under `$HOME` and three completion files under XDG paths, so a test
/// binary that got the environment wrong would silently edit the developer's real
/// Codex, Hermes and OpenClaw configs. That happened once, during the authoring
/// of this file, from a single run with an inherited `$HOME`.
///
/// The report's path column is the honest witness: it is what the installer
/// actually resolved, not what this test assumed it would.
#[test]
fn every_path_a_run_touches_lies_inside_the_sandbox() {
    let s = Sandbox::new();

    for verb in ["install", "status", "uninstall"] {
        let runs = [
            (
                "agent",
                s.agent(&["install", "--agent", Sandbox::EVERY_AGENT]),
            ),
            (
                "agent --global",
                s.agent(&[verb, "--agent", Sandbox::EVERY_AGENT, "--global"]),
            ),
            ("docs", s.docs(&[verb])),
            ("hooks", s.hooks(&[verb])),
            ("completions", s.completions(&[verb])),
        ];
        for (what, out) in runs {
            let text = stdout(&out);
            let lines = lines_of(&text);
            assert!(
                !lines.is_empty(),
                "`{what} {verb}` reported no artifacts at all — the parser or the run is \
                 wrong:\n{text}"
            );
            for line in lines {
                assert!(
                    Path::new(&line.path).starts_with(&s.root),
                    "`{what} {verb}` touched {} for {}, which is outside the sandbox {}",
                    line.path,
                    line.target,
                    s.root.display()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 1. §17.1 — `--global` is a consent gate, and this is the sentence it buys.
// ---------------------------------------------------------------------------

/// **`filigrio agent install` without `--global` cannot write outside the
/// repository.**
///
/// The invariant ADR-0034 §17.1 exists to establish, asserted the only way that
/// means anything: **by walking the filesystem**, not by reading the report and
/// not by trusting the adapters. An adapter that ignored its own
/// `ScopeSupport` — or a gate that resolved the wrong way round — would still
/// print a plausible report; the only witness is the file that appeared under
/// `$HOME`.
///
/// Asserted at the command's **widest**: all eight agents named at once, plus a
/// bare `uninstall`, which since §17.2's correction is the one selection the CLI
/// still makes for itself. `$HOME` is snapshotted before and after because
/// "nothing new" is the claim, and an empty `$HOME` would make it vacuous — the
/// sandbox seeds one.
#[test]
fn without_global_nothing_is_written_outside_the_repository() {
    let s = Sandbox::new();
    // A `$HOME` that is already in use, so "unchanged" is a real comparison
    // rather than "still empty".
    s.seed(&s.home_path(".codex/config.toml"), "model = \"o3\"\n");
    s.seed(&s.home_path(".bashrc"), "# mine\n");
    let before = snapshot(&s.home);

    let mut runs: Vec<(String, Output)> = ["install", "uninstall", "status"]
        .iter()
        .map(|verb| {
            (
                format!("{verb} --agent <all eight>"),
                s.agent(&[verb, "--agent", Sandbox::EVERY_AGENT]),
            )
        })
        .collect();
    // The CLI's own widest selection: a bare `uninstall` sweeps, and the gate has
    // to hold for the selection the user did not spell out just as it does for
    // the one they did.
    runs.push(("uninstall (bare, a sweep)".into(), s.agent(&["uninstall"])));

    for (verb, out) in &runs {
        let after = snapshot(&s.home);
        // Reported as *paths*, not as the two byte-vectors: the whole value of
        // this test is the name of the file that should not be there, and a
        // 300-element `assertion left == right` dump buries it.
        let mut changed: Vec<String> = after
            .iter()
            .filter(|e| !before.contains(e))
            .map(|(path, _)| format!("appeared or changed: {}", path.display()))
            .collect();
        changed.extend(
            before
                .iter()
                .filter(|e| !after.contains(e))
                .map(|(path, _)| format!("vanished or changed: {}", path.display())),
        );
        assert!(
            changed.is_empty(),
            "`agent {verb}` without --global touched $HOME:\n  {}\n\n{}",
            changed.join("\n  "),
            stdout(out)
        );
    }

    // …and the report agrees, which is the weaker check the strong one exists to
    // make unnecessary — kept because a disagreement between the two would mean
    // the report is lying about where it wrote.
    let text = stdout(&s.agent(&["install", "--agent", Sandbox::PROJECT_AGENTS]));
    for line in lines_of(&text) {
        assert!(
            Path::new(&line.path).starts_with(&s.repo),
            "`{}` reported a path outside the repository: {}",
            line.target,
            line.path
        );
    }
}

/// Every regular file under `dir`, with its bytes. The comparison unit for the
/// invariant above.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

/// **The four `ScopeSupport` positions, one test, both refusals in their own
/// words** (ADR-0034 §17.1).
///
/// A `supports_global() -> bool` would satisfy the first and third rows and give
/// the same silent no to the second and fourth. The two refusals are made for
/// *opposite* reasons — `~/.claude.json` is too live to touch, `.codex/config.toml`
/// is too easily ignored to trust — and a user who hits one deserves the one that
/// applies. So the assertion is on the *sentence*, matched against the adapter's
/// own constant rather than against a phrase retyped here.
#[test]
fn each_scope_position_installs_or_refuses_in_the_adapters_own_words() {
    let s = Sandbox::new();

    // (1) project default, other Available — both scopes work, at two paths.
    assert!(s.agent(&["install", "--agent", "cursor"]).status.success());
    assert!(s.repo_path(".cursor/mcp.json").exists());
    assert!(!s.home_path(".cursor/mcp.json").exists());
    assert!(s
        .agent(&["install", "--agent", "cursor", "--global"])
        .status
        .success());
    assert!(
        s.home_path(".cursor/mcp.json").exists(),
        "`--global` must reach the user-scoped file Cursor's docs name"
    );
    // The second adapter in this position, whose global path is the one §17
    // required verifying rather than guessing.
    assert!(s
        .agent(&["install", "--agent", "opencode", "--global"])
        .status
        .success());
    assert!(
        s.home_path(".config/opencode/opencode.json").exists(),
        "opencode's user-scope path is ~/.config/opencode/opencode.json"
    );
    // The third, and the one that arrived here by correction rather than by
    // design: `windsurf` was in position (3) — global default, other
    // `NotOffered` — until the 2026-08-08 sweep found the vendor documenting
    // `.devin/mcp_config.json` as committed to version control. Both paths are
    // asserted because a half-applied correction (project scope offered, global
    // still writing the legacy Cascade file) would satisfy neither.
    assert!(s
        .agent(&["install", "--agent", "windsurf"])
        .status
        .success());
    assert!(s.repo_path(".devin/mcp_config.json").exists());
    assert!(s
        .agent(&["install", "--agent", "windsurf", "--global"])
        .status
        .success());
    assert!(
        s.home_path(".config/devin/mcp_config.json").exists(),
        "windsurf's user-scope path is the Devin CLI one the default agent reads"
    );
    assert!(
        !s.home_path(".codeium").exists(),
        "~/.codeium/windsurf/mcp_config.json applies to the legacy Cascade agent only"
    );

    // (2) project default, other Refused — §9's reason, quoted.
    let out = s.agent(&["install", "--agent", "claude-code", "--global"]);
    assert!(!out.status.success(), "a refusal must fail the run");
    let text = format!("{}{}", stdout(&out), stderr(&out));
    for fragment in [
        "~/.claude.json",
        "live per-project session state",
        "drop --global",
    ] {
        assert!(
            text.contains(fragment),
            "the refusal must carry §9's reason, not a generic no — missing {fragment:?}:\n{text}"
        );
    }
    assert!(
        !s.home_path(".claude.json").exists(),
        "a refused write must not have happened first"
    );

    // (3) global default, other NotOffered — nothing is being declined, because
    // there is nowhere to decline. `openclaw` stands here now; `windsurf` held
    // this row on a claim its own vendor contradicts, and OpenClaw's absence is
    // one every documented config root was read to establish.
    let out = s.agent(&["install", "--agent", "openclaw"]);
    assert!(!out.status.success());
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("documents no project-scoped file") && text.contains("`--global`"),
        "a NotOffered refusal must say the vendor offers nothing and name the flag:\n{text}"
    );
    assert!(s
        .agent(&["install", "--agent", "openclaw", "--global"])
        .status
        .success());
    assert!(s.home_path(".openclaw/openclaw.json").exists());

    // (4) global default, other Refused — the *other* refusal, and it must not
    // read like (3)'s. Codex's project file exists; it is the trust rule that
    // makes writing there a registration that can sit on disk being ignored.
    let out = s.agent(&["install", "--agent", "codex"]);
    assert!(!out.status.success());
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("trusted the project") && text.contains("being ignored"),
        "the codex refusal must be §13's reason, not openclaw's:\n{text}"
    );
    assert!(
        !text.contains("documents no project-scoped file"),
        "two refusals made for opposite reasons must not print the same sentence:\n{text}"
    );
    assert!(!s.repo_path(".codex/config.toml").exists());
}

/// **Every agent is named, so a scope mismatch is always a failure** (ADR-0034
/// §17.2, as corrected).
///
/// The earlier rule had two arms — a named agent failed, a swept one was noted —
/// and the second arm existed only because `--agent all` did. With `install`
/// unable to select anything it was not told, every refusal is a request that was
/// not honoured, and a request that is not honoured must reach `$?`. There is no
/// "note instead" left to get wrong.
#[test]
fn an_install_refused_by_scope_always_fails_because_every_agent_was_named() {
    let s = Sandbox::new();

    for (agents, global, expected) in [
        ("hermes", false, vec!["hermes/scope"]),
        // Two at once, so "a refusal is per agent" is exercised rather than
        // assumed — and two adapters that refuse for *different* reasons
        // (`NotOffered` vs `Refused`) both reaching the failure block.
        ("hermes,codex", false, vec!["hermes/scope", "codex/scope"]),
        ("claude-code", true, vec!["claude-code/scope"]),
    ] {
        let mut argv = vec!["install", "--agent", agents];
        if global {
            argv.push("--global");
        }
        let out = s.agent(&argv);
        assert_eq!(
            out.status.code(),
            Some(1),
            "`--agent {agents}` at the wrong scope must fail, not shrug:\n{}{}",
            stdout(&out),
            stderr(&out)
        );
        let text = stdout(&out);
        let failures: Vec<String> = lines_of(&text)
            .into_iter()
            .filter(|l| l.symbol == "!")
            .map(|l| l.target)
            .collect();
        assert_eq!(failures, expected, "wrong failure lines:\n{text}");
        assert!(
            notes_of(&text).is_empty(),
            "a named refusal is a failure, not a failure *and* a note:\n{text}"
        );
    }

    // Naming all seven at once is still seven named agents, not a sweep: the four
    // that cannot be installed here fail the run, and the three that can are
    // installed anyway (a refusal is per artifact, never an abort).
    let out = s.agent(&["install", "--agent", Sandbox::EVERY_AGENT]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        s.repo_path(".mcp.json").exists()
            && s.repo_path(".opencode/skills/filigrio/SKILL.md").exists()
    );
    assert!(
        !s.repo_path("AGENTS.md").exists(),
        "`agent install` never writes AGENTS.md — that is `docs install` (ADR-0034 §18.2)"
    );
}

/// **A bare `uninstall` sweeps, and says what it could not reach** (ADR-0034
/// §17.2).
///
/// The asymmetry with `install` is deliberate — generous on removal, conservative
/// on creation — but `--global` still gates *reach*, so a project-scoped sweep
/// leaves everything under `$HOME` exactly where it was. Saying nothing would
/// make "remove everything" a claim the command does not honour, and the user
/// would find the leftovers the way people always find leftovers: when something
/// they uninstalled keeps spawning a binary that is gone.
///
/// So: one note, counting what is **actually still registered** rather than
/// asserting that anything is, naming the agents and naming the flag.
#[test]
fn a_bare_uninstall_sweeps_the_repository_and_reports_what_it_left_under_home() {
    let s = Sandbox::new();
    s.install_everything();

    let out = s.agent(&["uninstall"]);
    assert!(
        out.status.success(),
        "a sweep must not be failed by the agents it deliberately left out:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(
        !lines_of(&text).iter().any(|l| l.symbol == "!"),
        "no failures in a sweep:\n{text}"
    );

    // The repository's *agent* artifacts are gone…
    for p in [
        s.repo_path(".mcp.json"),
        s.repo_path(".claude/skills/filigrio/SKILL.md"),
        s.repo_path(".cursor/mcp.json"),
        s.repo_path("opencode.json"),
        s.repo_path(".opencode/skills/filigrio/SKILL.md"),
        s.repo_path(".devin/mcp_config.json"),
    ] {
        assert!(!p.exists(), "the sweep left {} behind", p.display());
    }
    // …and `AGENTS.md` is **not** one of them (ADR-0034 §18.2). A sweep over
    // agents removes agent artifacts; the repository's own documentation is a
    // separate decision and survives until its own command removes it. This is
    // the property the old model could not have: `agent uninstall` used to take
    // the user's documentation with it because `agents-md` was in the roster.
    assert!(
        s.repo_path("AGENTS.md").exists(),
        "a sweep over agents removed the repository's own documentation"
    );
    assert!(s.docs(&["uninstall"]).status.success());
    assert!(
        !s.repo_path("AGENTS.md").exists(),
        "`docs uninstall` is what removes it"
    );
    // …and every $HOME registration is untouched, which is what the note is about.
    let left = [
        s.home_path(".codex/config.toml"),
        s.home_path(".openclaw/openclaw.json"),
        s.home_path(".hermes/config.yaml"),
    ];
    for p in &left {
        assert!(p.exists(), "a project-scoped sweep removed {}", p.display());
    }

    let note = notes_of(&text)
        .into_iter()
        .find(|n| n.contains("did not touch them"))
        .unwrap_or_else(|| panic!("a sweep must report what it left:\n{text}"));
    for slug in ["codex", "openclaw", "hermes"] {
        assert!(note.contains(slug), "`{slug}` unnamed in: {note}");
    }
    assert!(
        !note.contains("windsurf"),
        "windsurf's default scope is this repository now, so the sweep did reach it: {note}"
    );
    assert!(
        note.contains("3 of 3") && note.contains("--global"),
        "the note must count what is still there and name the flag that removes it: {note}"
    );

    // And the flag it names finishes the job — including the count going to zero
    // rather than the note repeating itself.
    let out = s.agent(&["uninstall", "--global"]);
    assert!(out.status.success(), "{}", stderr(&out));
    for p in &left {
        assert!(!p.exists(), "`--global` did not remove {}", p.display());
    }
    let text = stdout(&s.agent(&["uninstall"]));
    let note = notes_of(&text)
        .into_iter()
        .find(|n| n.contains("did not touch them"))
        .unwrap_or_else(|| panic!("the note is unconditional:\n{text}"));
    assert!(
        note.contains("None of them has anything installed to remove"),
        "with nothing left, the note must say so rather than repeat a count: {note}"
    );
}

// ---------------------------------------------------------------------------
// 2. §17.2 — a bare `install` teaches instead of guessing.
// ---------------------------------------------------------------------------

/// **A bare `agent install` writes nothing and exits non-zero**, printing the
/// roster and the invocation to use (ADR-0034 §17.2).
///
/// The previous reading of "no arguments" as "all of them" is what put sixteen
/// artifacts on disk. Exiting non-zero rather than zero is deliberate and is the
/// half that cannot be seen from the output: a CI job that runs the bare command
/// must fail loudly rather than appear to succeed while doing nothing.
///
/// **`install` is the only gated verb**, and the asymmetry is the decision:
/// `uninstall` and `status` read an empty selection as every agent. A sweep that
/// removes can only remove what we wrote; a sweep that creates cannot be undone
/// by the user not having asked. The oracle has the same shape —
/// `_project_uninstall_all` exists and install-all does not.
#[test]
fn a_bare_install_prints_the_roster_writes_nothing_and_exits_non_zero() {
    let s = Sandbox::new();

    let out = s.agent(&["install"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a bare `agent install` must exit as a usage error, not 0 and not 1:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let text = stdout(&out);
    for slug in [
        "claude-code",
        "cursor",
        "windsurf",
        "opencode",
        "codex",
        "openclaw",
        "hermes",
    ] {
        assert!(
            text.contains(slug),
            "the roster must list every agent — `{slug}` missing:\n{text}"
        );
    }
    // The roster is where a user learns that most of these get a registration
    // and no manual, so it is where the other command has to be named
    // (ADR-0034 §18.2). Without this line, a `+` beside `cursor/mcp` reads as
    // "Cursor is fully wired".
    assert!(
        text.contains("filigrio docs install"),
        "the roster must name the command that documents the registration-only \
         agents:\n{text}"
    );
    assert!(
        text.contains("detected") && text.contains("installed"),
        "the roster must say what was detected and what is already installed:\n{text}"
    );
    assert!(
        text.contains("filigrio agent install --agent claude-code"),
        "the roster must print the invocation to use:\n{text}"
    );
    assert!(
        !text.contains("--agent all"),
        "`--agent all` is retired; the roster must not teach it:\n{text}"
    );

    // Nothing on disk, in the repository or under $HOME.
    assert!(
        snapshot(&s.home).is_empty(),
        "a bare install wrote something under $HOME"
    );
    for p in [
        s.repo_path(".mcp.json"),
        s.repo_path("AGENTS.md"),
        s.repo_path(".cursor/mcp.json"),
        s.repo_path("opencode.json"),
    ] {
        assert!(!p.exists(), "a bare install wrote {}", p.display());
    }

    // The two exemptions, and they are real ones: both cover every agent.
    for verb in ["status", "uninstall"] {
        let out = s.agent(&[verb]);
        assert!(out.status.success(), "bare `{verb}`: {}", stderr(&out));
        let covered = targets(&stdout(&out));
        // One artifact from each end of the project-scoped list — a sweep is
        // still gated by `--global`, so the four `$HOME` agents are reported as
        // an exclusion note rather than as steps (§17.2), which the assertion
        // below covers.
        for target in ["claude-code/mcp", "cursor/mcp", "opencode/skill"] {
            assert!(
                covered.contains(&target.to_string()),
                "a bare `agent {verb}` covers every agent — `{target}` missing:\n{}",
                stdout(&out)
            );
        }
        assert!(
            notes_of(&stdout(&out))
                .iter()
                .any(|n| n.contains("did not touch them") && n.contains("hermes")),
            "a bare `agent {verb}` must still account for the agents this scope \
             cannot reach:\n{}",
            stdout(&out)
        );
        assert!(
            !covered.iter().any(|t| t.starts_with("docs/")),
            "…and covers only agents: `docs` is a resource of its own:\n{}",
            stdout(&out)
        );
    }
}

/// A misspelled `--agent` is a **hard error before anything is written**, and the
/// message names both the bad name and the good ones.
///
/// The failure this prevents is specific: "I asked for `claud-code` and nothing
/// happened." A typo that silently selects the empty set writes no artifacts,
/// reports no failures, and exits 0 — so the tool looks broken later, somewhere
/// else, for a reason nobody can trace back to a misspelling.
#[test]
fn an_unknown_agent_is_a_hard_error_that_names_the_known_ones() {
    let s = Sandbox::new();
    let out = s.agent(&["install", "--agent", "claud-code"]);

    assert!(
        !out.status.success(),
        "a typo'd agent exited 0:\n{}",
        stdout(&out)
    );
    let message = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        message.contains("claud-code"),
        "the message must quote what was asked for: {message}"
    );
    for known in [
        "claude-code",
        "cursor",
        "windsurf",
        "opencode",
        "codex",
        "openclaw",
        "hermes",
    ] {
        assert!(
            message.contains(known),
            "the message must list `{known}` among the known agents: {message}"
        );
    }

    // Rejected before the loop, so nothing was written on the way to the error.
    for p in [
        s.repo_path(".mcp.json"),
        s.repo_path("AGENTS.md"),
        s.repo_path(".cursor/mcp.json"),
    ] {
        assert!(!p.exists(), "a rejected run still wrote {}", p.display());
    }

    // **`all` is one of those typos now** (ADR-0034 §17.2). Asserted at the
    // process, not only at `resolve_agents`, because the muscle memory it has to
    // defeat is a command line: a re-introduction that only the unit test could
    // see would be a re-introduction nobody notices in review.
    let out = s.agent(&["install", "--agent", "all"]);
    assert!(
        !out.status.success(),
        "`--agent all` is retired and must not install anything:\n{}",
        stdout(&out)
    );
    assert!(
        format!("{}{}", stdout(&out), stderr(&out)).contains("unknown agent `all`"),
        "it must fail as an unknown name, listing the real ones"
    );
    assert!(
        !s.repo_path("AGENTS.md").exists() && snapshot(&s.home).is_empty(),
        "`--agent all` wrote something"
    );

    // **And `agents-md` is one of those typos now** (ADR-0034 §18.2). A name
    // this build used to accept has to fail exactly as a name it never accepted
    // does, or it becomes a special case someone maintains. Asserted at the
    // process for the same reason `all` is: what has to be defeated is a
    // command line somebody already typed once.
    let out = s.agent(&["install", "--agent", "agents-md"]);
    assert!(
        !out.status.success(),
        "`--agent agents-md` is retired and must not install anything:\n{}",
        stdout(&out)
    );
    let message = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        message.contains("unknown agent `agents-md`"),
        "it must fail as an unknown name, listing the real ones: {message}"
    );
    assert!(
        !s.repo_path("AGENTS.md").exists(),
        "a rejected `--agent agents-md` wrote AGENTS.md anyway"
    );
    // …and the command that replaced it works on the same tree.
    assert!(s.docs(&["install"]).status.success());
    assert!(s.repo_path("AGENTS.md").exists());
}

/// The same rule one resource over: an unknown `--hook` or `--shell` is a hard
/// error naming the members, not an empty selection reported as success.
#[test]
fn an_unknown_hook_or_shell_is_a_hard_error_that_names_the_members() {
    let s = Sandbox::new();

    let out = s.hooks(&["install", "--hook", "post-comit"]);
    assert!(!out.status.success(), "{}", stdout(&out));
    let msg = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        msg.contains("post-comit") && msg.contains("post-commit"),
        "{msg}"
    );
    assert!(!s.repo_path(".git/hooks/post-commit").exists());

    let out = s.completions(&["install", "--shell", "zshh"]);
    assert!(!out.status.success(), "{}", stdout(&out));
    let msg = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        msg.contains("zshh") && msg.contains("zsh") && msg.contains("fish"),
        "{msg}"
    );
    for p in s.completion_files() {
        assert!(!p.exists(), "a rejected run wrote {}", p.display());
    }
}

// ---------------------------------------------------------------------------
// 3. No command spans families (ADR-0034 §17).
// ---------------------------------------------------------------------------

/// **Each command touches its own family and nothing else.**
///
/// The defect §17 retires: `integration install --client cursor` also installed
/// four git hooks and three completion files, because `--client` narrowed one
/// dimension while the family dimension stayed at its default of *all*. There is
/// no family dimension now, and this asserts it the only way that means anything
/// — by the files that must be **absent**, since a dispatch bug still prints a
/// plausible report.
#[test]
fn each_resource_writes_its_own_family_and_nothing_else() {
    // agent → no hooks, no completions.
    let s = Sandbox::new();
    let out = s.agent(&["install", "--agent", Sandbox::PROJECT_AGENTS]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(s.repo_path(".mcp.json").exists(), "{}", stdout(&out));
    for p in [
        s.repo_path(".git/hooks/post-commit"),
        s.repo_path(".git/hooks/post-rewrite"),
    ] {
        assert!(
            !p.exists(),
            "`agent install` wrote a git hook: {}",
            p.display()
        );
    }
    for p in s.completion_files() {
        assert!(
            !p.exists(),
            "`agent install` wrote a completion: {}",
            p.display()
        );
    }
    assert!(
        !s.repo_path("AGENTS.md").exists(),
        "`agent install` wrote the `docs` family's artifact (ADR-0034 §18.2)"
    );
    assert!(
        targets(&stdout(&out))
            .iter()
            .all(|t| !t.starts_with("hooks/")
                && !t.starts_with("completions/")
                && !t.starts_with("docs/")),
        "`agent install` reported another family:\n{}",
        stdout(&out)
    );

    // docs → one file, and nothing that belongs to an agent, a hook or a shell.
    let s = Sandbox::new();
    let out = s.docs(&["install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(s.repo_path("AGENTS.md").exists(), "{}", stdout(&out));
    assert_eq!(
        targets(&stdout(&out)),
        vec!["docs/agents-md".to_string()],
        "`docs install` writes exactly one artifact:\n{}",
        stdout(&out)
    );
    for p in [
        s.repo_path(".mcp.json"),
        s.repo_path(".claude/skills/filigrio/SKILL.md"),
        s.repo_path(".cursor/mcp.json"),
        s.repo_path("opencode.json"),
        s.repo_path(".git/hooks/post-commit"),
    ] {
        assert!(
            !p.exists(),
            "`docs install` wrote another family's artifact: {}",
            p.display()
        );
    }
    for p in s.completion_files() {
        assert!(!p.exists(), "`docs install` wrote a completion");
    }
    assert!(
        snapshot(&s.home).is_empty(),
        "`docs install` wrote under $HOME — it is project-scoped by definition"
    );

    // hooks → no registrations, no doc, no completions.
    let s = Sandbox::new();
    let out = s.hooks(&["install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        s.repo_path(".git/hooks/post-commit").exists(),
        "{}",
        stdout(&out)
    );
    for p in [
        s.repo_path(".mcp.json"),
        s.repo_path("AGENTS.md"),
        s.repo_path(".cursor/mcp.json"),
        s.repo_path("opencode.json"),
        s.home_path(".codex/config.toml"),
        s.home_path(".hermes/config.yaml"),
        s.home_path(".openclaw/openclaw.json"),
        s.repo_path(".devin/mcp_config.json"),
    ] {
        assert!(
            !p.exists(),
            "`hooks install` wrote a client registration: {}",
            p.display()
        );
    }
    for p in s.completion_files() {
        assert!(
            !p.exists(),
            "`hooks install` wrote a completion: {}",
            p.display()
        );
    }

    // completions → nothing in the repository at all.
    let s = Sandbox::new();
    let out = s.completions(&["install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    for p in s.completion_files() {
        assert!(
            p.exists(),
            "{} was not written:\n{}",
            p.display(),
            stdout(&out)
        );
    }
    for p in [
        s.repo_path(".mcp.json"),
        s.repo_path("AGENTS.md"),
        s.repo_path(".git/hooks/post-commit"),
    ] {
        assert!(
            !p.exists(),
            "`completions install` wrote into the repository: {}",
            p.display()
        );
    }
}

/// A member flag confines the run **within** its family, in both directions.
#[test]
fn a_member_flag_confines_the_run_to_the_members_it_names() {
    // One agent: the claude-code adapter owns two artifacts, and the second of
    // them is a `SKILL.md` rendered from the same template OpenCode's is — so a
    // selection bug that ran every adapter would still produce a `.mcp.json`
    // and still exit 0. Only the files that must be absent tell the two apart,
    // and `.opencode/` is now one of them.
    let s = Sandbox::new();
    let out = s.agent(&["install", "--agent", "claude-code"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        targets(&stdout(&out)),
        vec![
            "claude-code/mcp".to_string(),
            "claude-code/skill".to_string()
        ],
        "exactly the selected adapter's artifacts should be reported:\n{}",
        stdout(&out)
    );
    for p in [
        s.repo_path(".cursor/mcp.json"),
        s.repo_path("opencode.json"),
        s.repo_path(".opencode"),
        s.repo_path("AGENTS.md"),
    ] {
        assert!(
            !p.exists(),
            "`--agent claude-code` wrote another agent's artifact: {}",
            p.display()
        );
    }

    // One hook.
    let s = Sandbox::new();
    let out = s.hooks(&["install", "--hook", "post-commit"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        targets(&stdout(&out)),
        vec!["hooks/post-commit".to_string()]
    );
    for name in ["post-checkout", "post-merge", "post-rewrite"] {
        assert!(
            !s.repo_path(&format!(".git/hooks/{name}")).exists(),
            "`--hook post-commit` wrote {name}"
        );
    }

    // One shell — and the zsh `fpath` note must not fire for a run that wrote no
    // zsh file, which would be a confidently wrong instruction.
    let s = Sandbox::new();
    let out = s.completions(&["install", "--shell", "fish"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(targets(&stdout(&out)), vec!["completions/fish".to_string()]);
    let [bash, zsh, fish] = s.completion_files();
    assert!(fish.exists() && !bash.exists() && !zsh.exists());
    assert!(
        !notes_of(&stdout(&out)).iter().any(|n| n.contains("fpath")),
        "a fish-only run must not tell the user to edit ~/.zshrc:\n{}",
        stdout(&out)
    );
}

/// **A repeated selector is one selection, not two runs of it.**
///
/// `--hook post-commit --hook post-commit` ran the hook twice and reported
/// `✅ 2 artifact(s) handled` over one file; `--agent` doubled every note the same
/// way. Nothing was corrupted — each pass is idempotent — but the number a user
/// reads to find out what is on their disk was a count of *passes*, and the
/// second pass reports `=` where the first reported `+`, so the run also
/// disagrees with itself about what it just did.
#[test]
fn a_repeated_member_selects_it_once_rather_than_running_it_twice() {
    // Two sandboxes rather than two runs in one: a second run over the same tree
    // is legitimately different (`=` where the first said `+`), so only a fresh
    // tree can tell "ran twice" from "already installed". Compared on target
    // names and notes, never on paths, which differ by tempdir.
    let once = Sandbox::new();
    let twice = Sandbox::new();
    let single = stdout(&once.hooks(&["install", "--hook", "post-commit"]));
    let repeated =
        stdout(&twice.hooks(&["install", "--hook", "post-commit", "--hook", "post-commit"]));
    assert_eq!(
        targets(&repeated),
        targets(&single),
        "a repeated --hook ran the hook twice:\nonce:\n{single}\ntwice:\n{repeated}"
    );
    assert!(
        single.contains(&format!(
            "✅ {} artifact(s) handled",
            targets(&single).len()
        )),
        "the summary counts artifacts, so it is the number the doubling corrupts:\n{single}"
    );

    let once = Sandbox::new();
    let twice = Sandbox::new();
    let single = stdout(&once.agent(&["install", "--agent", "cursor"]));
    let repeated = stdout(&twice.agent(&["install", "--agent", "cursor,cursor"]));
    assert_eq!(
        targets(&repeated),
        targets(&single),
        "a repeated --agent ran the adapter twice:\nonce:\n{single}\ntwice:\n{repeated}"
    );
    assert_eq!(
        notes_of(&repeated),
        notes_of(&single),
        "a repeated --agent doubled the notes:\nonce:\n{single}\ntwice:\n{repeated}"
    );
}

// ---------------------------------------------------------------------------
// 3b. §18 — an agent's install writes only into that agent's own namespace.
// ---------------------------------------------------------------------------

/// **`--agent opencode` alone documents OpenCode, and creates no `.claude/`.**
///
/// The defect ADR-0034 §18 exists to fix, asserted at the process because that
/// is where a user meets it: before §18 this exact command wrote `opencode.json`
/// and nothing else, and the only way to give OpenCode a manual was
/// `--agent claude-code` — which mints a `.claude/` directory in a repository
/// where nobody runs Claude Code, registers a second MCP server, and leaves a
/// reviewer asking why wiring OpenCode installed Anthropic's tooling.
///
/// The negative half is the load-bearing one and is asserted on the
/// **directory**, not on the `SKILL.md` inside it: a run that created an empty
/// `.claude/` would still have made the decision this forbids. Asserted by path
/// rather than by reading the report, because a dispatch bug prints a plausible
/// report either way.
#[test]
fn wiring_opencode_alone_writes_its_own_skill_and_never_a_claude_directory() {
    let s = Sandbox::new();
    let out = s.agent(&["install", "--agent", "opencode"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));

    // The negative first: it is the defect, so it should be the sentence a
    // regression prints. The report goes in the message because the path column
    // is what shows *where* the skill went instead.
    assert!(
        !s.repo_path(".claude").exists(),
        "wiring OpenCode created a Claude Code namespace — the §18 defect is back:\n{}",
        stdout(&out)
    );
    assert!(
        !s.repo_path(".mcp.json").exists(),
        "…and it must not have registered a second MCP server either"
    );
    assert!(
        s.repo_path("opencode.json").is_file()
            && s.repo_path(".opencode/skills/filigrio/SKILL.md").is_file(),
        "`--agent opencode` must wire OpenCode completely — registration *and* \
         manual:\n{}",
        stdout(&out)
    );
    assert_eq!(
        targets(&stdout(&out)),
        vec!["opencode/mcp".to_string(), "opencode/skill".to_string()],
        "…and report both:\n{}",
        stdout(&out)
    );

    // The report must not still send the user to the other agent, either: with
    // its own skill on disk, "install claude-code for the doc" is not stale
    // prose, it is wrong advice (ADR-0034 §18.3).
    for note in notes_of(&stdout(&out)) {
        assert!(
            !note.contains("claude-code") && !note.contains(".claude/skills"),
            "OpenCode writes its own skill; nothing should still point at \
             another adapter: {note}"
        );
    }

    // The mirror, so the boundary is pinned in both directions rather than as a
    // one-way rule about one adapter.
    let s = Sandbox::new();
    let out = s.agent(&["install", "--agent", "claude-code"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(s.repo_path(".claude/skills/filigrio/SKILL.md").is_file());
    assert!(
        !s.repo_path(".opencode").exists() && !s.repo_path("opencode.json").exists(),
        "wiring Claude Code reached into OpenCode's namespace"
    );
}

/// **`--agent opencode --global` puts the registration under `$HOME` and leaves
/// the skill in the repository** (ADR-0034 §18.1), through the real binary.
///
/// The library test asserts the paths; this asserts that the *dispatch* hands
/// the adapter one requested scope and the adapter still splits its two
/// artifacts across it. A CLI that resolved the scope and then also chose the
/// destinations — or an `install_at(scope)` helper that grew a second root —
/// would pass the unit test and fail here.
///
/// `$HOME` is snapshotted rather than probed at one path, because the claim is
/// *no skill anywhere under `$HOME`*: `~/.config/opencode/skills/` is only the
/// first of three global directories OpenCode scans, and asserting on that one
/// would let `~/.claude/skills/` or `~/.agents/skills/` through.
#[test]
fn a_global_opencode_install_registers_under_home_and_leaves_the_skill_in_the_repository() {
    let s = Sandbox::new();
    let out = s.agent(&["install", "--agent", "opencode", "--global"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));

    assert!(
        s.home_path(".config/opencode/opencode.json").is_file(),
        "the registration is an availability declaration and follows --global:\n{}",
        stdout(&out)
    );
    let skills_under_home: Vec<PathBuf> = snapshot(&s.home)
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.to_string_lossy().contains("skills"))
        .collect();
    assert!(
        skills_under_home.is_empty(),
        "a skill asserting *this codebase* has a knowledge graph must never be \
         installed machine-wide; found: {skills_under_home:?}"
    );
    assert!(
        s.repo_path(".opencode/skills/filigrio/SKILL.md").is_file(),
        "the skill is a claim about this checkout, so it stays in this checkout:\n{}",
        stdout(&out)
    );
    assert!(
        !s.repo_path("opencode.json").exists(),
        "…and the registration did not also land in the repository"
    );

    // The report says so rather than leaving the split to be discovered — and
    // the default rendering's aggregate says "user-scoped *registrations*",
    // which is the wording that stays true of the one adapter writing two
    // artifacts at two scopes.
    let explained = stdout(&s.agent(&["status", "--agent", "opencode", "--global", "--explain"]));
    assert!(
        notes_of(&explained)
            .iter()
            .any(|n| n.contains("skill stayed in this repository")),
        "the split must be stated where the user reads it:\n{explained}"
    );
    let brief = stdout(&s.agent(&["status", "--agent", "opencode", "--global"]));
    assert!(
        notes_of(&brief)
            .iter()
            .any(|n| n.starts_with("user-scoped registrations")),
        "the folded line must not claim the skill is user-scoped too:\n{brief}"
    );

    // `uninstall --global` reclaims both halves from where each actually went.
    let out = s.agent(&["uninstall", "--agent", "opencode", "--global"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!s.home_path(".config/opencode/opencode.json").exists());
    assert!(
        !s.repo_path(".opencode/skills/filigrio").exists(),
        "`uninstall --global` left the repository-side skill it wrote:\n{}",
        stdout(&out)
    );
}

/// **`--agent windsurf` writes a project file and does not touch `$HOME`**
/// (2026-08-08 vendor sweep; ADR-0034 §3, §17.1).
///
/// Until this sweep the same command was a *refusal*, quoting a `SCOPE_NOTE`
/// that said Windsurf documents no project-scoped file. It documents
/// `.devin/mcp_config.json`, and documents it as committed to version control —
/// so the refusal was ours, and every Windsurf user was told a vendor limitation
/// existed in order to be sent to a `$HOME` file that reaches the vendor's
/// *legacy* agent.
///
/// Both halves are asserted at the process because both were wrong at the
/// process: the scope gate is in the CLI, and the path is in the adapter.
#[test]
fn wiring_windsurf_writes_the_repositorys_devin_config_and_never_the_legacy_cascade_file() {
    let s = Sandbox::new();
    let out = s.agent(&["install", "--agent", "windsurf"]);
    assert!(
        out.status.success(),
        "the project scope is offered now, not refused:\n{}{}",
        stdout(&out),
        stderr(&out)
    );

    let v: serde_json::Value =
        serde_json::from_str(&read(&s.repo_path(".devin/mcp_config.json"))).unwrap();
    assert!(
        v["mcpServers"]["filigrio"]["command"].is_string(),
        "the vendor's shape is mcpServers.<name>.{{command, args, env}}: {v}"
    );
    assert!(
        snapshot(&s.home).is_empty(),
        "a default-scope install must not touch $HOME:\n{}",
        stdout(&out)
    );

    // The legacy file at neither scope — it is documented, so this is a decision
    // and not an oversight, and it is the decision that a registration must
    // reach the agent the vendor opens new tabs in.
    assert!(s
        .agent(&["install", "--agent", "windsurf", "--global"])
        .status
        .success());
    assert!(s.home_path(".config/devin/mcp_config.json").is_file());
    assert!(
        !s.home_path(".codeium").exists(),
        "~/.codeium/windsurf/mcp_config.json applies to \"the legacy Cascade agent only\""
    );

    // Both verbs tell the user which of the vendor's two agents this reaches,
    // and the sentence that was false for two verification rounds is gone.
    for out in [
        s.agent(&["install", "--agent", "windsurf", "--explain"]),
        s.agent(&["status", "--agent", "windsurf", "--explain"]),
    ] {
        let notes = notes_of(&stdout(&out));
        assert!(
            notes
                .iter()
                .any(|n| n.contains("legacy Cascade agent") && n.contains(".devin/mcp_config.json")),
            "the two-agent split must be stated where the user reads it:\n{}",
            stdout(&out)
        );
        assert!(
            !notes
                .iter()
                .any(|n| n.contains("documents no project-scoped file")),
            "the retracted claim survived:\n{}",
            stdout(&out)
        );
    }

    // …and it is a foldable note, not a paragraph on every run: the user who is
    // on the default agent needs it once, as reference.
    let brief = notes_of(&stdout(&s.agent(&["status", "--agent", "windsurf"])));
    assert!(
        brief
            .iter()
            .any(|n| n.starts_with("the vendor ships a second agent") && n.contains("windsurf")),
        "the aggregate must name the agent it covers: {brief:?}"
    );

    let out = s.agent(&["uninstall", "--agent", "windsurf"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !s.repo_path(".devin").exists(),
        "our empty directory goes too:\n{}",
        stdout(&out)
    );
    assert!(
        s.home_path(".config/devin/mcp_config.json").exists(),
        "…and a project-scoped uninstall does not reach the $HOME half"
    );
}

/// **One template, two destinations** (ADR-0034 §18.1), through the real binary.
///
/// The library test asserts the two rendered files agree; this asserts the
/// *installed* files do, which is the claim a user could check. They must differ
/// in exactly one line — the registration each one tells its reader to check
/// when the tools are missing — because a skill that named the other agent's
/// config would send an OpenCode user to a `.mcp.json` that may not exist.
#[test]
fn the_two_installed_skills_are_one_document_that_names_two_registrations() {
    let s = Sandbox::new();
    assert!(s
        .agent(&["install", "--agent", "claude-code,opencode"])
        .status
        .success());

    let claude = read(&s.repo_path(".claude/skills/filigrio/SKILL.md"));
    let opencode = read(&s.repo_path(".opencode/skills/filigrio/SKILL.md"));

    assert_eq!(
        claude.lines().count(),
        opencode.lines().count(),
        "one template means one shape"
    );
    let differing: Vec<(&str, &str)> = claude
        .lines()
        .zip(opencode.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(
        differing.len(),
        1,
        "exactly one line may differ: {differing:#?}"
    );
    assert!(
        differing[0].0.contains(".mcp.json") && differing[0].1.contains("opencode.json"),
        "and it is the registration each names: {differing:?}"
    );
}

/// **`docs` round-trips a hand-written `AGENTS.md` byte-exactly** (ADR-0034
/// §18.2), through its own command.
///
/// The library test proves the block writer reverses. This proves the *resource*
/// does — install, install again, status, uninstall — over a file the user wrote,
/// reached by the command that replaced `--agent agents-md`. It matters more
/// here than for any other artifact: `AGENTS.md` is the one destination that is
/// unambiguously somebody else's document, and it is now reachable by a verb
/// somebody can run on its own, in a repository with no agents wired at all.
#[test]
fn docs_round_trips_a_hand_written_agents_md_byte_exactly() {
    let s = Sandbox::new();
    s.seed(&s.repo_path("AGENTS.md"), USER_AGENTS_MD);
    let path = s.repo_path("AGENTS.md");

    // Absent before anything is installed — and `status` is not a failure for
    // saying so.
    let out = s.docs(&["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        detail_of(&stdout(&out), "docs/agents-md"),
        "file exists but holds no filigrio block"
    );

    let out = s.docs(&["install"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let after = read(&path);
    assert!(
        after.starts_with(USER_AGENTS_MD),
        "the user's prose must stay verbatim and stay first:\n{after}"
    );
    assert!(after.contains("<!-- filigrio:start"), "{after}");

    // Idempotent, and it says so.
    let out = s.docs(&["install"]);
    assert!(out.status.success());
    assert_eq!(
        lines_of(&stdout(&out))
            .into_iter()
            .map(|l| l.symbol)
            .collect::<Vec<_>>(),
        vec!["=".to_string()],
        "a second `docs install` must change nothing:\n{}",
        stdout(&out)
    );
    assert_eq!(read(&path), after, "…and touch no bytes");

    assert_eq!(
        detail_of(&stdout(&s.docs(&["status"])), "docs/agents-md"),
        "current"
    );

    let out = s.docs(&["uninstall"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        read(&path),
        USER_AGENTS_MD,
        "the user's file must come back byte for byte"
    );
}

// ---------------------------------------------------------------------------
// 4. The lifecycle, over files a user already owns.
// ---------------------------------------------------------------------------

/// Prose the user wrote. `AGENTS.md` is *their* file; we get a managed block in
/// it and nothing else.
const USER_AGENTS_MD: &str = "\
# The Fabricator

House rules, written by hand and not by a tool:

- the parser lives in `src/parse.rs`; start there
- never touch `vendor/`
";

/// Someone else's MCP server, already registered. Written in the 2-space pretty
/// form `json_entry` normalises to, which is what makes a byte-exact round trip
/// available at all (`json_entry`'s module docs state that limit plainly).
const USER_MCP_JSON: &str = "\
{
  \"mcpServers\": {
    \"sqlite\": {
      \"command\": \"uvx\",
      \"args\": [
        \"mcp-server-sqlite\",
        \"--db-path\",
        \"./notes.db\"
      ]
    }
  }
}
";

/// A Codex config that is comments and scalars — and, deliberately, **has no
/// trailing newline**. A real `~/.config/opencode/opencode.json` was found in
/// exactly that shape, and a round trip that helpfully adds the newline comes
/// back one byte longer than it went in (`had_trailing_newline`). This is the
/// CLI-level restatement of that: "byte-exact" has to survive the whole verb,
/// not just the entry writer.
const USER_CODEX_TOML: &str = "\
# Codex, configured by hand.
model = \"gpt-5-codex\"
approval_policy = \"on-request\"";

/// A Hermes config with a **sibling** server under the same key our block is
/// spliced into, and a top-level key after it. YAML is the one format with no
/// format-preserving editor in Rust, so the entry is spliced as text
/// (`yaml_block`) — the sibling and the trailing key are what prove the splice
/// did not re-emit the mapping.
const USER_HERMES_YAML: &str = "\
# Hermes, configured by hand.
mcp_servers:
  filesystem:
    command: mcp-server-filesystem
    args:
      - /srv/notes

default_model: hermes-4
";

/// A husky-style hook that is already doing a job. ADR-0034 §3's "chain, never
/// clobber": our block is appended and removed, and the user's script is
/// untouched in both directions.
const USER_POST_COMMIT: &str = "\
#!/bin/sh
. \"$(dirname -- \"$0\")/_/husky.sh\"

npm test --silent
";

/// Seed a sandbox with the five files a real developer would already have.
fn seeded() -> Sandbox {
    let s = Sandbox::new();
    s.seed(&s.repo_path("AGENTS.md"), USER_AGENTS_MD);
    s.seed(&s.repo_path(".mcp.json"), USER_MCP_JSON);
    s.seed(&s.repo_path(".git/hooks/post-commit"), USER_POST_COMMIT);
    s.seed(&s.home_path(".codex/config.toml"), USER_CODEX_TOML);
    s.seed(&s.home_path(".hermes/config.yaml"), USER_HERMES_YAML);
    s
}

/// The whole surface, over a machine that is already in use:
/// **install → install → status → uninstall**, across all three commands, ending
/// with every pre-existing file byte-identical to what it was before.
///
/// Each library test proves one artifact reverses. This proves the *surface*
/// reverses — every artifact, four commands, seven agents, over files someone
/// else owns. That composition is the thing a user actually experiences, and it
/// is the thing no unit test can assert.
///
/// `docs` is the fourth command since ADR-0034 §18.2, and the byte-exactness
/// requirement is *stronger* for it than for anything else here: `AGENTS.md` is
/// a file the user hand-wrote, it is the one artifact whose whole reversibility
/// mechanism is a marker pair, and it is now reached by a command that can be
/// run on its own.
#[test]
fn a_seeded_repository_and_home_round_trip_byte_exactly_through_the_whole_lifecycle() {
    let s = seeded();
    let before: Vec<(PathBuf, String)> = [
        s.repo_path("AGENTS.md"),
        s.repo_path(".mcp.json"),
        s.repo_path(".git/hooks/post-commit"),
        s.home_path(".codex/config.toml"),
        s.home_path(".hermes/config.yaml"),
    ]
    .into_iter()
    .map(|p| {
        let body = read(&p);
        (p, body)
    })
    .collect();

    // --- install ---------------------------------------------------------
    s.install_everything();

    let agents = read(&s.repo_path("AGENTS.md"));
    assert!(
        agents.starts_with(USER_AGENTS_MD),
        "the user's prose must survive verbatim and stay first:\n{agents}"
    );
    assert!(
        agents.contains("<!-- filigrio:start"),
        "no managed block was appended:\n{agents}"
    );

    let mcp: serde_json::Value =
        serde_json::from_str(&read(&s.repo_path(".mcp.json"))).expect(".mcp.json is still JSON");
    assert!(
        mcp["mcpServers"]["sqlite"]["command"] == "uvx",
        "someone else's server was disturbed: {mcp:#}"
    );
    let command = mcp["mcpServers"]["filigrio"]["command"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        command.ends_with("filigrio-mcp"),
        "the registration must point at the stdio bridge, not the CLI or the daemon: {command}"
    );

    let codex = read(&s.home_path(".codex/config.toml"));
    assert!(
        codex.contains("[mcp_servers.filigrio]") && codex.contains("gpt-5-codex"),
        "codex config lost its own settings or gained no entry:\n{codex}"
    );

    let hermes = read(&s.home_path(".hermes/config.yaml"));
    assert!(
        hermes.contains("filesystem:") && hermes.contains("default_model: hermes-4"),
        "the sibling server or the trailing key was re-emitted away:\n{hermes}"
    );
    assert!(
        hermes.contains("# filigrio:start"),
        "no spliced block:\n{hermes}"
    );

    let hook = read(&s.repo_path(".git/hooks/post-commit"));
    assert!(
        hook.starts_with(USER_POST_COMMIT),
        "husky's hook must be chained, never clobbered:\n{hook}"
    );
    assert!(
        hook.contains("# filigrio-hook-start") && hook.contains("hooks run post-commit"),
        "the appended block does not call the agreed verb:\n{hook}"
    );

    let created = [
        s.repo_path(".claude/skills/filigrio/SKILL.md"),
        s.repo_path(".cursor/mcp.json"),
        s.repo_path("opencode.json"),
        s.repo_path(".opencode/skills/filigrio/SKILL.md"),
        s.repo_path(".devin/mcp_config.json"),
        s.home_path(".openclaw/openclaw.json"),
        s.repo_path(".git/hooks/post-checkout"),
        s.repo_path(".git/hooks/post-merge"),
        s.repo_path(".git/hooks/post-rewrite"),
    ];
    for p in created.iter().chain(s.completion_files().iter()) {
        assert!(p.exists(), "install did not write {}", p.display());
    }

    // --- install again: idempotent, and it says so ------------------------
    for out in [
        s.agent(&["install", "--agent", Sandbox::PROJECT_AGENTS]),
        s.agent(&["install", "--agent", Sandbox::GLOBAL_AGENTS, "--global"]),
        s.docs(&["install"]),
        s.hooks(&["install"]),
        s.completions(&["install"]),
    ] {
        assert!(out.status.success(), "{}", stderr(&out));
        let text = stdout(&out);
        let repeated = lines_of(&text);
        assert!(!repeated.is_empty(), "no artifacts reported:\n{text}");
        for line in &repeated {
            assert_eq!(
                line.symbol, "=",
                "a second install changed {} ({}) — idempotence is the contract:\n{text}",
                line.target, line.path
            );
        }
    }

    // --- status: everything current --------------------------------------
    for out in [
        s.agent(&["status", "--agent", Sandbox::PROJECT_AGENTS]),
        s.agent(&["status", "--agent", Sandbox::GLOBAL_AGENTS, "--global"]),
        s.docs(&["status"]),
        s.hooks(&["status"]),
        s.completions(&["status"]),
    ] {
        assert!(out.status.success(), "{}", stderr(&out));
        let text = stdout(&out);
        for line in lines_of(&text) {
            assert_eq!(
                line.symbol, "=",
                "status calls {} not-present after a successful install:\n{text}",
                line.target
            );
        }
    }

    // --- uninstall: byte-exact reversal -----------------------------------
    for out in [
        s.agent(&["uninstall", "--agent", Sandbox::PROJECT_AGENTS]),
        s.agent(&["uninstall", "--agent", Sandbox::GLOBAL_AGENTS, "--global"]),
        s.docs(&["uninstall"]),
        s.hooks(&["uninstall"]),
        s.completions(&["uninstall"]),
    ] {
        assert!(
            out.status.success(),
            "uninstall exited {:?}:\n{}{}",
            out.status.code(),
            stdout(&out),
            stderr(&out)
        );
    }

    for (path, original) in &before {
        let now = read(path);
        assert_eq!(
            &now,
            original,
            "{} did not come back byte-identical.\n--- before ---\n{original}\n--- after ---\n{now}",
            path.display()
        );
    }
    for p in created.iter().chain(s.completion_files().iter()) {
        assert!(
            !p.exists(),
            "uninstall left {} behind:\n{}",
            p.display(),
            read(p)
        );
    }
    // The directories we minted go too; the ones we merely wrote into stay.
    for dir in [
        s.repo_path(".cursor"),
        s.repo_path(".claude/skills/filigrio"),
        s.repo_path(".opencode/skills/filigrio"),
        s.repo_path(".devin"),
        s.home_path(".openclaw"),
    ] {
        assert!(!dir.exists(), "uninstall left {} behind", dir.display());
    }
    assert!(
        s.home_path(".codex/config.toml").exists() && s.home_path(".hermes/config.yaml").exists(),
        "a config the user already had must never be deleted, only edited back"
    );
}

/// **A registration whose socket has moved reports `stale`, and a re-install
/// clears it.**
///
/// Until this existed, all seven MCP adapters answered `registered` — an
/// existence check on their own key — so a user who changed `--socket`, moved
/// the install directory, or upgraded past a new bridge argument was told every
/// client was wired while every client spawned the bridge against something that
/// was not there. The README carried the workaround in prose, which is what a
/// known and documented defect looks like.
///
/// Only the process can prove it. `--socket` is a CLI flag; it lands in five
/// JSON files, one TOML file and one YAML file, each compared by a different
/// code path, and a library test can exercise at most one of them at a time.
#[test]
fn a_socket_that_moved_makes_every_registration_stale_until_install_is_rerun() {
    let s = seeded();
    s.install_everything();

    // `status` is run at both scopes and the two reports concatenated, because
    // no single invocation can see all seven registrations any more — which is
    // itself the shape §17 chose, and this is the test that has to live with it.
    let status_of = |socket: &Path| -> String {
        format!(
            "{}{}",
            stdout(&s.agent_with_socket(socket, &["status", "--agent", Sandbox::PROJECT_AGENTS])),
            stdout(&s.agent_with_socket(
                socket,
                &["status", "--agent", Sandbox::GLOBAL_AGENTS, "--global"]
            ))
        )
    };

    let text = status_of(&s.dead_socket);
    for target in MCP_TARGETS {
        assert_eq!(
            detail_of(&text, target),
            "current",
            "`{target}` is not current immediately after the install that wrote it:\n{text}"
        );
    }

    // Same tree, same files, different `--socket`: nothing on disk changed, but
    // what install *would* write did.
    let moved = s.root.join("moved.sock");
    let text = status_of(&moved);
    for target in MCP_TARGETS {
        assert_eq!(
            detail_of(&text, target),
            "stale — re-run install to refresh",
            "`{target}` still points at the old socket and says it is fine:\n{text}"
        );
    }

    // The fix the message names has to be the fix that works.
    for out in [
        s.agent_with_socket(&moved, &["install", "--agent", Sandbox::PROJECT_AGENTS]),
        s.agent_with_socket(
            &moved,
            &["install", "--agent", Sandbox::GLOBAL_AGENTS, "--global"],
        ),
    ] {
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let text = status_of(&moved);
    for target in MCP_TARGETS {
        assert_eq!(
            detail_of(&text, target),
            "current",
            "re-running install did not clear `{target}`:\n{text}"
        );
    }

    // And the same property from the other side: the socket we started with is
    // now the stale one, so this is a comparison and not a constant.
    let text = status_of(&s.dead_socket);
    for target in MCP_TARGETS {
        assert_eq!(
            detail_of(&text, target),
            "stale — re-run install to refresh",
            "`{target}` calls the *previous* socket current too, so it is not \
             comparing anything:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Honest failure: reported, survivable, and visible in `$?`.
// ---------------------------------------------------------------------------

/// A foreign file at one of our destinations fails **that artifact**, and only
/// that artifact: the run continues, the other two are installed, the stranger's
/// file is untouched, and the process exits **1**.
///
/// Three separate promises, and each one has an obvious wrong implementation
/// that the other two would hide:
///
/// * exiting 0 because the report rendered fine (ADR-0042 F6c — a failed command
///   that exits 0 is how a CI job goes green on a broken install);
/// * aborting the run at the first failure, so one unwritable path hides the
///   rest;
/// * "fixing" the collision by overwriting a file we did not write.
///
/// `filigrio-install` proves the refusal at the artifact level; only the process
/// can prove the exit status and the continuation.
#[test]
fn a_foreign_completion_file_fails_one_artifact_and_the_run_still_exits_one() {
    let s = Sandbox::new();
    let foreign = "# hand written by me, years ago\ncomplete -F _mine filigrio\n";
    let bash = s.xdg_data.join("bash-completion/completions/filigrio");
    s.seed(&bash, foreign);

    let out = s.completions(&["install"]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "a failed artifact must reach the exit status; got {:?}\n{}{}",
        out.status.code(),
        stdout(&out),
        stderr(&out)
    );

    let text = stdout(&out);
    let failed: Vec<Line> = lines_of(&text)
        .into_iter()
        .filter(|l| l.symbol == "!")
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "expected exactly one failure line:\n{text}"
    );
    assert_eq!(failed[0].target, "completions/bash");
    assert!(
        text.contains("was not written by filigrio")
            && text.contains("no `# filigrio-completion` marker"),
        "the failure must carry the reason, not just the path:\n{text}"
    );
    assert!(
        stderr(&out).contains("1 of 3 artifact(s) failed"),
        "the summary must count the failure: {}",
        stderr(&out)
    );

    assert_eq!(
        read(&bash),
        foreign,
        "a file we did not write is never overwritten"
    );

    // The run did not abort: the other two shells landed.
    for p in [
        s.xdg_data.join("zsh/site-functions/_filigrio"),
        s.xdg_config.join("fish/completions/filigrio.fish"),
    ] {
        assert!(
            p.exists(),
            "one failure aborted the run — {} was never written:\n{text}",
            p.display()
        );
    }

    // And the other two commands are unaffected, which is the family boundary
    // doing its job under a failure rather than under a happy path.
    assert!(s.agent(&["install", "--agent", "cursor"]).status.success());
    assert!(s.hooks(&["install"]).status.success());
}

/// **The three verbs agree about one file.**
///
/// The stranger's completion above fails `install` and refuses `uninstall`;
/// `status` used to call it `=` — the glyph a *working, current* artifact gets —
/// and exit 0. One file, three verbs, three verdicts, and the reassuring one was
/// the verb people run first.
///
/// `filigrio-install` proves the arm reports `Action::Foreign`; only the process
/// can prove that what a user actually sees in the column is not the success
/// symbol.
#[test]
fn status_marks_a_foreign_file_with_a_symbol_no_installed_artifact_ever_gets() {
    let s = Sandbox::new();
    let bash = s.xdg_data.join("bash-completion/completions/filigrio");
    s.seed(
        &bash,
        "# hand written by me, years ago\ncomplete -F _mine filigrio\n",
    );

    // Install the other two shells first, so the report holds real `=` lines to
    // be confused with — the assertion below is about telling them apart.
    s.completions(&["install"]);

    let out = s.completions(&["status"]);
    let text = stdout(&out);
    let line = lines_of(&text)
        .into_iter()
        .find(|l| l.target == "completions/bash")
        .unwrap_or_else(|| panic!("no completions/bash line:\n{text}"));

    assert_eq!(
        line.symbol, "?",
        "a file filigrio did not write must not share a symbol with one it did:\n{text}"
    );
    assert!(
        lines_of(&text).iter().any(|l| l.symbol == "="),
        "the fixture stopped exercising the confusion: nothing else in this \
         report is `=`, so any symbol would have looked distinct:\n{text}"
    );
    assert!(
        line.detail.contains("not ours"),
        "the detail must say whose it is: {:?}",
        line.detail
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "a question about someone else's file is not a failed status run"
    );
}

// ---------------------------------------------------------------------------
// 6. The note block — aggregated by default, whole behind `--explain`.
// ---------------------------------------------------------------------------

/// **A default install says less, and `--explain` still says everything.**
///
/// The measurement that motivated the aggregation: a plain install printed
/// sixteen step lines and twenty notes, of which exactly one — the zsh `fpath`
/// line — asked the user to do anything. The other nineteen were five sentences
/// repeated once per adapter, and nineteen lines of reference material between
/// the steps and the failures is how a `!` line scrolls past unread.
///
/// Retiring the wide command shrank the numbers but not the mechanism, and the
/// mechanism is what this pins. Every number below is *derived from the two
/// runs*, never a literal: a hard-coded count would have to be edited into
/// agreement with whatever the code happened to do, which is how a test starts
/// passing for the wrong reason.
#[test]
fn a_default_install_aggregates_its_notes_and_explain_restores_every_one() {
    let s = Sandbox::new();
    let args = ["install", "--agent", Sandbox::GLOBAL_AGENTS, "--global"];
    let brief = stdout(&s.agent(&args));
    // Idempotent, so the second run reports the same notes over the same tree.
    let full = stdout(&s.agent(&[
        "install",
        "--agent",
        Sandbox::GLOBAL_AGENTS,
        "--global",
        "--explain",
    ]));

    let brief_notes = notes_of(&brief);
    let full_notes = notes_of(&full);

    assert!(
        full_notes.len() >= 8,
        "the run this compresses records at least two notes per adapter; got {}:\n{full}",
        full_notes.len()
    );
    assert!(
        brief_notes.len() < full_notes.len(),
        "the default rendering is not compressing: {} notes against {}:\n{brief}",
        brief_notes.len(),
        full_notes.len()
    );

    // The aggregate has to stay an answer, not a summary: each line names the
    // `--agent` slugs it covers, so "why did `--agent codex` not write a doc?"
    // is answerable without the flag.
    let joined = brief_notes.join("\n");
    let registration_only = brief_notes
        .iter()
        .find(|l| l.starts_with("registration only"))
        .unwrap_or_else(|| panic!("no registration-only aggregate in:\n{joined}"));
    let user_scoped = brief_notes
        .iter()
        .find(|l| l.starts_with("user-scoped"))
        .unwrap_or_else(|| panic!("no user-scoped aggregate in:\n{joined}"));
    for slug in ["codex", "openclaw", "hermes"] {
        assert!(
            registration_only.contains(slug),
            "the registration-only line does not name `{slug}`: {registration_only}"
        );
        assert!(
            user_scoped.contains(slug),
            "the user-scoped line does not name `{slug}`: {user_scoped}"
        );
    }

    // The footer counts what stopped being printed — the difference between the
    // two runs, and nothing else.
    let (footer, shown) = brief_notes.split_last().expect("a footer");
    let claimed: usize = footer
        .split_whitespace()
        .next()
        .and_then(|w| w.parse().ok())
        .unwrap_or_else(|| panic!("the footer must open with a count: {footer}"));
    assert_eq!(
        claimed,
        full_notes.len() - shown.len(),
        "footer claims {claimed}; --explain prints {} and the default prints {}:\n{brief}",
        full_notes.len(),
        shown.len()
    );
    assert!(footer.contains("--explain"), "{footer}");

    // `--explain` is the promise that nothing was dropped at record time.
    assert!(
        !full_notes.iter().any(|l| l.contains("--explain")),
        "the verbose rendering must not carry the footer:\n{full}"
    );
    // A per-client caveat the aggregate line defers rather than prints: Codex's
    // is the trust rule, which is why its project file is declined.
    let caveat = "sit on disk being ignored";
    assert!(
        full_notes.iter().any(|n| n.contains(caveat)),
        "a per-client caveat the aggregate defers must be in --explain:\n{full}"
    );
    assert!(
        !brief_notes.iter().any(|n| n.contains(caveat)),
        "the long form of a folded note must not survive the default rendering:\n{brief}"
    );
}

/// **The zsh `fpath` note is never folded, in either mode.**
///
/// The one note that asks the user to act, and the one whose loss is invisible:
/// without the `fpath` entry, zsh's completions are installed and silently do
/// nothing, which reads as "filigrio has no completions". It is emitted unkinded
/// for exactly that reason, and this asserts the *whole* sentence — including the
/// directory the run resolved — survives the aggregation.
#[test]
fn the_zsh_fpath_note_survives_at_full_detail_with_and_without_explain() {
    let s = Sandbox::new();
    let dir = s.xdg_data.join("zsh/site-functions");
    let expected = format!(
        "zsh needs the directory on its fpath: add `fpath=({} $fpath)` before `compinit` in \
         ~/.zshrc",
        dir.display()
    );

    for args in [vec!["install"], vec!["install", "--explain"]] {
        let text = stdout(&s.completions(&args));
        assert!(
            notes_of(&text).contains(&expected),
            "{args:?} lost or abridged the fpath note.\nwanted: {expected}\ngot:\n{text}"
        );
    }
}

/// **A sweep's exclusion note is never folded either**, for the same reason: it
/// names a flag, which makes it one of the handful of notes that asks the user to
/// *do* something. Folding it into "4 further notes" would hide the only line
/// explaining why four agents survived an uninstall the user believes removed
/// everything.
///
/// It is also **one** note rather than one per agent. Reusing each adapter's own
/// `SCOPE_NOTE` verbatim produced four paragraph-length notes on every sweep,
/// which is the density `Notes::Aggregated` exists to fight — so the aggregation
/// happens where the fact is produced rather than where it is rendered.
#[test]
fn a_scope_exclusion_note_is_printed_in_full_in_both_renderings() {
    let s = Sandbox::new();
    // Installed first, so the note is the one that matters: four registrations
    // under `$HOME` that this sweep is about to walk past.
    s.install_everything();
    for args in [vec!["uninstall"], vec!["uninstall", "--explain"]] {
        let text = stdout(&s.agent(&args));
        let notes: Vec<String> = notes_of(&text)
            .into_iter()
            .filter(|n| n.contains("did not touch them"))
            .collect();
        assert_eq!(
            notes.len(),
            1,
            "{args:?} must print exactly one exclusion note, not one per agent:\n{text}"
        );
        for slug in ["codex", "openclaw", "hermes"] {
            assert!(
                notes[0].contains(slug),
                "{args:?} folded `{slug}` away: {}",
                notes[0]
            );
        }
        assert!(notes[0].contains("--global"), "{}", notes[0]);
    }
}

/// **A failure is still loud after the notes were compressed.**
///
/// The compression exists to stop a `!` line scrolling past unread, so it is
/// worth nothing if it ever costs one its visibility. Pinned in both modes, with
/// the reason line and the exit status.
#[test]
fn a_failure_line_and_its_reason_survive_both_note_renderings() {
    for args in [vec!["install"], vec!["install", "--explain"]] {
        let s = Sandbox::new();
        s.seed(
            &s.xdg_data.join("bash-completion/completions/filigrio"),
            "# hand written by me\ncomplete -F _mine filigrio\n",
        );

        let out = s.completions(&args);
        let text = stdout(&out);
        let failed: Vec<Line> = lines_of(&text)
            .into_iter()
            .filter(|l| l.symbol == "!")
            .collect();

        assert_eq!(failed.len(), 1, "{args:?} lost the failure line:\n{text}");
        assert_eq!(failed[0].target, "completions/bash");
        assert!(
            text.contains("was not written by filigrio"),
            "{args:?} lost the reason:\n{text}"
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?} must still fail the process"
        );
    }
}
