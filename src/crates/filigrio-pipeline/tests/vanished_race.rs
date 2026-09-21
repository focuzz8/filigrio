//! ADR-0042 **Phase 1c / F8** at the production seam: a real `FsSource`, the
//! real `Pipeline`, and the shrink-guard the daemon turns on.
//!
//! `filigrio-resolve/tests/vanished.rs` pins the engine contract over the toy
//! ports. This file pins the two things only the real stack can show:
//!
//! 1. `Pipeline::apply`'s **shrink guard** must not veto an F8 convergence. The
//!    guard rejects node loss that no `removed` entry explains; a vanished file
//!    loses nodes and was announced as `added`/`modified`, so without wiring the
//!    vanished count into the guard, F8 would let the apply through the engine
//!    only to have the guard reject it — no convergence at all.
//! 2. **Exists-but-unreadable** on a real filesystem (`chmod 000`) still errors.
//!    `FsSource::exists` is `is_file()`, so a 0o000 file exists and F8 must not
//!    touch it; the read error propagates.

use filigrio_core::ChangeSet;
use filigrio_index::RustExtractor;
use filigrio_ingest::FsSource;
use filigrio_pipeline::Pipeline;
use filigrio_store::MemoryStore;
use std::fs;
use std::path::PathBuf;

fn repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("filigrio-f8-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/a.rs"), "fn alpha() {}\n").unwrap();
    fs::write(dir.join("src/b.rs"), "fn beta() {}\n").unwrap();
    fs::write(dir.join("src/c.rs"), "fn gamma() {}\n").unwrap();
    dir
}

fn files_indexed(store: &MemoryStore) -> Vec<String> {
    let mut v: Vec<String> = store
        .current()
        .unwrap()
        .graph
        .nodes
        .iter()
        .filter_map(|n| n.source_file.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The production race, end to end: the walk saw three files, one is deleted
/// before the apply reads it. The apply must converge — **with the daemon's
/// shrink guard enabled**, which is the configuration that actually ships.
#[test]
fn vanished_file_converges_through_the_pipeline_with_the_shrink_guard_on() {
    let dir = repo("pipeline");
    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    let ext = RustExtractor::new();

    Pipeline::new(&source, &ext, &store).build().unwrap();
    assert!(files_indexed(&store).contains(&"src/b.rs".to_string()));

    // The changeset (as a walk would have produced it) names all three; b.rs is
    // deleted in the window before the apply reads it.
    let prior = store.current().unwrap();
    let changes = ChangeSet {
        modified: vec!["src/a.rs".into(), "src/b.rs".into(), "src/c.rs".into()],
        ..Default::default()
    };
    fs::remove_file(dir.join("src/b.rs")).unwrap();

    let report = Pipeline::new(&source, &ext, &store)
        .with_shrink_guard(true)
        .apply(&prior, &changes)
        .expect("a vanished file must not abort the apply, nor trip the shrink guard");

    assert_eq!(
        report.vanished, 1,
        "the vanished count must reach BuildReport"
    );
    assert_eq!(report.changed, 3, "the changeset is still reported in full");
    let indexed = files_indexed(&store);
    assert!(indexed.contains(&"src/a.rs".to_string()), "{indexed:?}");
    assert!(indexed.contains(&"src/c.rs".to_string()), "{indexed:?}");
    assert!(
        !indexed.contains(&"src/b.rs".to_string()),
        "the vanished file's nodes must be gone: {indexed:?}"
    );
    assert!(
        !store
            .current()
            .unwrap()
            .manifest
            .entries
            .contains_key("src/b.rs"),
        "the vanished file's manifest entry must be gone"
    );
}

/// `project index`'s own path: `reconcile_and_apply` walks, then applies. A file
/// deleted inside that window is the exact production race F8 names.
#[test]
fn reconcile_and_apply_survives_a_file_deleted_after_the_walk() {
    let dir = repo("reconcile");
    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    let ext = RustExtractor::new();
    Pipeline::new(&source, &ext, &store).build().unwrap();

    // Touch every file so the reconcile sees drift on all three, then delete one
    // — the reconcile's own walk cannot have seen the deletion for `a`/`c` to be
    // re-extracted, which is what makes this a race and not a plain removal.
    for f in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        let p = dir.join(f);
        let body = fs::read_to_string(&p).unwrap();
        fs::write(&p, format!("{body}fn extra_{} () {{}}\n", f.len())).unwrap();
    }
    let prior = store.current().unwrap();
    let (_, build) = Pipeline::new(&source, &ext, &store)
        .with_shrink_guard(true)
        .reconcile_and_apply(&prior, true)
        .expect("reconcile_and_apply must converge");
    assert_eq!(build.vanished, 0, "nothing vanished on this pass");

    // Now the race: the walk inside reconcile happens first, and we simulate the
    // deletion landing between it and the read by deleting through a changeset
    // the walk already produced.
    let prior = store.current().unwrap();
    let changes = ChangeSet {
        modified: vec!["src/a.rs".into(), "src/b.rs".into()],
        ..Default::default()
    };
    fs::remove_file(dir.join("src/b.rs")).unwrap();
    let report = Pipeline::new(&source, &ext, &store)
        .with_shrink_guard(true)
        .apply(&prior, &changes)
        .expect("converge");
    assert_eq!(report.vanished, 1);
    assert!(!files_indexed(&store).contains(&"src/b.rs".to_string()));
}

/// A filename that is not valid UTF-8 is **outside the addressable path
/// vocabulary** — `ChangeSet` is `Vec<String>` — so the walk skips it with a
/// `warn!` and it never enters the changeset (audit §H).
///
/// It used to be `to_string_lossy()`d into a U+FFFD path that names no real
/// file; the engine's F8 reconcile then probed `exists()`, got `false`, and
/// folded it into `removed` with a **`vanished` bump**. That turned a hard
/// failure into a *mislabelled* silent one: `vanished` is documented as "a path
/// disappeared between detection and apply", which is not what happened, and no
/// reconcile could ever recover the file because every walk repeated the same
/// lossy conversion. The file is still not indexed — that part is inherent —
/// but the report no longer lies about why.
///
/// Skipped when the filesystem refuses to create such a name (rather than
/// false-passing) — the `store/src/atomic.rs` skip pattern.
#[cfg(unix)]
#[test]
fn non_utf8_filename_is_skipped_not_counted_as_vanished() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = repo("nonutf8");
    // 0xFF can never appear in valid UTF-8, so this name has no `&str` form.
    let bad = dir.join("src").join(OsStr::from_bytes(b"bad\xff.rs"));
    if fs::write(&bad, "fn hidden() {}\n").is_err() || !bad.exists() {
        eprintln!("skipped: this filesystem will not hold a non-UTF-8 filename");
        return;
    }

    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    let ext = RustExtractor::new();
    let report = Pipeline::new(&source, &ext, &store)
        .build()
        .expect("an unaddressable filename must not break the build");

    assert_eq!(
        report.vanished, 0,
        "nothing vanished: the file was never addressable, so `vanished` must not be credited"
    );
    assert_eq!(
        report.changed, 3,
        "only the three UTF-8 files are in the changeset"
    );
    let state = store.current().unwrap();
    let phantom: Vec<&String> = state
        .manifest
        .entries
        .keys()
        .filter(|k| k.contains('\u{FFFD}'))
        .collect();
    assert!(
        phantom.is_empty(),
        "no lossy phantom path may reach the manifest: {phantom:?}"
    );
    assert!(
        !files_indexed(&store).iter().any(|f| f.contains('\u{FFFD}')),
        "no lossy phantom path may reach the graph"
    );
}

