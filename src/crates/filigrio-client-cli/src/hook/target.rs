//! "Which project id will my hooks submit under, and is anything registered to
//! receive it?" — ADR-0032b OQ4.
//!
//! Delivery is fire-and-forget (see [`super::deliver`]): the hook writes a
//! framed `Submit` and hangs up, so it never learns that the daemon declined.
//! That is the right trade at *commit* time — §4 forbids blocking — but it left
//! two silences:
//!
//! 1. **An unregistered project.** Every commit submits, every submission is
//!    declined, the developer sees nothing, the index drifts.
//! 2. **An individually-registered monorepo.** The hook submits the
//!    **repository root**; the daemon resolves an exact project id or the
//!    nearest **ancestor** root (`find_by_path` is `path.starts_with(root)`).
//!    A sub-project registered at `repo/packages/web` is therefore a
//!    *descendant* of what the hook sends, not an ancestor of it, so it can
//!    never receive the changeset. Nothing said so.
//!
//! Both are the same question, and it is a question a developer should ask
//! **once**, not on every commit. So this is a *status* surface: a probe that
//! resolves the hook's target the same way the hook does, asks the daemon
//! whether anything answers to it, and reports. Nothing here runs from a hook,
//! and nothing here changes what a hook does.
//!
//! The probe deliberately **never auto-starts the daemon** — "is a daemon
//! running?" is a question, and a question that starts one to answer it is not
//! a question (the same rule `attach_to_daemon` follows in the CLI).

use super::git::Git;
use super::spool::SpoolDir;
use filigrio_protocol::{DaemonClientTrait, ProjectStatus, SocketClient};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A status probe must be snappy: it is answering a question, not doing work.
/// `Status` is a data-plane read served from resident state, so a daemon that
/// cannot answer within this is a daemon worth reporting as wedged.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the daemon says about the hook's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registration {
    /// The working directory is not inside a git worktree, so no hook will ever
    /// fire here.
    NoRepository,
    /// Nothing is listening on the socket. Hooks still work — they spool, and
    /// the next hook run replays (§4) — so this is not a failure.
    DaemonDown,
    /// The daemon resolved the target. `id` is the project that will actually
    /// receive the changeset, which is **not** always the path submitted:
    /// resolution matches an exact id or the nearest ancestor root.
    Registered {
        id: String,
        watching: bool,
        file_count: usize,
        node_count: usize,
        dirty: bool,
    },
    /// The daemon is up and resolves nothing for the target — every hook-driven
    /// changeset for this repository is being declined.
    Unregistered { daemon_said: String },
}

impl Registration {
    fn from_status(status: ProjectStatus) -> Registration {
        Registration::Registered {
            id: status.project,
            watching: status.watching,
            file_count: status.file_count,
            node_count: status.node_count,
            dirty: status.dirty,
        }
    }
}

/// The answer to OQ4's question for one working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookTarget {
    /// The project path a hook fired here would submit under — the worktree
    /// root, resolved exactly as [`super::run`] resolves it. `None` when there
    /// is no worktree.
    pub project: Option<PathBuf>,
    pub registration: Registration,
    /// Undelivered changesets currently on disk.
    pub spooled: usize,
    /// The socket the hook would try first.
    pub socket: PathBuf,
}

impl HookTarget {
    /// Resolve the target and ask the daemon about it.
    ///
    /// Read-only and side-effect-free: it discovers the worktree, counts the
    /// spool, and sends at most one data-plane `Status`. It registers nothing,
    /// starts nothing, and writes nothing.
    pub fn probe(cwd: &Path, socket: &Path, spool: &SpoolDir) -> HookTarget {
        let spooled = spool.jobs().len();
        let socket = socket.to_path_buf();

        let Some(git) = Git::discover(cwd) else {
            return HookTarget {
                project: None,
                registration: Registration::NoRepository,
                spooled,
                socket,
            };
        };
        let project = git.root().to_path_buf();

        let client = SocketClient::new(&socket).with_timeout(PROBE_TIMEOUT);
        let registration = if !client.is_reachable() {
            Registration::DaemonDown
        } else {
            match client.project_status(project.to_string_lossy().into_owned()) {
                Ok(status) => Registration::from_status(status),
                Err(e) => Registration::Unregistered {
                    daemon_said: plain_daemon_message(&e.to_string()),
                },
            }
        };

        HookTarget {
            project: Some(project),
            registration,
            spooled,
            socket,
        }
    }

