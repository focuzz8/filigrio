//! The `filigrio hooks run` **interface contract** (ADR-0032b §4 + ADR-0034's
//! generated hook scripts), exercised through the real binary.
//!
//! The hook scripts `filigrio hooks install` writes call this verb and
//! nothing else, so its shape is fixed by agreement:
//!
//! ```text
//! filigrio hooks run <event> [git's own hook arguments…]
//! ```
//!
//! …passing git's argv positionally and unmodified, reading stdin where git
//! uses stdin, inferring the repository from the working directory, honouring
//! `FILIGRIO_SKIP_HOOK`, and **always exiting 0**. Each of those is one test
//! below, run against the built binary rather than the library, because a
//! contract with a shell script is a contract about a *process*.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

struct Scratch {
    _dir: TempDir,
    root: PathBuf,
    cache: PathBuf,
    /// A socket path with nothing listening on it.
    dead_socket: PathBuf,
}

impl Scratch {
    /// A one-commit repository, plus a cache dir the spool will land in.
    fn repo() -> Scratch {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("work");
        std::fs::create_dir_all(&root).expect("mkdir");
        let s = Scratch {
            cache: dir.path().join("cache"),
            dead_socket: dir.path().join("no-daemon.sock"),
            root,
            _dir: dir,
        };
        s.git(&["init", "-q", "-b", "main", "."]);
        s.git(&["config", "user.email", "hook@test"]);
        s.git(&["config", "user.name", "Hook Test"]);
        s.git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(s.root.join("a.rs"), "fn a() {}\n").expect("write");
        s.git(&["add", "-A"]);
        s.git(&["commit", "-q", "-m", "root"]);
        s
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.root)
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

    fn commit(&self, name: &str) {
        std::fs::write(self.root.join(name), "fn added() {}\n").expect("write");
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", name]);
    }

    /// `filigrio --socket <dead> hooks run <args…>` from the repo root.
    fn hook(&self, args: &[&str]) -> Output {
        self.hook_with(args, &[], None)
    }

    fn hook_with(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_filigrio"));
        cmd.arg("--socket")
            .arg(&self.dead_socket)
            .args(["hooks", "run"])
            .args(args)
            .current_dir(&self.root)
            .env("XDG_CACHE_HOME", &self.cache)
            .env_remove("FILIGRIO_SKIP_HOOK")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn filigrio");
        {
            use std::io::Write;
            let mut pipe = child.stdin.take().expect("stdin pipe");
            if let Some(body) = stdin {
                pipe.write_all(body.as_bytes()).expect("write stdin");
            }
            // Dropping closes it — a hook that reads stdin must see EOF.
        }
        child.wait_with_output().expect("wait")
    }

    /// `filigrio --socket <dead> <resource> <args…>` from the repo root, with
    /// `HOME`/`XDG_*` pinned inside the scratch tree so a status run cannot read
    /// or write the developer's real config.
    fn installer(&self, resource: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_filigrio"))
            .arg("--socket")
            .arg(&self.dead_socket)
            .arg(resource)
            .args(args)
            .current_dir(&self.root)
            .env("HOME", self.cache.parent().expect("scratch root"))
            .env("XDG_CACHE_HOME", &self.cache)
            .env("XDG_CONFIG_HOME", self.cache.join("config"))
            .env("XDG_DATA_HOME", self.cache.join("data"))
            .output()
            .expect("run the installer")
    }

    fn spool_dir(&self) -> PathBuf {
        self.cache.join("filigrio").join("spool")
    }

