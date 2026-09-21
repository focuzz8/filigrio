//! Hermes / Nous (ADR-0034 §15, verified 2026-07-29 against the **live binary**
//! — `Hermes Agent v0.19.0 (2026.7.20)`, exercised against a scratch
//! `HERMES_HOME`; the real `~/.hermes/config.yaml` was never read or written,
//! being mode 0600 and holding credentials).
//!
//! One artifact, **user-scoped**: `~/.hermes/config.yaml`, an entry at
//! `mcp_servers.filigrio`.
//!
//! ## YAML, and why this adapter does not get a value model
//!
//! This is the third config format after JSON and TOML, and the first with no
//! format-preserving editor in Rust: `serde_yaml` is deprecated and round-trips
//! through a value model. Using it would reformat the user's file on the first
//! install — the exact defect this crate records against three vendor CLIs. So
//! the registration is written as a **spliced managed block**
//! ([`crate::yaml_block`] over [`crate::block`]), which is byte-exact by
//! construction because it never parses the file.
//!
//! ## The measurement that shaped the design
//!
//! Hermes' own writer **strips every comment in the file**. A plain `hermes mcp
//! remove` — not even an install — turned a 20-line hand-written config into 48
//! lines: both user comments gone, both of *our* markers gone,
//! `_config_version: 33` injected, and pages of commented-out template
//! boilerplate appended. Our entry survived; our markers did not.
//!
//! So a marker-only uninstall would report "nothing of ours" and leave a live
//! `filigrio:` entry pointing at a binary the user had just removed. Removal
//! therefore falls back to finding the entry **by name** and excising its lines,
//! and says which path it took. Read paths do not rewrite — `hermes mcp list`
//! left a hand-written config byte for byte — so the markers survive ordinary
//! use and the fallback covers the case where they did not.
//!
//! ## Why not `hermes mcp add`, the third vendor CLI tested and rejected
//!
//! It is **interactive** (`Enable all 9 tools? [Y/n/select]:`) and
//! discovery-first — it connects to the server and enumerates its tools before
//! saving, so it blocks on a command that does not exist yet — and its write is
//! the rewrite described above. Codex's CLI deletes a comment and appends a
//! newline; OpenClaw's reformats the file and injects a timestamp that survives
//! removal; Hermes' re-emits the document from a template. Three vendors, three
//! measurements, one conclusion: a vendor's config writer optimises for its own
//! defaults, not for the user's file (ADR-0034 §15).
//!
//! ## `enabled` is not ours to write
//!
//! `hermes mcp add` writes `enabled: true`, and the reference lists it as part
//! of the entry — but a server with **no** `enabled` key lists as `✓ enabled`,
//! measured side by side in one config. It is a preference with a working
//! default, so the same rule as every other adapter applies: `command` and
//! `args`, nothing else. `ENTRY_KEYS` holds that and a test asserts it.
//!
//! ## No Hermes capability doc
//!
//! Hermes reads `AGENTS.md` — its bootstrap list is `["SOUL.md", "IDENTITY.md",
//! "AGENTS.md", "TOOLS.md", …]` and `--ignore-rules` is documented as disabling
//! "AGENTS.md/memory injection" — which [`crate::docs`] writes.
//! Registration-only, the fifth of that shape — with two qualifications below:
//! the skills root, and a gap.
//!
//! ### Why no Hermes skill: `~/.hermes/skills/` is the only root there is
//!
//! `https://hermes-agent.nousresearch.com/docs/user-guide/features/skills`
//! (2026-08-08) calls it "the primary directory and source of truth" for every
//! skill, and its sole extension is `external_dirs` — a list declared in the
//! same `~/.hermes/config.yaml` this adapter writes, whose entries are absolute
//! or `~`-expanded. No cwd-relative discovery, no walk to a git root: a
//! repository *can* be reached, but only after the user edits a file in
//! `$HOME`, which is what ADR-0034 §18.1's question excludes. The absence is a
//! vendor fact, not a judgement like [`super::cursor`]'s —
//! `docs/vendor-path-verification.md` (`hermes`).
//!
//! ### `.hermes.md` suppresses `AGENTS.md`, and we neither write nor detect it
//!
//! `https://hermes-agent.nousresearch.com/docs/user-guide/which-file-does-what`
//! (2026-08-08) documents a project-scoped, repo-committed, Hermes-specific
//! context file whose "discovery walks up to the git root", and then:
//!
//! > Only **one** project context type is loaded per session, first match wins:
//! > `.hermes.md` → `AGENTS.md` → `CLAUDE.md` → `.cursorrules`.
//!
//! So the claim above — Hermes reads `AGENTS.md`, therefore [`crate::docs`]
//! covers it — **holds only in a repository with no `.hermes.md` or
//! `HERMES.md`**. In one that has either, Hermes reads that and never reads
//! ours, while `filigrio agent install --agent hermes` and `filigrio docs
//! install` both report success. That is this crate's worst failure shape, and
//! it is stated rather than fixed: writing a `.hermes.md` would use the same
//! first-match-wins rule to disable a repository's existing `AGENTS.md` for every
//! other agent, and detecting one is behaviour this adapter does not have —
//! `detect()` is deliberately `Unknown` and nothing here looks at the project
//! root. Which of those to do is ADR-0034's question.
//! `docs/vendor-path-verification.md` lists it under *Not modelled at all*.