    /// Whether hook-driven freshness is actually reaching a project right now.
    ///
    /// Only [`Registration::Registered`] is `true`. `DaemonDown` is
    /// deliberately **not** healthy-but-also-not-broken-enough-to-mention: the
    /// spool covers it, but a developer asking this question wants to know.
    pub fn is_delivering(&self) -> bool {
        matches!(self.registration, Registration::Registered { .. })
    }

    /// The human report. Returned as lines so the caller owns the prefixing and
    /// the stream; `filigrio hooks status` prints them to stdout, after the
    /// artifact lines and before the summary.
    pub fn report(&self) -> Vec<String> {
        let mut out = Vec::new();

        let Some(project) = &self.project else {
            out.push("git hooks: no worktree here — a hook fired in this directory would".into());
            out.push("           find no repository and do nothing.".into());
            return out;
        };

        out.push(format!("git hooks submit under: {}", project.display()));

        match &self.registration {
            // `probe` never builds this pairing (a target with a path but no
            // repository), but the types permit it and a panic in a *status*
            // command would be absurd — ADR-0041's rule against asserted
            // infallibility applies to `unreachable!` exactly as it does to
            // `unwrap`.
            Registration::NoRepository => {
                out.push("  receiver: unknown — no repository resolved for this path".into());
            }
            Registration::DaemonDown => {
                out.push(format!(
                    "  daemon:   not running ({})",
                    self.socket.display()
                ));
                out.push(
                    "  receiver: unknown until the daemon is up — hooks spool while it is down"
                        .into(),
                );
                out.push("            and the next hook run replays them (ADR-0032b §4).".into());
            }
            Registration::Registered {
                id,
                watching,
                file_count,
                node_count,
                dirty,
            } => {
                out.push(format!("  daemon:   running ({})", self.socket.display()));
                out.push(format!(
                    "  receiver: project `{id}` — {file_count} file(s), {node_count} node(s){}{}",
                    if *watching { ", watching" } else { "" },
                    if *dirty { ", unflushed" } else { "" }
                ));
                // The resolved id is the point of printing it: resolution
                // matches the nearest ancestor root, so a hook fired deep in a
                // tree can legitimately land on a project whose name looks
                // nothing like the path.
            }
            Registration::Unregistered { daemon_said } => {
                out.push(format!("  daemon:   running ({})", self.socket.display()));
                out.push(format!(
                    "  receiver: NONE — nothing is registered at or above {}",
                    project.display()
                ));
                out.push(format!("            (daemon: {daemon_said})"));
                out.push(
                    "            Every hook-driven changeset for this repository is declined,"
                        .into(),
                );
                out.push(
                    "            silently — delivery is fire-and-forget — so the index".into(),
                );
                out.push("            drifts until an explicit `filigrio project index`.".into());
                out.push(format!(
                    "            → fix: `filigrio project register` from {}",
                    project.display()
                ));
                out.extend(monorepo_note());
            }
        }

        out.push(match self.spooled {
            0 => "  spool:    empty".to_string(),
            n => format!("  spool:    {n} changeset(s) undelivered, replayed by the next hook"),
        });
        out
    }
}

/// Strip the transport's framing from a daemon-authored message.
///
/// A daemon that *answers* `Response::Error { message }` is decoded by the
/// client into `Error::Socket(message)`, whose `Display` prepends
/// `"socket error: "`. Quoting that verbatim tells the developer their registry
/// miss was a **socket fault**, which is the opposite of true and sends them
/// debugging the wrong layer. The daemon reached us; only its answer was no.
fn plain_daemon_message(raw: &str) -> String {
    raw.strip_prefix("socket error: ")
        .unwrap_or(raw)
        .to_string()
}