    fn spooled(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.spool_dir()) else {
            return Vec::new();
        };
        let mut v: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        v.sort();
        v
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// §4 rung two, through the process: **no daemon ⇒ the changeset is spooled and
/// the hook exits 0**. A `git commit` must survive a daemon that was never
/// started.
#[test]
fn a_missing_daemon_spools_the_changeset_and_still_exits_zero() {
    let s = Scratch::repo();
    s.commit("b.rs");

    let out = s.hook(&["post-commit"]);
    assert!(
        out.status.success(),
        "exit status was {:?}\n{}",
        out.status.code(),
        stderr(&out)
    );

    let spooled = s.spooled();
    assert_eq!(spooled.len(), 1, "expected one spooled job: {spooled:?}");
    let job: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&spooled[0]).expect("read job")).expect("parse job");
    assert_eq!(job["event"], "post-commit");
    assert_eq!(job["changeset"]["added"][0], "b.rs");
    assert_eq!(
        job["project"],
        s.root.to_string_lossy().as_ref(),
        "the job is addressed to the worktree root"
    );
    assert!(
        stderr(&out).contains("spooled"),
        "the fallback must be visible: {}",
        stderr(&out)
    );
}

/// The opt-out, spelled as the oracle spells it: **no work, no spool, exit 0**.
/// A skipped hook must not even shell out to git.
#[test]
fn skip_hook_makes_the_verb_a_silent_no_op() {
    let s = Scratch::repo();
    s.commit("b.rs");

    let out = s.hook_with(&["post-commit"], &[("FILIGRIO_SKIP_HOOK", "1")], None);
    assert!(out.status.success());
    assert_eq!(stderr(&out), "", "a skip says nothing");
    assert!(out.stdout.is_empty());
    assert!(
        s.spooled().is_empty(),
        "a skip must not spool: {:?}",
        s.spooled()
    );
    assert!(
        !s.spool_dir().exists(),
        "a skip must not even create the spool dir"
    );
}

/// Every runtime path exits 0 — a hook cannot be allowed to fail a git
/// operation (§4). These are the four ways it can go wrong at once: an unknown
/// event, a missing repository, a nonsense argument, and a dead socket.
#[test]
fn every_failure_mode_still_exits_zero() {
    let s = Scratch::repo();
    s.commit("b.rs");

    let unknown = s.hook(&["pre-commit"]);
    assert!(unknown.status.success(), "unknown event must exit 0");
    assert!(stderr(&unknown).contains("unknown hook event"));

    let junk = s.hook(&["post-checkout", "not-a-sha", "also-not-a-sha", "1"]);
    assert!(junk.status.success(), "bad revisions must exit 0");
    assert!(stderr(&junk).contains("git said"), "{}", stderr(&junk));

    // Outside a repository entirely.
    let outside = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_filigrio"))
        .arg("--socket")
        .arg(&s.dead_socket)
        .args(["hooks", "run", "post-commit"])
        .current_dir(outside.path())
        .env("XDG_CACHE_HOME", &s.cache)
        .env_remove("FILIGRIO_SKIP_HOOK")
        // `git rev-parse` walks *upwards*; a ceiling stops it escaping the
        // temp dir into whatever repository happens to contain $TMPDIR.
        .env("GIT_CEILING_DIRECTORIES", outside.path())
        .output()
        .expect("run outside a repo");
    assert!(out.status.success(), "a non-repository must exit 0");
}

