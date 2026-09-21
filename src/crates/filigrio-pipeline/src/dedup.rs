//! Two-stage deduplication (ADR-0032 §4).
//!
//! Dedup treats an event as a signal, not data. Gate order:
//! mtime (0032c fast-path) → hash → parse.

use filigrio_core::{Manifest, ManifestEntry};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Result of the dedup check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupResult {
    /// File unchanged (mtime matches manifest).
    Unchanged,
    /// File changed (hash differs from manifest).
    Changed,
    /// File not in manifest (new file).
    NotFound,
}

/// Deduplication gate for a single file.
pub fn dedup_file(
    abs_path: &Path,
    key: &str,
    manifest: &Manifest,
    mtime: Option<u64>,
    deep: bool,
) -> Result<DedupResult, std::io::Error> {
    // Look up by the manifest KEY (the relative path the persisted manifest is keyed
    // by), but read/hash via the ABSOLUTE path. These differ in reconcile — the walk
    // yields absolute paths while the Engine keys the manifest relative — and
    // conflating them made every lookup miss (always `NotFound`), defeating the whole
    // mtime fast-path / deep distinction.
    let entry = match manifest.entries.get(key) {
        Some(entry) => entry,
        None => return Ok(DedupResult::NotFound),
    };

    // Stage 1: mtime fast-path (0032c) — SKIPPED under `deep` (crash recovery /
    // a deep index), where mtime is untrusted (restored backup, clock reset)
    // and every file is re-hashed against the manifest.
    if !deep {
        if let Some(manifest_mtime) = entry.last_modified {
            if let Some(file_mtime) = mtime {
                if file_mtime == manifest_mtime {
                    return Ok(DedupResult::Unchanged);
                }
            }
        }
    }

    // Stage 2: hash check (authority)
    let file_hash_u64 = hash_file_u64(abs_path)?;
    if file_hash_u64 == entry.hash {
        // Hash matches even though mtime didn't: the file is Unchanged. Note this
        // does NOT rewrite the manifest — the stale mtime persists, so the file
        // falls through the mtime fast-path and re-hashes on every pass until an
        // apply rewrites its entry.
        return Ok(DedupResult::Unchanged);
    }

    // Hash differs — file changed
    Ok(DedupResult::Changed)
}

/// Compute SHA-256 hash of a file.
pub fn hash_file(path: &Path) -> Result<String, std::io::Error> {
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// [`hash_file`] truncated to the manifest's `u64` hash representation: the
/// first 16 hex chars (64 bits) of the SHA-256, parsed as `u64`. The single
/// definition of the hash→u64 mapping every manifest comparison uses.
pub fn hash_file_u64(path: &Path) -> Result<u64, std::io::Error> {
    let hex = hash_file(path)?;
    Ok(u64::from_str_radix(&hex[..16], 16).unwrap_or(0))
}

/// Get mtime of a file as **nanoseconds** since the Unix epoch. Nanos (not seconds)
/// so the mtime fast-path (§4) reliably distinguishes sub-second edits: a
/// second-granularity mtime misses a same-second modification and wrongly reports
/// `Unchanged` without hashing (ADR-0032c open-Q1: precision = nanos). `hash`
/// remains the content authority — mtime only decides whether to skip the hash.
pub fn get_mtime(path: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_nanos() as u64)
}

/// Update the manifest entry under an explicit `key` from the file at `abs_path`.
///
/// The key is the caller's manifest key (the Engine keys manifests by the
/// root-relative path) and is deliberately separate from the absolute path used
/// to read/hash — the same relative-vs-absolute split `dedup_file` documents;
/// keying by the absolute path string here was exactly that conflation.
pub fn update_manifest_entry(
    abs_path: &Path,
    key: &str,
    manifest: &mut Manifest,
) -> Result<(), std::io::Error> {
    let hash_u64 = hash_file_u64(abs_path)?;
    let mtime = get_mtime(abs_path);

    let entry = ManifestEntry {
        hash: hash_u64,
        last_modified: mtime,
        revision: manifest.latest_revision().clone(),
    };

    manifest.entries.insert(key.to_string(), entry);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use filigrio_core::Manifest;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_hash_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "test content").unwrap();

        let hash1 = hash_file(file.path()).unwrap();
        let hash2 = hash_file(file.path()).unwrap();

        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64); // SHA-256 hex
    }

    #[test]
    fn test_hash_changes() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "content 1").unwrap();

        let hash1 = hash_file(file.path()).unwrap();

        writeln!(file, "content 2").unwrap();

        let hash2 = hash_file(file.path()).unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_dedup_unchanged() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "test content").unwrap();

        let mut manifest = Manifest::default();
        update_manifest_entry(file.path(), file.path().to_str().unwrap(), &mut manifest).unwrap();

        let result = dedup_file(
            file.path(),
            file.path().to_str().unwrap(),
            &manifest,
            get_mtime(file.path()),
            false,
        )
        .unwrap();
        assert_eq!(result, DedupResult::Unchanged);
    }

    #[test]
    fn test_dedup_changed() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "content 1").unwrap();
        file.flush().unwrap();

        let mut manifest = Manifest::default();
        update_manifest_entry(file.path(), file.path().to_str().unwrap(), &mut manifest).unwrap();

        // Modify file
        writeln!(file, "content 2").unwrap();
        file.flush().unwrap();

        // Pass None for mtime to bypass fast-path and force hash check
        let result = dedup_file(
            file.path(),
            file.path().to_str().unwrap(),
            &manifest,
            None,
            false,
        )
        .unwrap();
        assert_eq!(result, DedupResult::Changed);
    }

    #[test]
    fn test_dedup_not_found() {
        let file = NamedTempFile::new().unwrap();
        let manifest = Manifest::default();

        let result = dedup_file(
            file.path(),
            file.path().to_str().unwrap(),
            &manifest,
            get_mtime(file.path()),
            false,
        )
        .unwrap();
        assert_eq!(result, DedupResult::NotFound);
    }
}
