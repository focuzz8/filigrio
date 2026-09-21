//! The delivery ladder (ADR-0032b §4): **socket → spool → drop**.
//!
//! ## Why the socket write is fire-and-forget
//!
//! `Submit` executes *synchronously at ingress* (ADR-0042 F6c) — the daemon
//! applies the changeset and only then writes the response. Waiting for that
//! response would put the whole apply (measured ≈2.5 s on this repo, ≈3 s at
//! next.js scale — `docs/perf/benchmarks.md` §5d) inside `git commit`, which is
//! precisely what §4 forbids: *"The hook must never gate or slow the git
//! operation."*
//!
//! So the hook writes the framed request and hangs up without reading. The
//! daemon's `handle_conn` reads the frame, runs the apply, and fails only on
//! the final `write_frame` to a closed peer — the work is already done, and
//! Rust's runtime ignores `SIGPIPE`, so a hung-up client cannot kill it. The
//! whole hook costs one `connect(2)` and one small `write(2)`.
//!
//! What that costs us is the *outcome*: the hook cannot tell "applied" from
//! "project not registered". That is the honest price of §4's best-effort
//! contract, and it is why the ladder branches on **connect**, not on the
//! response — connect failing is exactly "no daemon here".

use crate::hook::spool::{SpoolDir, SpooledJob};
use filigrio_protocol::{ChangeSet, Command, Priority, Request, MAX_FRAME};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A hook must not sit on a write either. The daemon reads its frame
/// immediately; if a socket buffer is full for this long, something is wrong
/// enough that the spool is the better answer.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Which rung of the ladder the changeset came to rest on.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Handed to the daemon over the socket.
    Daemon,
    /// The daemon was unreachable; the job is on disk for the next hook run.
    Spooled(PathBuf),
    /// Neither worked. §4: the index drifts until [0032c]'s reconcile heals it.
    Dropped(String),
}

impl Delivery {
    pub fn describe(&self) -> String {
        match self {
            Delivery::Daemon => "submitted to the daemon".to_string(),
            Delivery::Spooled(p) => format!("daemon unreachable — spooled at {}", p.display()),
            Delivery::Dropped(why) => {
                format!("dropped ({why}); `filigrio project index` will heal it")
            }
        }
    }
}

/// Build the `Submit` for a changeset. `Priority::Git` is ADR-0032b §1's high
/// lane — above `fs-event`, below `manual`.
fn submit(project: &str, changeset: ChangeSet) -> Request {
    Request::command(Command::Submit {
        project: project.to_string(),
        changeset,
        priority: Priority::Git,
    })
}

/// Write one framed request and hang up. `Ok(())` means the bytes reached the
/// daemon's socket buffer — never that the apply succeeded (see the module
/// header).
fn fire_and_forget(socket: &Path, request: &Request) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let bytes = filigrio_protocol::frame(request).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_FRAME {
            return Err(format!(
                "changeset frames to {} bytes, over the {MAX_FRAME}-byte wire limit",
                bytes.len()
            ));
        }
        let mut stream = UnixStream::connect(socket).map_err(|e| format!("connect: {e}"))?;
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .map_err(|e| format!("set timeout: {e}"))?;
        stream
            .write_all(&bytes)
            .map_err(|e| format!("write: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, request);
        Err("unix-domain sockets only".to_string())
    }
}

/// Run the ladder for one changeset.
pub fn deliver(
    socket: &Path,
    spool: &SpoolDir,
    project: &str,
    event: &str,
    changeset: ChangeSet,
) -> Delivery {
    let request = submit(project, changeset.clone());
    match fire_and_forget(socket, &request) {
        Ok(()) => Delivery::Daemon,
        Err(socket_err) => {
            let job = SpooledJob::new(project.to_string(), event, changeset);
            match spool.push(&job) {
                Ok(path) => Delivery::Spooled(path),
                Err(spool_err) => Delivery::Dropped(format!(
                    "socket {socket_err}; spool {}",
                    root_cause(&spool_err)
                )),
            }
        }
    }
}

fn root_cause(err: &anyhow::Error) -> String {
    err.chain()
        .last()
        .map(|c| c.to_string())
        .unwrap_or_else(|| err.to_string())
}

/// What one replay pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Replay {
    /// Jobs handed to the daemon.
    pub delivered: usize,
    /// Jobs discarded because this build cannot read them (corrupt, or a newer
    /// on-disk format). Removed rather than retried — an undeliverable job must
    /// not wedge the ones behind it.
    pub discarded: usize,
    /// Jobs left on disk because the daemon went away mid-pass.
    pub remaining: usize,
}