/// **Nothing ever goes to stdout.** Hook output is interleaved into the git
/// command's own, and this CLI's stdout is its data channel
/// (`filigrio graph … | jq`).
#[test]
fn diagnostics_go_to_stderr_and_stdout_stays_empty() {
    let s = Scratch::repo();
    s.commit("b.rs");

    for args in [
        vec!["post-commit"],
        vec!["pre-commit"],
        vec!["post-checkout", "HEAD", "HEAD", "1"],
    ] {
        let out = s.hook(&args);
        assert!(
            out.stdout.is_empty(),
            "`hook {args:?}` wrote to stdout: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// `post-rewrite` is the one event git feeds on **stdin**, and the verb must
/// read it from there — the rewrite pairs are the only way to learn the
/// pre-rewrite tip.
#[test]
fn post_rewrite_reads_its_pairs_from_stdin() {
    let s = Scratch::repo();
    let before = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&s.root)
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .trim()
    .to_string();
    s.commit("b.rs");
    let after = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&s.root)
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .trim()
    .to_string();

    let out = s.hook_with(
        &["post-rewrite", "amend"],
        &[],
        Some(&format!("{before} {after}\n")),
    );
    assert!(out.status.success());
    assert!(
        stderr(&out).contains("1 added"),
        "the pair on stdin must drive the diff: {}",
        stderr(&out)
    );

    // No pairs at all is a clean no-op, not a hang and not an error.
    let empty = s.hook_with(&["post-rewrite", "rebase"], &[], Some(""));
    assert!(empty.status.success());
    assert!(
        stderr(&empty).contains("no rewrite pairs"),
        "{}",
        stderr(&empty)
    );
}

/// The other three events must **not** read stdin. Git leaves a hook's stdin
/// attached to whatever the invoking process had; a verb that read it
/// unconditionally would hang `git commit` on an open pipe forever. Proven by
/// handing it a pipe that is never closed.
#[test]
fn the_non_stdin_events_do_not_block_on_an_open_pipe() {
    let s = Scratch::repo();
    s.commit("b.rs");

    let mut child = Command::new(env!("CARGO_BIN_EXE_filigrio"))
        .arg("--socket")
        .arg(&s.dead_socket)
        .args(["hooks", "run", "post-commit"])
        .current_dir(&s.root)
        .env("XDG_CACHE_HOME", &s.cache)
        .env_remove("FILIGRIO_SKIP_HOOK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    // Deliberately hold the write end open for the whole run.
    let held = child.stdin.take().expect("stdin");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success());
                break;
            }
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                panic!("`hook post-commit` blocked on an open stdin pipe");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    drop(held);
}

/// Git's arguments are passed **positionally, verbatim** — including the
/// leading-`0`/`1` flags, which must not be mistaken for options. The verb also
/// tolerates git handing it more arguments than it knows about.
#[test]
fn gits_own_arguments_are_accepted_positionally_and_verbatim() {
    let s = Scratch::repo();
    s.commit("b.rs");

    // post-merge's single squash flag, post-checkout's triple, and a
    // hypothetical future argument git might append.
    for args in [
        vec!["post-merge", "0"],
        vec!["post-checkout", "HEAD", "HEAD", "1"],
        vec!["post-merge", "0", "some-future-argument"],
    ] {
        let out = s.hook(&args);
        assert!(
            out.status.success(),
            "`hook {args:?}` exited {:?}: {}",
            out.status.code(),
            stderr(&out)
        );
        assert!(
            !stderr(&out).contains("unexpected argument"),
            "`hook {args:?}` must not be parsed as options: {}",
            stderr(&out)
        );
    }
}

/// Rung one *replaces* rung two, and the next run drains what the last one
/// spooled. This is the whole retry mechanism: no replay verb, no retry daemon
/// — the next hook to fire is the retry.
#[test]
fn the_next_hook_run_replays_what_an_earlier_one_spooled() {
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    let s = Scratch::repo();
    s.commit("b.rs");
    s.hook(&["post-commit"]);
    assert_eq!(s.spooled().len(), 1, "the daemon was down");

    // Bring a listener up on the socket the CLI is pointed at.
    let listener = UnixListener::bind(&s.dead_socket).expect("bind");
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let Ok((mut conn, _)) = listener.accept() else {
                break;
            };
            let mut len = [0u8; 4];
            if conn.read_exact(&mut len).is_err() {
                break;
            }
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            if conn.read_exact(&mut body).is_err() {
                break;
            }
            let v: serde_json::Value = serde_json::from_slice(&body).expect("frame is json");
            seen.push(v);
        }
        seen
    });

    s.commit("c.rs");
    let out = s.hook(&["post-commit"]);
    assert!(out.status.success());
    assert!(
        s.spooled().is_empty(),
        "the backlog must be drained: {:?}",
        s.spooled()
    );
    assert!(
        stderr(&out).contains("replayed 1"),
        "the replay must be visible: {}",
        stderr(&out)
    );

    let seen = handle.join().expect("listener thread");
    assert_eq!(seen.len(), 2, "the backlog first, then the new changeset");
    assert_eq!(seen[0]["changeset"]["added"][0], "b.rs");
    assert_eq!(seen[1]["changeset"]["added"][0], "c.rs");
}