/// The monorepo half of OQ4, printed whenever the target resolves to nothing.
///
/// It is printed **unconditionally** in that case rather than only when
/// sub-projects are known to exist, because the client cannot enumerate the
/// registry — there is no list query on the data plane, and inventing one to
/// decorate a diagnostic would be a wire surface bought for a sentence. What
/// matters is that the failure mode is *named* where a developer hits it: the
/// mechanism (root submitted, ancestor matched) is exact, checkable, and true
/// whether or not this particular repository is a monorepo.
fn monorepo_note() -> Vec<String> {
    vec![
        "            → if this is a monorepo whose sub-projects you registered".to_string(),
        "              individually: the hook submits the REPOSITORY ROOT, and the".to_string(),
        "              daemon matches an exact project id or the nearest ANCESTOR".to_string(),
        "              root. A project registered *below* this path is a descendant".to_string(),
        "              of what the hook sends, so it can never receive it.".to_string(),
        "              Per-subproject hook routing is not built (ADR-0032b);".to_string(),
        "              register the repository root to get hook-driven freshness.".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_protocol::{DataQuery, Request, Response};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    fn target(registration: Registration, spooled: usize) -> HookTarget {
        HookTarget {
            project: Some(PathBuf::from("/repo")),
            registration,
            spooled,
            socket: PathBuf::from("/run/filigrio.sock"),
        }
    }

    fn joined(t: &HookTarget) -> String {
        t.report().join("\n")
    }

    /// A unique socket path that removes itself on drop, **including on panic**.
    ///
    /// Not in a `TempDir`: a unix socket path is capped at ~108 bytes
    /// (`SUN_LEN`), and this repository's scratch trees are long enough to blow
    /// that — a real bind failure I hit standing up the end-to-end daemon. So
    /// the path is short and lives in `/tmp`, which makes cleanup this type's
    /// job rather than the directory's. An earlier version cleaned up *after*
    /// `join()`, so the two runs that panicked before reaching it leaked
    /// sockets into `/tmp`; `Drop` runs during unwinding, this does not.
    struct TestSocket(PathBuf);

    impl TestSocket {
        fn new(tag: &str) -> TestSocket {
            use std::sync::atomic::{AtomicU32, Ordering};
            static SEQ: AtomicU32 = AtomicU32::new(0);
            // pid + sequence: unique across concurrent runs *and* across the
            // several tests sharing one test binary's process.
            let path = PathBuf::from("/tmp").join(format!(
                "gfy-{tag}-{}-{}.sock",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);
            TestSocket(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A fake daemon that answers the first real `Status` with `response` and
    /// hands back the project string it was asked about. Fake rather than real
    /// because this crate must stay engine-free — the *other* side of the seam
    /// (`find_by_path`'s ancestor semantics) is pinned by `filigrio-daemon`'s
    /// own `test_find_by_path`.
    ///
    /// It **loops over connections** rather than accepting once, because a real
    /// daemon does: `SocketClient::is_reachable` connects and immediately drops,
    /// so the probe's liveness check arrives as its own empty connection before
    /// the request does. A single-accept fake would answer the liveness check
    /// and never see the query — which is exactly the bug this loop caught.
    fn fake_daemon(socket: PathBuf, response: Response) -> std::thread::JoinHandle<String> {
        let listener = UnixListener::bind(&socket).expect("bind");
        std::thread::spawn(move || loop {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut len = [0u8; 4];
            if conn.read_exact(&mut len).is_err() {
                continue; // a liveness probe: connected and hung up
            }
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            conn.read_exact(&mut body).expect("body");
            let request: Request = serde_json::from_slice(&body).expect("request");
            let asked = match request {
                Request::Data(DataQuery::Status { project }) => project.unwrap_or_default(),
                other => panic!("the probe must send a data-plane Status, sent {other:?}"),
            };
            let out = filigrio_protocol::frame(&response).expect("frame");
            conn.write_all(&out).expect("write");
            return asked;
        })
    }

    fn repo(dir: &Path) -> PathBuf {
        let root = dir.join("work");
        std::fs::create_dir_all(&root).expect("mkdir");
        for args in [
            vec!["init", "-q", "-b", "main", "."],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git");
            assert!(out.status.success(), "{args:?}");
        }
        root
    }

    /// The probe asks the daemon about the **worktree root**, on the data
    /// plane. That is the whole contract with the daemon: it must be the same
    /// string the hook would submit under, or the answer is about a different
    /// question than the one asked.
    #[test]
    fn the_probe_asks_about_the_worktree_root_on_the_data_plane() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = repo(dir.path());
        let socket = TestSocket::new("probe");

        let handle = fake_daemon(
            socket.path().to_path_buf(),
            Response::QueryResult {
                data: serde_json::to_value(ProjectStatus {
                    project: "work".into(),
                    has_drift: false,
                    last_revision: None,
                    file_count: 12,
                    node_count: 40,
                    watching: true,
                    dirty: false,
                    dirty_for_secs: None,
                    last_persisted_secs_ago: None,
                })
                .expect("value"),
            },
        );

        // Probe from a SUBDIRECTORY: the hook resolves the worktree root, so
        // the probe must too — asking about the cwd would report on a path no
        // hook ever submits.
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).expect("mkdir");
        let spool = SpoolDir::new(dir.path().join("spool"));
        let t = HookTarget::probe(&deep, socket.path(), &spool);

        let asked = handle.join().expect("fake daemon");

        assert_eq!(
            std::fs::canonicalize(asked).expect("canonicalize"),
            std::fs::canonicalize(&root).expect("canonicalize"),
            "the probe must ask about the worktree root, not the cwd"
        );
        assert_eq!(
            t.registration,
            Registration::Registered {
                id: "work".into(),
                watching: true,
                file_count: 12,
                node_count: 40,
                dirty: false,
            }
        );
        assert!(t.is_delivering());
    }

    /// The daemon declining is the OQ4 case. It must come back as
    /// `Unregistered` carrying what the daemon actually said — never as a
    /// generic failure, and never swallowed.
    #[test]
    fn a_declined_target_is_unregistered_and_quotes_the_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = repo(dir.path());
        let socket = TestSocket::new("probe-no");

        let handle = fake_daemon(
            socket.path().to_path_buf(),
            Response::Error {
                message: "project not registered: '/somewhere'.".into(),
            },
        );
        let spool = SpoolDir::new(dir.path().join("spool"));
        let t = HookTarget::probe(&root, socket.path(), &spool);
        handle.join().expect("fake daemon");

        let Registration::Unregistered { daemon_said } = &t.registration else {
            panic!("expected Unregistered, got {:?}", t.registration);
        };
        assert!(
            daemon_said.contains("project not registered"),
            "{daemon_said}"
        );
        // A registry miss is not a transport fault. The client decodes an
        // answered `Response::Error` into `Error::Socket`, and quoting that
        // Display verbatim would tell the developer their *socket* failed —
        // sending them to debug the wrong layer entirely.
        assert!(
            !daemon_said.contains("socket error"),
            "the daemon answered; only its answer was no: {daemon_said}"
        );
        assert!(!t.is_delivering());
    }

    /// The transport prefix is stripped, and a message that never had one is
    /// left exactly as the daemon wrote it.
    #[test]
    fn a_daemon_message_keeps_its_own_words_and_loses_the_transport_framing() {
        assert_eq!(
            plain_daemon_message("socket error: project not registered: '/repo'."),
            "project not registered: '/repo'."
        );
        assert_eq!(
            plain_daemon_message("project not registered: '/repo'."),
            "project not registered: '/repo'."
        );
        // A genuine transport failure must NOT be laundered into looking like a
        // daemon answer — only the exact prefix is removed.
        assert_eq!(
            plain_daemon_message("I/O error: connection refused"),
            "I/O error: connection refused"
        );
    }

    /// No daemon is **not** a failure — the spool covers it — but it is
    /// reported, because "are my hooks working?" deserves a real answer.
    #[test]
    fn no_daemon_is_reported_without_starting_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = repo(dir.path());
        let socket = dir.path().join("nothing.sock");
        let spool = SpoolDir::new(dir.path().join("spool"));

        let t = HookTarget::probe(&root, &socket, &spool);
        assert_eq!(t.registration, Registration::DaemonDown);
        assert!(
            !socket.exists(),
            "the probe must never auto-start a daemon (nothing may bind the socket)"
        );
    }

    /// Outside a worktree there is no target at all, and the report says so
    /// rather than inventing a project.
    #[test]
    fn outside_a_worktree_there_is_no_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = SpoolDir::new(dir.path().join("spool"));
        let t = HookTarget::probe(dir.path(), &dir.path().join("x.sock"), &spool);
        assert_eq!(t.project, None);
        assert_eq!(t.registration, Registration::NoRepository);
        assert!(joined(&t).contains("no worktree"));
    }

    /// The spool depth is part of the answer: "3 changesets undelivered" is a
    /// direct answer to "are my hooks reaching anything?".
    #[test]
    fn the_spool_depth_is_counted_and_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = repo(dir.path());
        let spool = SpoolDir::new(dir.path().join("spool"));
        for n in 0..3 {
            spool
                .push(&super::super::SpooledJob::new(
                    "/repo".into(),
                    "post-commit",
                    filigrio_protocol::ChangeSet::all_added([format!("{n}.rs")]),
                ))
                .expect("push");
        }
        let t = HookTarget::probe(&root, &dir.path().join("nothing.sock"), &spool);
        assert_eq!(t.spooled, 3);
        assert!(joined(&t).contains("3 changeset(s) undelivered"));

        let empty = HookTarget::probe(
            &root,
            &dir.path().join("nothing.sock"),
            &SpoolDir::new(dir.path().join("other")),
        );
        assert!(joined(&empty).contains("spool:    empty"));
    }

    /// The **headline** of OQ4: a declined target must name the monorepo
    /// mechanism exactly — root submitted, ancestor matched, descendant cannot
    /// receive — and say that per-subproject routing is not built. A vague
    /// "not registered" would leave the sophisticated user exactly where they
    /// were.
    #[test]
    fn a_declined_report_names_the_monorepo_mechanism_and_the_fix() {
        let text = joined(&target(
            Registration::Unregistered {
                daemon_said: "project not registered: '/repo'.".into(),
            },
            0,
        ));

        assert!(text.contains("receiver: NONE"), "{text}");
        // The mechanism, not just the symptom.
        assert!(text.contains("REPOSITORY ROOT"), "{text}");
        assert!(text.contains("ANCESTOR"), "{text}");
        assert!(text.contains("descendant"), "{text}");
        // The consequence, stated plainly (this is the silence being closed).
        assert!(text.contains("declined"), "{text}");
        assert!(text.contains("drifts"), "{text}");
        // Both fixes: the general one and the monorepo one.
        assert!(text.contains("filigrio project register"), "{text}");
        assert!(text.contains("not built"), "{text}");
        assert!(text.contains("ADR-0032b"), "{text}");
    }

    /// A healthy target reports the **resolved id**, which is the thing the
    /// developer cannot otherwise see: resolution matches the nearest ancestor
    /// root, so the receiving project's name need not resemble the path.
    #[test]
    fn a_healthy_report_names_the_resolved_project_id() {
        let text = joined(&target(
            Registration::Registered {
                id: "web".into(),
                watching: true,
                file_count: 412,
                node_count: 8134,
                dirty: false,
            },
            0,
        ));
        assert!(text.contains("submit under: /repo"), "{text}");
        assert!(text.contains("project `web`"), "{text}");
        assert!(text.contains("412 file(s)"), "{text}");
        assert!(text.contains("watching"), "{text}");
        assert!(
            !text.contains("monorepo"),
            "a working setup must not be lectured: {text}"
        );
    }

    /// Every report answers both halves of OQ4's question — *which id*, and
    /// *is anything receiving* — in every reachable state. A state that
    /// answered only one would be a new silence.
    #[test]
    fn every_state_answers_both_halves_of_the_question() {
        let states = [
            Registration::DaemonDown,
            Registration::Unregistered {
                daemon_said: "project not registered: '/repo'.".into(),
            },
            Registration::Registered {
                id: "repo".into(),
                watching: false,
                file_count: 1,
                node_count: 2,
                dirty: true,
            },
        ];
        for state in states {
            let text = joined(&target(state.clone(), 0));
            assert!(
                text.contains("submit under: /repo"),
                "{state:?} did not say which id: {text}"
            );
            assert!(
                text.contains("receiver:"),
                "{state:?} did not say whether anything receives it: {text}"
            );
            assert!(
                text.contains("spool:"),
                "{state:?} did not report the spool: {text}"
            );
        }
    }
}