/// Replay the spool, oldest first, stopping at the first socket failure.
///
/// This is the whole "replayed later" half of §4, and it needs no scheduler:
/// the next hook that fires *is* the retry, and a repository with a hook
/// installed fires one on every commit. Stopping at the first failure keeps
/// delivery ordered and avoids hammering a socket that is not there.
pub fn replay(socket: &Path, spool: &SpoolDir) -> Replay {
    spool.sweep();
    let mut out = Replay::default();
    let jobs = spool.jobs();
    for (i, path) in jobs.iter().enumerate() {
        let job = match spool.read(path) {
            Ok(job) => job,
            Err(_) => {
                spool.remove(path);
                out.discarded += 1;
                continue;
            }
        };
        match fire_and_forget(socket, &submit(&job.project, job.changeset)) {
            Ok(()) => {
                spool.remove(path);
                out.delivered += 1;
            }
            Err(_) => {
                out.remaining = jobs.len() - i;
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn cs(path: &str) -> ChangeSet {
        ChangeSet::all_added([path.to_string()])
    }

    /// Rung two: no daemon ⇒ the changeset lands on disk, intact and readable.
    #[test]
    fn an_unreachable_daemon_spools_the_changeset() {
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path().join("spool"));
        let socket = dir.path().join("nothing.sock");

        let d = deliver(&socket, &spool, "/repo", "post-commit", cs("a.rs"));
        let Delivery::Spooled(path) = d else {
            panic!("expected a spooled delivery, got {d:?}");
        };
        let job = spool.read(&path).unwrap();
        assert_eq!(job.project, "/repo");
        assert_eq!(job.event, "post-commit");
        assert_eq!(job.changeset.added, vec!["a.rs"]);
    }

    /// Rung three: neither rung works (the spool path is a *file*, so
    /// `create_dir_all` cannot succeed) ⇒ dropped, with both failures named.
    /// §4 accepts this; ADR-0029 requires that it says so.
    #[test]
    fn a_dead_socket_and_an_unusable_spool_drop_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, b"in the way").unwrap();

        let d = deliver(
            &dir.path().join("nothing.sock"),
            &SpoolDir::new(&blocked),
            "/repo",
            "post-commit",
            cs("a.rs"),
        );
        let Delivery::Dropped(why) = &d else {
            panic!("expected a dropped delivery, got {d:?}");
        };
        assert!(why.contains("socket"), "got: {why}");
        assert!(why.contains("spool"), "got: {why}");
        assert!(d.describe().contains("project index"));
    }

    /// Rung one, end to end over a real unix socket: the hook writes a framed
    /// `Submit` and hangs up **without reading**, and the listener still gets
    /// the whole frame. This is the property the whole non-blocking design
    /// rests on — if a hung-up writer lost its bytes, the hook would be silently
    /// dropping every changeset.
    #[test]
    fn a_reachable_socket_receives_the_whole_frame_from_a_client_that_never_reads() {
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut len = [0u8; 4];
            conn.read_exact(&mut len).unwrap();
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            conn.read_exact(&mut body).unwrap();
            body
        });

        let spool = SpoolDir::new(dir.path().join("spool"));
        assert_eq!(
            deliver(&socket, &spool, "/repo", "post-commit", cs("a.rs")),
            Delivery::Daemon
        );
        assert!(
            spool.jobs().is_empty(),
            "a delivered changeset is not spooled"
        );

        let body = handle.join().unwrap();
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["type"], "Command");
        assert_eq!(request["kind"], "Submit");
        assert_eq!(request["project"], "/repo");
        assert_eq!(request["priority"], "Git", "hooks ride the high lane (§1)");
        assert_eq!(request["changeset"]["added"][0], "a.rs");
    }

    /// A changeset too large for the wire is dropped, not spooled: a job that
    /// can never be framed would be replayed forever.
    #[test]
    fn an_over_sized_changeset_is_reported_rather_than_spooled_forever() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        let huge = ChangeSet::all_added((0..400_000).map(|n| format!("src/generated/f{n}.rs")));
        let err = fire_and_forget(&socket, &submit("/repo", huge)).unwrap_err();
        assert!(err.contains("wire limit"), "got: {err}");
    }

    /// Replay is the retry: the next hook run drains what the last one spooled,
    /// in order.
    #[test]
    fn replay_drains_the_spool_in_order_once_the_daemon_is_back() {
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path().join("spool"));
        let dead = dir.path().join("dead.sock");
        for n in 0..3 {
            deliver(
                &dead,
                &spool,
                "/repo",
                "post-commit",
                cs(&format!("{n}.rs")),
            );
        }
        assert_eq!(spool.jobs().len(), 3);

        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let (mut conn, _) = listener.accept().unwrap();
                let mut len = [0u8; 4];
                conn.read_exact(&mut len).unwrap();
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                conn.read_exact(&mut body).unwrap();
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                seen.push(v["changeset"]["added"][0].as_str().unwrap().to_string());
            }
            seen
        });

        let r = replay(&socket, &spool);
        assert_eq!(
            r,
            Replay {
                delivered: 3,
                discarded: 0,
                remaining: 0
            }
        );
        assert!(spool.jobs().is_empty());
        assert_eq!(handle.join().unwrap(), vec!["0.rs", "1.rs", "2.rs"]);
    }

    /// A job this build cannot read is removed, not retried — otherwise it
    /// wedges every job behind it for the life of the installation.
    #[test]
    fn replay_discards_an_unreadable_job_instead_of_wedging_behind_it() {
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path().join("spool"));
        spool
            .push(&SpooledJob::new("/repo".into(), "post-commit", cs("a.rs")))
            .unwrap();
        std::fs::write(spool.path().join("000-0.json"), b"{corrupt").unwrap();

        // No daemon: the good job cannot go, but the corrupt one still goes away.
        let r = replay(&dir.path().join("nothing.sock"), &spool);
        assert_eq!(r.discarded, 1);
        assert_eq!(r.delivered, 0);
        assert_eq!(spool.jobs().len(), 1, "only the readable job is left");
    }

    /// An empty spool is the common case and must cost nothing and say nothing.
    #[test]
    fn replay_on_an_empty_spool_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path().join("spool"));
        assert_eq!(
            replay(&dir.path().join("nothing.sock"), &spool),
            Replay::default()
        );
    }
}