// ---------------------------------------------------------------------------
// ADR-0032b OQ4 — the hook-target section of `filigrio hooks status`.
// ---------------------------------------------------------------------------

/// `hooks status` answers OQ4's question: which project id the hooks submit
/// under, and whether anything receives it. Installing the scripts proves the
/// *files* are in place; only this says whether the changesets they send reach
/// anything.
#[test]
fn hooks_status_reports_the_hook_target_and_the_daemon() {
    let s = Scratch::repo();
    let out = s.installer("hooks", &["status"]);
    assert!(
        out.status.success(),
        "status exited {:?}: {}",
        out.status.code(),
        stderr(&out)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("git hooks submit under:"),
        "the target must be named: {stdout}"
    );
    assert!(
        stdout.contains(s.root.to_string_lossy().as_ref()),
        "the target is the worktree root: {stdout}"
    );
    // No daemon on the dead socket, so the honest answer is "unknown, and the
    // hook would spool" — not a failure, and not silence.
    assert!(stdout.contains("daemon:   not running"), "{stdout}");
    assert!(stdout.contains("receiver: unknown"), "{stdout}");
    assert!(stdout.contains("spool:"), "{stdout}");
}

/// The probe belongs to the **hooks** command and to **status**. The other two
/// resources must not drag in a daemon round trip, and `install` is not asking a
/// question — a status probe there would be chatter on a verb whose job is to
/// write files.
///
/// ADR-0034 §17 strengthened the first half of this from a runtime check into a
/// structural one: the probe used to be gated on `families.contains(Hooks)`
/// inside a shared dispatch, and now lives in `run_hooks`, which no other
/// resource can reach. The assertion is unchanged, because a reader of the
/// output cannot tell which kind of guarantee they are getting and should not
/// have to.
#[test]
fn the_hook_target_probe_is_scoped_to_status_and_to_the_hooks_command() {
    let s = Scratch::repo();

    for other in [
        s.installer("completions", &["status"]),
        s.installer("agent", &["status"]),
    ] {
        assert!(
            !String::from_utf8_lossy(&other.stdout).contains("git hooks submit under:"),
            "only `filigrio hooks status` may probe the hook target"
        );
    }

    let install = s.installer("hooks", &["install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    assert!(
        !String::from_utf8_lossy(&install.stdout).contains("git hooks submit under:"),
        "install writes files; it does not interrogate the daemon"
    );
}

/// An unregistered repository must not make `hooks status` **fail**. A
/// repository you have not registered yet is not a broken installation — the
/// probe is a diagnostic about the daemon, not an artifact that was installed.
#[test]
fn an_unreceived_hook_target_is_reported_without_failing_the_status_run() {
    let s = Scratch::repo();
    let out = s.installer("hooks", &["status"]);
    assert!(
        out.status.success(),
        "a diagnostic must not change the exit status"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("git hooks submit under:"));
}

/// The verb is discoverable from `--help`, spelled exactly as the generated
/// hook scripts call it. A rename here silently breaks every installed hook.
#[test]
fn the_verb_is_spelled_hooks_run_and_documents_the_four_events() {
    let out = Command::new(env!("CARGO_BIN_EXE_filigrio"))
        .args(["hooks", "run", "--help"])
        .output()
        .expect("run --help");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for event in ["post-commit", "post-checkout", "post-merge", "post-rewrite"] {
        assert!(help.contains(event), "`hooks run --help` must name {event}");
    }
    assert!(help.contains("FILIGRIO_SKIP_HOOK"));
}
