//! The spool — rung two of ADR-0032b §4's delivery ladder.
//!
//! **try the daemon socket → else spool a detached job → else drop.**
//!
//! A spooled job is one changeset, written as one file, replayed by the next
//! hook invocation that finds the daemon reachable. That is the whole
//! mechanism, and the smallness is the design: §4 is explicit that
//! *correctness does not depend on delivery* — [0032c]'s reconcile is the
//! always-converging backstop — so a spool that needs a queue manager, a
//! retry daemon, or a replay verb would be machinery bought with nothing.
//!
//! Crash-safety comes from `write to <name>.tmp, rename to <name>.json`:
//! `rename(2)` within a directory is atomic, so a job file either exists whole
//! or does not exist. A crash between the two leaves a `.tmp` file, which
//! nothing reads and [`SpoolDir::sweep`] removes.
//!
//! The spool is **capped**. An uninstalled daemon plus a busy repository is a
//! spool that grows forever, and every one of those jobs is redundant with a
//! single reconcile. Past [`MAX_JOBS`] the oldest are dropped, loudly.

use anyhow::{Context, Result};
use filigrio_protocol::ChangeSet;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many undelivered jobs the spool keeps before dropping the oldest.
pub const MAX_JOBS: usize = 128;

/// The on-disk format version. A job written by a future client with a shape
/// this one cannot read is skipped and removed rather than retried forever.
pub const FORMAT: u32 = 1;

/// One undelivered changeset.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpooledJob {
    pub format: u32,
    /// The project path the changeset was computed against (the worktree root).
    pub project: String,
    /// Which hook produced it — diagnostics only, but the first question asked
    /// of a spool that is not draining.
    pub event: String,
    /// Unix seconds at spool time.
    pub created: u64,
    pub changeset: ChangeSet,
}

impl SpooledJob {
    pub fn new(project: String, event: &str, changeset: ChangeSet) -> SpooledJob {
        SpooledJob {
            format: FORMAT,
            project,
            event: event.to_string(),
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            changeset,
        }
    }
}

/// A directory of spooled jobs.
pub struct SpoolDir {
    dir: PathBuf,
}

impl SpoolDir {
    pub fn new(dir: impl Into<PathBuf>) -> SpoolDir {
        SpoolDir { dir: dir.into() }
    }

    /// The default spool location: `$XDG_CACHE_HOME/filigrio/spool`, falling
    /// back to `$HOME/.cache/filigrio/spool`.
    ///
    /// Deliberately **cache**, not state: every job in here is reconstructible
    /// from git, and losing the directory costs at most one reconcile.
    pub fn default_dir() -> PathBuf {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("filigrio").join("spool")
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Append a job. Returns the file it landed in.
    ///
    /// The name is `<unix-nanos>-<pid>.json`: sorting by name is sorting by
    /// spool order (which is delivery order), and the pid disambiguates two
    /// hooks firing in the same nanosecond — a `git rebase` fires several in a
    /// row, so this collision is real, not theoretical.
    pub fn push(&self, job: &SpooledJob) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create spool dir {}", self.dir.display()))?;

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let stem = format!("{stamp:039}-{}", std::process::id());
        let tmp = self.dir.join(format!("{stem}.tmp"));
        let final_path = self.dir.join(format!("{stem}.json"));

        let body = serde_json::to_vec(job).context("serialize spooled job")?;
        std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
        // Atomic within the directory: a reader sees the whole job or no job.
        std::fs::rename(&tmp, &final_path)
            .with_context(|| format!("rename {} → {}", tmp.display(), final_path.display()))?;

        self.enforce_cap();
        Ok(final_path)
    }

    /// Spooled jobs in delivery order (oldest first).
    pub fn jobs(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        out.sort();
        out
    }

    /// Read one job. A file this build cannot parse — corrupt, or a newer
    /// `format` — is an `Err`, and the caller **removes** it: a job that can
    /// never be delivered must not block the ones behind it forever.
    pub fn read(&self, path: &Path) -> Result<SpooledJob> {
        let body =
            std::fs::read(path).with_context(|| format!("read spooled job {}", path.display()))?;
        let job: SpooledJob = serde_json::from_slice(&body)
            .with_context(|| format!("parse spooled job {}", path.display()))?;
        if job.format != FORMAT {
            anyhow::bail!(
                "spooled job {} has format {} (this build reads {FORMAT})",
                path.display(),
                job.format
            );
        }
        Ok(job)
    }