use super::{
    bridge_args, note_detection, report_registration, ClientId, ClientInstaller, Detection,
    OtherScope, Scope, ScopeSupport,
};
use crate::block::YAML;
use crate::capability::SERVER_NAME;
use crate::{yaml_block, Environment, NoteKind, Report};
use std::path::PathBuf;

pub struct Hermes;

/// The top-level YAML key. Not `mcpServers`, not `mcp.servers` — a fourth
/// spelling of the same idea, spelled out here.
const MCP_SERVERS_YAML: &str = "mcp_servers";

/// Exactly the two fields we write; see the module note on `enabled`. `hermes
/// mcp add` also takes `--url`, `--auth`, `--preset`, `--connect-timeout` and
/// `--env`, all preferences with working defaults.
#[cfg(test)]
const ENTRY_KEYS: &[&str] = &["command", "args"];

/// Said on install *and* status. Three facts a user cannot recover on their own:
/// the scope, the credential mode, and that a Hermes write eats our markers.
const SCOPE_NOTE: &str =
    "Hermes' config is global only (~/.hermes/config.yaml) — it documents no project-scoped file, \
     so this registration is machine-local and does not travel with the repository; every \
     teammate installs it themselves. That file holds credentials and is owner-only (0600): if \
     this install created it we matched that mode, and if it already existed we left its mode \
     alone. Note that any `hermes mcp add`/`remove` rewrites the file and strips every comment in \
     it, including our markers — uninstall still finds the entry by name, so it stays reversible";

/// Said on install *and* on status, as in [`super::cursor`]. The skills clause
/// is a fact about this vendor a user cannot recover on their own — module
/// header, which also has the `.hermes.md` condition under which this note's
/// first clause stops being true.
fn doc_note() -> String {
    super::registration_only_note(
        "Hermes",
        "it is in Hermes' bootstrap file list; Hermes' only skills root is ~/.hermes/skills/, \
         extended solely by `external_dirs` declared in ~/.hermes/config.yaml, so a repository \
         cannot supply a skill and nothing is written there",
    )
}

/// A double-quoted YAML scalar. Plain scalars would be fine for the paths we
/// actually emit, but a path containing `:` or `#` would silently change
/// meaning, and "silently" is the word this crate exists to avoid. `\` and `"`
/// are escaped; so is every control character (legal in a Unix path, if only
/// ever by accident), as YAML's own `\n` / `\t` / `\r` / `\x..` forms — emitted
/// raw, a newline inside double quotes is *folded to a space* and the value
/// silently changes, while the other control bytes are not YAML-printable at
/// all and would break the file this module just wrote. UTF-8 passes through.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Hermes {
    fn config_yaml(env: &Environment) -> PathBuf {
        env.home.join(".hermes/config.yaml")
    }

    /// The entry as YAML text, written at column 0; [`crate::yaml_block`]
    /// indents it to wherever it lands.
    ///
    /// The first line is the guidance the [`YAML`] markers used to carry
    /// (`crate::block`'s module docs, *A delimiter names the thing it
    /// delimits*). It lives here because the body is regenerated on every
    /// install and can therefore say anything, for ever, without the delimiter
    /// changing underneath the blocks it already wrote. A YAML comment is
    /// lexical: `yaml_block`'s `indent_lines` moves it to whatever column the
    /// mapping uses and the parser ignores it either way, so it is invisible to
    /// both the value comparison in [`crate::yaml_block::state`] and the
    /// fragment parse behind it — pinned by
    /// [`tests::the_guidance_comment_is_invisible_to_the_current_versus_stale_comparison`].
    fn entry_body(env: &Environment) -> String {
        let mut out = format!(
            "# managed by `filigrio agent install`; edits between the filigrio markers are \
             overwritten\n{SERVER_NAME}:\n  command: {}\n",
            quoted(&env.bridge_bin.display().to_string())
        );
        let args = bridge_args(env);
        if !args.is_empty() {
            out.push_str("  args:\n");
            for arg in args {
                out.push_str(&format!("    - {}\n", quoted(&arg)));
            }
        }
        out.trim_end().to_string()
    }
}

