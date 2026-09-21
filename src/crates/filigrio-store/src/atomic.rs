//! Atomic file writes shared by every persistence path (ADR-0042 Phase 1c F1).
//!
//! Contract: no torn/partial file is ever observable at the target path — on
//! any failure the prior file (if any) survives intact and no temp is left
//! behind. Durability (fsync) is deliberately out of scope. Extracted from
//! `ProjectRegistry::save_to` so temp+rename exists exactly once.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// Write `bytes` to `path` atomically, creating missing parent directories.
///
/// The temp file lives in the SAME directory as the target (so the rename
/// stays on one filesystem and is atomic) and its name is pid-tagged to avoid
/// colliding with a concurrent writer's temp.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("atomic_write: no file name in {}", path.display()),
        )
    })?;
    let mut tmp_name = OsString::from(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Don't leave the temp behind on a failed rename.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::atomic_write;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("filigrio-atomic-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Any `*.tmp.*` entries in `dir` (temp residue the util must never leave).
    fn temp_residue(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.contains(".tmp."))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn writes_content_and_creates_parent_dirs() {
        let dir = tmp("happy");
        let path = dir.join("nested").join("deep").join("state.json");

        atomic_write(&path, b"{\"ok\":true}").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"{\"ok\":true}");
        assert!(
            temp_residue(path.parent().unwrap()).is_empty(),
            "no temp file left after a successful write"
        );
    }

    #[test]
    fn overwrite_replaces_content_exactly() {
        let dir = tmp("overwrite");
        let path = dir.join("state.json");
        // New content is SHORTER than the old — a torn in-place write would
        // leave a suffix of the old file behind.
        atomic_write(&path, b"old content, quite long indeed").unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(temp_residue(&dir).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_preserves_prior_content() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("readonly");
        let path = dir.join("state.json");
        atomic_write(&path, b"prior good state").unwrap();

        // Read-only directory: the temp-file write itself must fail, and the
        // prior file must survive untouched.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = atomic_write(&path, b"never lands");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if result.is_ok() {
            // Running privileged (root ignores directory permissions) — this
            // failure cannot be forced here, so there is nothing to assert.
            return;
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"prior good state");
        assert!(
            temp_residue(&dir).is_empty(),
            "no temp left after a failure"
        );
    }

    #[test]
    fn failed_rename_cleans_up_temp_and_preserves_target() {
        let dir = tmp("renamefail");
        // The target path is a NON-EMPTY directory, so the final rename must
        // fail after the temp was already written.
        let target = dir.join("state.json");
        std::fs::create_dir_all(target.join("occupied")).unwrap();

        assert!(atomic_write(&target, b"cannot land").is_err());

        assert!(
            target.join("occupied").exists(),
            "rename failure must leave the pre-existing target intact"
        );
        assert!(
            temp_residue(&dir).is_empty(),
            "the temp must be cleaned up when the rename fails"
        );
    }

    #[test]
    fn stale_temp_from_crashed_writer_does_not_block() {
        let dir = tmp("stale");
        let path = dir.join("state.json");
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate a crashed previous writer: a stale temp under OUR pid (the
        // pid was recycled) and one under a foreign pid.
        let ours = dir.join(format!("state.json.tmp.{}", std::process::id()));
        let foreign = dir.join("state.json.tmp.999999999");
        std::fs::write(&ours, b"{ torn garba").unwrap();
        std::fs::write(&foreign, b"{ torn garba").unwrap();

        atomic_write(&path, b"fresh content").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"fresh content");
        assert!(!ours.exists(), "our-pid temp is consumed by the rename");
        // A foreign writer's temp is not ours to garbage-collect.
        assert!(foreign.exists());
    }
}