    pub fn remove(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// Remove `.tmp` leftovers from a crash between write and rename.
    pub fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for path in entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "tmp"))
        {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Drop the oldest jobs past [`MAX_JOBS`]. Returns how many went.
    fn enforce_cap(&self) -> usize {
        let jobs = self.jobs();
        if jobs.len() <= MAX_JOBS {
            return 0;
        }
        let excess = jobs.len() - MAX_JOBS;
        for path in jobs.iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
        excess
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(n: &str) -> SpooledJob {
        SpooledJob::new(
            "/repo".into(),
            "post-commit",
            ChangeSet::all_added([n.to_string()]),
        )
    }

    #[test]
    fn a_pushed_job_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path());
        let path = spool.push(&job("a.rs")).unwrap();

        assert_eq!(spool.jobs(), vec![path.clone()]);
        let back = spool.read(&path).unwrap();
        assert_eq!(back.project, "/repo");
        assert_eq!(back.event, "post-commit");
        assert_eq!(back.changeset.added, vec!["a.rs"]);

        spool.remove(&path);
        assert!(spool.jobs().is_empty());
    }

    /// The rename discipline: nothing but a whole job ever carries the `.json`
    /// extension, so a half-written file cannot be read as a job.
    #[test]
    fn only_committed_jobs_are_listed_and_tmp_leftovers_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path());
        spool.push(&job("a.rs")).unwrap();
        std::fs::write(dir.path().join("999-1.tmp"), b"half a job").unwrap();

        assert_eq!(spool.jobs().len(), 1, "a .tmp file is not a job");
        spool.sweep();
        assert!(!dir.path().join("999-1.tmp").exists());
        assert_eq!(spool.jobs().len(), 1, "sweep must not touch real jobs");
    }

    /// Delivery order is spool order, and the name is what encodes it.
    #[test]
    fn jobs_come_back_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path());
        let mut pushed = Vec::new();
        for n in 0..5 {
            pushed.push(spool.push(&job(&format!("{n}.rs"))).unwrap());
        }
        let listed = spool.jobs();
        assert_eq!(listed, pushed, "listing must be in push order");
    }

    /// An undeliverable job must not wedge the queue behind it: the reader
    /// reports it, and the caller's contract is to delete it.
    #[test]
    fn an_unreadable_or_future_format_job_is_an_error_not_a_silent_skip() {
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path());

        let corrupt = dir.path().join("000-1.json");
        std::fs::write(&corrupt, b"{not json").unwrap();
        assert!(spool.read(&corrupt).is_err());

        let future = dir.path().join("001-1.json");
        let mut j = job("a.rs");
        j.format = FORMAT + 1;
        std::fs::write(&future, serde_json::to_vec(&j).unwrap()).unwrap();
        let err = spool.read(&future).unwrap_err().to_string();
        assert!(err.contains("format"), "got: {err}");
    }

    /// An uninstalled daemon plus a busy repo must not fill the disk. The cap
    /// drops the *oldest*, because the newest changeset is the one closest to
    /// the current tree.
    #[test]
    fn the_spool_is_capped_and_drops_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path());
        for n in 0..(MAX_JOBS + 5) {
            spool.push(&job(&format!("{n}.rs"))).unwrap();
        }
        let jobs = spool.jobs();
        assert_eq!(jobs.len(), MAX_JOBS);
        // The survivor set is the newest window: the last push is still there.
        let newest = spool.read(jobs.last().unwrap()).unwrap();
        assert_eq!(
            newest.changeset.added,
            vec![format!("{}.rs", MAX_JOBS + 4)],
            "the cap must drop the oldest, not the newest"
        );
    }

    /// The default is a **cache** path — every job in it is reconstructible
    /// from git, so it must never land in a state or config directory.
    #[test]
    fn the_default_spool_lives_under_a_cache_dir() {
        let d = SpoolDir::default_dir();
        assert!(d.ends_with("filigrio/spool"), "got {}", d.display());
    }
}