/// The other half of the split, on a real filesystem: `chmod 000` leaves the
/// file **present** (`FsSource::exists` is `is_file()`), so F8 must not fold it
/// away — the read error is a real fault and must abort the apply.
///
/// Skipped when the test runs as root (permissions are not enforced) or when the
/// filesystem ignores the mode, since then there is nothing to observe.
#[cfg(unix)]
#[test]
fn exists_but_unreadable_still_aborts_the_apply() {
    use filigrio_core::Source;
    use std::os::unix::fs::PermissionsExt;

    let dir = repo("unreadable");
    let source = FsSource::new(&dir);
    let store = MemoryStore::new();
    let ext = RustExtractor::new();
    Pipeline::new(&source, &ext, &store).build().unwrap();
    let prior = store.current().unwrap();

    let victim = dir.join("src/b.rs");
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&victim).is_ok() {
        // root, or a filesystem that ignores the mode bit — nothing to test.
        let _ = fs::set_permissions(&victim, fs::Permissions::from_mode(0o644));
        eprintln!("skipped: this environment can read a 0o000 file");
        return;
    }
    assert!(
        source.exists("src/b.rs"),
        "precondition: a 0o000 file still EXISTS — this is what makes it not-vanished"
    );

    let changes = ChangeSet {
        modified: vec!["src/a.rs".into(), "src/b.rs".into()],
        ..Default::default()
    };
    let err = Pipeline::new(&source, &ext, &store)
        .with_shrink_guard(true)
        .apply(&prior, &changes)
        .expect_err("an unreadable-but-present file must still be a hard error");
    assert!(
        format!("{err}").to_lowercase().contains("permission")
            || format!("{err}").to_lowercase().contains("denied"),
        "the real IO error must propagate, got: {err}"
    );
    assert!(
        files_indexed(&store).contains(&"src/b.rs".to_string()),
        "the aborted apply wrote nothing, so b.rs's prior nodes are untouched"
    );

    let _ = fs::set_permissions(&victim, fs::Permissions::from_mode(0o644));
}