impl ClientInstaller for Hermes {
    fn id(&self) -> ClientId {
        ClientId::Hermes
    }

    fn display_name(&self) -> &'static str {
        "Hermes"
    }

    /// §17.1's third row, as [`super::openclaw`]: no project-scoped file exists,
    /// and `SCOPE_NOTE` is the sentence that says so.
    fn scope_support(&self) -> ScopeSupport {
        ScopeSupport {
            default: Scope::Global,
            other: OtherScope::NotOffered(SCOPE_NOTE),
        }
    }

    fn detect(&self, _env: &Environment) -> Detection {
        // `~/.hermes` holds the install itself, but the only path this crate
        // *verified* is the config file it writes, and probing that would make
        // the first install teach every later run to report "detected".
        // `super::codex` and `super::openclaw` document the
        // same trap.
        Detection::Unknown(
            "the only verified Hermes path is ~/.hermes/config.yaml, which this adapter writes \
             itself, so probing for it would only find our own footprint"
                .into(),
        )
    }

    fn install(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::UserScoped, self.id().slug(), SCOPE_NOTE);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());

        let cfg = Self::config_yaml(env);
        let existed = cfg.exists();
        let outcome = yaml_block::upsert(
            &cfg,
            MCP_SERVERS_YAML,
            SERVER_NAME,
            &Self::entry_body(env),
            &YAML,
        );
        if !existed && outcome.is_ok() {
            super::restrict_to_owner(&cfg, "Hermes", report);
        }
        report.record("hermes/mcp", &cfg, outcome);
    }

    fn uninstall(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        let cfg = Self::config_yaml(env);
        report.record(
            "hermes/mcp",
            &cfg,
            yaml_block::remove(&cfg, MCP_SERVERS_YAML, SERVER_NAME, &YAML),
        );
        // Bounded at `$HOME`, and `~/.hermes` only goes if it is empty — a real
        // install keeps the agent itself, sessions, skills and its own
        // timestamped backups in there, and the walk stops at the first of them.
        if let Some(dir) = cfg.parent() {
            crate::prune_empty_dirs(dir, &env.home);
        }
    }

    fn status(&self, env: &Environment, _scope: Scope, report: &mut Report) {
        note_detection(self.id(), self.display_name(), self.detect(env), report);
        report.note_kind(NoteKind::UserScoped, self.id().slug(), SCOPE_NOTE);
        report.note_kind(NoteKind::RegistrationOnly, self.id().slug(), doc_note());

        let cfg = Self::config_yaml(env);
        report_registration(
            "hermes/mcp",
            &cfg,
            yaml_block::state(&cfg, MCP_SERVERS_YAML, SERVER_NAME, &Self::entry_body(env)),
            report,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, STALE};

    fn env(root: &std::path::Path, home: &std::path::Path) -> Environment {
        Environment {
            project_root: root.to_path_buf(),
            home: home.to_path_buf(),
            cli_bin: PathBuf::from("/opt/g/bin/filigrio"),
            bridge_bin: PathBuf::from("/opt/g/bin/filigrio-mcp"),
            socket_path: PathBuf::from("/run/filigrio.sock"),
            version: "0.0.1".into(),
        }
    }

    #[test]
    fn install_writes_the_documented_shape_at_the_user_scope_path() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let mut r = Report::default();
        Hermes.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let text = std::fs::read_to_string(home.path().join(".hermes/config.yaml")).unwrap();
        assert!(text.contains("mcp_servers:\n"), "got:\n{text}");
        assert!(text.contains("\n  filigrio:\n"));
        assert!(text.contains("command: \"/opt/g/bin/filigrio-mcp\""));
        assert!(text.contains("- \"--socket\""));
        assert!(text.contains("- \"/run/filigrio.sock\""));
        assert!(
            !text.contains("mcpServers") && !text.contains("mcp.servers"),
            "another client's spelling leaked in:\n{text}"
        );

        // Nothing landed in the repository — this client has no project scope.
        assert!(!d.path().join(".hermes").exists());
    }

    /// `enabled` defaults to true (measured: a server with no `enabled` key
    /// lists as `✓ enabled`), so it is a preference and not ours to write.
    #[test]
    fn the_entry_carries_exactly_the_two_keys_we_chose_to_write() {
        let d = tempfile::tempdir().unwrap();
        let body = Hermes::entry_body(&env(d.path(), d.path()));
        let keys: Vec<&str> = body
            .lines()
            .filter(|l| l.starts_with("  ") && !l.starts_with("    "))
            .filter_map(|l| l.trim().split(':').next())
            .collect();
        assert_eq!(keys, ENTRY_KEYS, "got:\n{body}");
    }

    /// **The guidance comment is part of the body and invisible to the
    /// comparison** — which is the whole reason it could leave the delimiter.
    ///
    /// It is a `#` comment at column 0 in the fragment, so `indent_lines` moves
    /// it to whatever column the container mapping uses; a YAML comment is
    /// lexical, so the parser emits no event for it. Both halves are asserted,
    /// because either one failing turns every Hermes install into a permanent
    /// `stale` on a 0600 credentials file that re-running install cannot clear.
    #[test]
    fn the_guidance_comment_is_invisible_to_the_current_versus_stale_comparison() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());

        let body = Hermes::entry_body(&e);
        let first = body.lines().next().unwrap();
        assert!(
            first.starts_with("# managed by") && first.contains("overwritten"),
            "the guidance the marker used to carry must be the body's first line: {first}"
        );

        // A user's config whose entries are indented four, so "indented to
        // wherever it lands" is a real claim rather than the default.
        let p = home.path().join(".hermes/config.yaml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "mcp_servers:\n    sentry:\n        command: npx\n").unwrap();

        let mut r = Report::default();
        Hermes.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("\n    # managed by"),
            "the comment must be indented with the entry it introduces:\n{text}"
        );
        assert!(
            text.contains("        command: ") && text.contains("    filigrio:\n"),
            "the entry itself must still land inside the mapping:\n{text}"
        );

        let mut r = Report::default();
        Hermes.status(&e, Scope::Global, &mut r);
        assert_eq!(
            r.steps[0].detail, "current",
            "a comment in the body must not read as a difference in the entry"
        );
    }

    /// A path with a `:` in it is why the scalars are quoted — unquoted, YAML
    /// would read `command: /a:b` as something else entirely, silently.
    #[test]
    fn awkward_paths_are_quoted_and_escaped() {
        let d = tempfile::tempdir().unwrap();
        let mut e = env(d.path(), d.path());
        e.bridge_bin = PathBuf::from("/opt/a:b/say \"hi\"/filigrio-mcp");
        let body = Hermes::entry_body(&e);
        assert!(
            body.contains(r#"command: "/opt/a:b/say \"hi\"/filigrio-mcp""#),
            "got:\n{body}"
        );
    }

    /// A control character in a path must come out as YAML's escape, never as a
    /// raw byte: emitted raw, a newline inside double quotes is *folded to a
    /// space* — a silently changed value — and before this was handled the raw
    /// byte broke the entry across two lines, producing YAML the walk could not
    /// even parse back. End to end because the emission is only half the claim:
    /// `status` must read the escaped scalar back as `current`.
    #[test]
    fn control_characters_in_paths_are_escaped_not_emitted_raw() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut e = env(d.path(), home.path());
        e.bridge_bin = PathBuf::from("/opt/weird\nname\t/filigrio-mcp");

        let body = Hermes::entry_body(&e);
        assert!(
            body.contains(r#"command: "/opt/weird\nname\t/filigrio-mcp""#),
            "got:\n{body}"
        );
        assert!(
            !body.contains("weird\nname"),
            "no raw control byte may reach the emitted YAML:\n{body}"
        );

        Hermes.install(&e, Scope::Global, &mut Report::default());
        let mut r = Report::default();
        Hermes.status(&e, Scope::Global, &mut r);
        assert_eq!(
            r.steps[0].detail, "current",
            "the escaped scalar must read back as the same value: {:?}",
            r.steps[0]
        );
    }

    #[test]
    fn both_install_and_status_state_the_scope_and_the_marker_caveat() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());

        for run in [
            {
                let mut r = Report::default();
                Hermes.install(&e, Scope::Global, &mut r);
                r
            },
            {
                let mut r = Report::default();
                Hermes.status(&e, Scope::Global, &mut r);
                r
            },
        ] {
            assert!(
                run.notes.iter().any(|n| n.text().contains("global only")
                    && n.text().contains("does not travel")
                    && n.text().contains("strips every comment")),
                "notes were {:?}",
                run.notes
            );
            assert!(
                run.notes.iter().any(|n| n.text().contains("AGENTS.md")
                    && n.text().contains("`filigrio docs install`")),
                "notes were {:?}",
                run.notes
            );
        }
    }

    #[test]
    fn detection_stays_unknown_even_after_an_install() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        Hermes.install(&e, Scope::Global, &mut Report::default());
        assert!(matches!(Hermes.detect(&e), Detection::Unknown(_)));
    }

    #[test]
    fn install_is_idempotent_and_uninstall_leaves_no_trace_in_home() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());

        let mut r = Report::default();
        Hermes.install(&e, Scope::Global, &mut r);
        Hermes.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(r.steps.iter().any(|s| s.action == Action::Unchanged));

        let mut r = Report::default();
        Hermes.uninstall(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        assert!(!home.path().join(".hermes").exists(), "our dir goes too");
        assert!(home.path().exists(), "$HOME survives, obviously");
    }

    /// A real `~/.hermes` holds the agent itself, sessions, skills and Hermes'
    /// own timestamped backups; the prune must stop there.
    #[test]
    fn uninstall_keeps_a_hermes_directory_the_vendor_is_using() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        Hermes.install(&e, Scope::Global, &mut Report::default());
        let theirs = home.path().join(".hermes/config.yaml.bak.20260729_205823");
        std::fs::write(&theirs, "model: x\n").unwrap();

        Hermes.uninstall(&e, Scope::Global, &mut Report::default());
        assert!(theirs.exists(), "Hermes' own backup must survive");
    }

    /// The headline promise, on the shape Hermes' own writer destroys: comments
    /// above and below, a sibling server, and a flow sequence.
    #[test]
    fn a_users_config_round_trips_byte_exactly() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".hermes/config.yaml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let prior = concat!(
            "# my hermes config\n",
            "model: hermes-4-405b\n",
            "\n",
            "mcp_servers:\n",
            "  sentry:\n",
            "    command: npx\n",
            "    args: [\"-y\", \"@sentry/mcp\"]\n",
            "    enabled: true\n",
            "\n",
            "# a trailing note\n",
            "temperature: 0.7\n"
        );
        std::fs::write(&p, prior).unwrap();

        Hermes.install(&e, Scope::Global, &mut Report::default());
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("# my hermes config"), "their comment stays");
        assert!(after.contains("# a trailing note"));
        assert!(
            after.contains("args: [\"-y\", \"@sentry/mcp\"]"),
            "their flow sequence is not reformatted:\n{after}"
        );
        assert!(!after.contains("_config_version"), "we inject nothing");

        Hermes.uninstall(&e, Scope::Global, &mut Report::default());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "a user's real config must come back byte for byte"
        );
    }

    /// The measured hazard, end to end: Hermes rewrote the config and ate our
    /// markers. `status` must still see the entry and `uninstall` must still
    /// remove it, or we leave a live server pointing at a deleted binary.
    #[test]
    fn a_vendor_rewrite_that_strips_our_markers_does_not_orphan_the_entry() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".hermes/config.yaml");

        Hermes.install(&e, Scope::Global, &mut Report::default());
        // What `hermes mcp remove` actually leaves behind: no comments at all.
        let stripped: String = std::fs::read_to_string(&p)
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter(|l| !l.trim().is_empty())
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(&p, format!("{stripped}_config_version: 33\n")).unwrap();

        let mut r = Report::default();
        Hermes.status(&e, Scope::Global, &mut r);
        // `Action::Present` alone is not "must not lie": it is true of `current`
        // *and* of `stale`, so asserting it would pass whichever this reported.
        // The entry here is intact — only our comment markers went — so the one
        // right answer is `current`. Were it ever `stale`, every Hermes user
        // would carry a phantom that re-arms on each `hermes mcp` write and that
        // re-running install cannot clear: the unclearable alarm
        // [`crate::EntryState`] documents as the reason status compares the
        // entry rather than the file.
        assert_eq!(r.steps[0].action, Action::Present, "status must not lie");
        assert_eq!(
            r.steps[0].detail, "current",
            "the entry is intact and only our markers went, so this must be \
             `current`; `{}` is the phantom nobody can clear",
            r.steps[0].detail
        );

        let mut r = Report::default();
        Hermes.uninstall(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(!after.contains("filigrio"), "no orphan left:\n{after}");
        assert!(after.contains("_config_version: 33"), "not ours to remove");
    }

    /// The same exercise against a *real* config, opt-in and copied to scratch
    /// first: `FILIGRIO_REAL_HERMES_CONFIG=… cargo test`. The real file is never
    /// opened for writing — it is mode 0600 and holds credentials.
    #[test]
    fn a_real_hermes_config_round_trips_when_one_is_pointed_at() {
        let Some(src) = std::env::var_os("FILIGRIO_REAL_HERMES_CONFIG") else {
            return;
        };
        let prior = std::fs::read_to_string(&src)
            .unwrap_or_else(|e| panic!("FILIGRIO_REAL_HERMES_CONFIG is unreadable: {e}"));

        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".hermes/config.yaml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &prior).unwrap();

        let mut r = Report::default();
        Hermes.install(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);
        Hermes.uninstall(&e, Scope::Global, &mut r);
        assert!(r.is_ok(), "{:?}", r.failures);

        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            prior,
            "a real config did not come back byte-exact"
        );
    }

    /// `mcp_servers: {}` is legal YAML we refuse rather than re-emit.
    #[test]
    fn an_inline_mapping_is_a_reported_failure_and_the_file_survives() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".hermes/config.yaml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let prior = "mcp_servers: {}\nmodel: hermes-4-405b\n";
        std::fs::write(&p, prior).unwrap();

        let mut r = Report::default();
        Hermes.install(&e, Scope::Global, &mut r);
        assert!(!r.is_ok(), "a shape we will not rewrite is a named failure");
        assert!(r.failures[0].reason.contains("config.yaml"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// The credential-store precedent from [`super::openclaw`], measured again
    /// here: Hermes writes this file 0600 and our writer would not.
    #[cfg(unix)]
    #[test]
    fn a_config_we_create_is_owner_only_and_an_existing_ones_mode_is_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let p = home.path().join(".hermes/config.yaml");

        Hermes.install(&e, Scope::Global, &mut Report::default());
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "we created it (got {mode:o})");

        Hermes.uninstall(&e, Scope::Global, &mut Report::default());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "model: hermes-4-405b\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        Hermes.install(&e, Scope::Global, &mut Report::default());
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "an existing file's mode is not ours to change");
    }

    /// The three states a user reads, and the middle one is the whole point:
    /// a registration whose socket has moved is `Present` either way, so only
    /// the detail tells them apart. `status` said `registered` for both until
    /// this test existed.
    #[test]
    fn status_distinguishes_absent_current_and_stale() {
        let d = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let e = env(d.path(), home.path());
        let mut r = Report::default();
        Hermes.status(&e, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Absent);
        assert_eq!(r.steps[0].detail, "not registered");

        Hermes.install(&e, Scope::Global, &mut Report::default());
        let mut r = Report::default();
        Hermes.status(&e, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, "current");

        // The user re-ran with a different `--socket`. Nothing on disk changed;
        // what install *would* write did.
        let moved = Environment {
            socket_path: PathBuf::from("/run/user/1000/filigrio.sock"),
            ..e.clone()
        };
        let mut r = Report::default();
        Hermes.status(&moved, Scope::Global, &mut r);
        assert_eq!(r.steps[0].action, Action::Present);
        assert_eq!(r.steps[0].detail, STALE);

        // And the fix the message names actually works.
        Hermes.install(&moved, Scope::Global, &mut Report::default());
        let mut r = Report::default();
        Hermes.status(&moved, Scope::Global, &mut r);
        assert_eq!(r.steps[0].detail, "current");
    }
}
