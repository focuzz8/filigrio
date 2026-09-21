//! Managed blocks — the reversibility primitive (ADR-0034 §2.3, ADR-0032b §3).
//!
//! A managed block is a marker-delimited region we own inside a file the *user*
//! owns. Install upserts it; uninstall removes exactly it; everything the user
//! wrote around it survives byte for byte.
//!
//! ## Why byte-exactness is achievable here, precisely
//!
//! The insertion is defined so that removal can invert it without recording any
//! state. For a file whose prior content is `C`:
//!
//! ```text
//! block  B = "<start>\n" + body + "\n<end>\n"
//! C empty            → new = B
//! C ends with "\n"   → new = C + "\n" + B      (exactly one blank line before <start>)
//! C lacks final "\n" → new = C + "\n" + "\n" + B
//! ```
//!
//! Removal takes the text before `<start>`'s line and the text after `<end>`'s
//! newline, and drops **exactly one** `"\n"` from the tail of the head. That
//! inverts the first two cases exactly. The third normalizes: a file with no
//! final newline gets one, so `install` → `uninstall` yields `C + "\n"` rather
//! than `C`. That is the one and only normalization, it is what every text
//! editor would do anyway, and [`tests::round_trip_adds_a_final_newline_when_the_file_lacked_one`]
//! pins it rather than leaving it to be discovered.
//!
//! The oracle (`graphify/install.py:463-510`) matched its block by a bare
//! heading (`## graphify`) and had to grow a `line.strip() == marker` rule after
//! a *substring* match anchored the replace on a bullet that merely mentioned
//! the heading and deleted everything to the next `##` — "silently destroying
//! hand-curated content" (their #1688). Explicit paired start/end markers,
//! matched at line granularity, make that failure unrepresentable: a mention of
//! the marker inside prose is not a line equal to the marker.
//!
//! ## A delimiter names the thing it delimits, and nothing else
//!
//! The marker pairs below are **command-free** (ADR-0034 §17): a delimiter that
//! embeds anything that can change — a command name, a verb, guidance — cannot
//! find the blocks it wrote once that thing changes, because [`find`] matches
//! whole lines. A renamed delimiter orphans every existing block: `uninstall`
//! reports "no filigrio block in this file" over a file that has one, and
//! `install` stacks a second block under the first, while the idempotency test
//! passes throughout — a test that uses one marker cannot see across a version
//! boundary. The rule is a property rather than a convention:
//! [`tests::a_delimiter_is_a_single_token_because_one_that_can_change_cannot_find_what_it_wrote`],
//! with [`tests::a_delimiter_that_changed_orphans_every_block_the_previous_build_wrote`]
//! keeping the measurement executable.
//!
//! The guidance the markers once carried lives in the block **body**, which is
//! regenerated on every install and therefore free to change forever without
//! orphaning anything — an HTML comment line at the top of
//! `assets/capability/agents-md.hbs`, a `#` comment line at the top of
//! `clients::hermes::entry_body`. It stays a *comment* in both, deliberately:
//! `AGENTS.md` is injected verbatim into the context of every agent that reads
//! it, and promoting a file-maintenance instruction into visible prose would put
//! it in the model's reading material on every request.
//!
//! Blocks written by the pre-correction build are orphaned, and no migration
//! path is offered: nothing here has shipped, so that is affordable exactly
//! once. The property above is what makes it once.

use crate::{read_opt, remove_file, write_all, Action, InstallError};
use std::path::Path;

/// A paired start/end marker. Both must be whole-line comments in the host
/// file's syntax — `<!-- … -->` for markdown, `#` for shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Markers {
    pub start: &'static str,
    pub end: &'static str,
}

impl Markers {
    pub const fn new(start: &'static str, end: &'static str) -> Self {
        Self { start, end }
    }
}

/// Markdown files (`AGENTS.md`, `CLAUDE.md`): HTML comments, invisible when
/// rendered. Command-free — see the module docs, *A delimiter names the thing it
/// delimits*.
pub const MARKDOWN: Markers = Markers::new("<!-- filigrio:start -->", "<!-- filigrio:end -->");

/// Shell scripts (git hooks). The oracle's shape (`graphify/hooks.py:8-11`),
/// under our own name: filigrio is a different tool, so neither one's hooks
/// may find, refresh, or remove the other's block.
pub const SHELL: Markers = Markers::new("# filigrio-hook-start", "# filigrio-hook-end");

/// YAML configs (`~/.hermes/config.yaml`). Same `#` comment syntax as shell, and
/// deliberately at column 0: a YAML comment is lexical and does not terminate a
/// block mapping at any column, which `hermes mcp list` confirmed by reading both
/// servers out of a config marked this way (ADR-0034 §15).
pub const YAML: Markers = Markers::new("# filigrio:start", "# filigrio:end");

/// The half-open byte range a block occupies, from the first byte of the
/// `start` line through the byte after `end`'s newline.
fn find(content: &str, m: &Markers) -> Option<(usize, usize)> {
    let mut start = None;
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        match start {
            None if trimmed.trim() == m.start => start = Some(offset),
            Some(s) if trimmed.trim() == m.end => return Some((s, offset + line.len())),
            _ => {}
        }
        offset += line.len();
    }
    // A start with no end is a truncated/hand-mangled block. Treat it as
    // running to EOF rather than pretending it is absent: leaving it behind
    // would make `uninstall` a lie and the next `install` would stack a second
    // block under it.
    start.map(|s| (s, content.len()))
}

/// Is our block present?
pub fn contains(content: &str, m: &Markers) -> bool {
    find(content, m).is_some()
}

/// The block's body (between the markers), if present.
pub fn body_of(content: &str, m: &Markers) -> Option<String> {
    let (s, e) = find(content, m)?;
    let inner = &content[s..e];
    let mut lines: Vec<&str> = inner.lines().collect();
    if !lines.is_empty() {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.trim() == m.end) {
        lines.pop();
    }
    Some(lines.join("\n"))
}

/// Render the block text for `body`.
fn block_text(body: &str, m: &Markers) -> String {
    format!("{}\n{}\n{}\n", m.start, body.trim_matches('\n'), m.end)
}

/// Insert or replace the managed block, leaving all other content untouched.
///
/// If several blocks exist (a hand-edit, or a pre-marker install that was
/// appended twice), the **first** is replaced in place and the rest are
/// removed — so "installing twice does not duplicate blocks" holds even when
/// the starting state was already duplicated.
pub fn upsert(content: &str, body: &str, m: &Markers) -> String {
    let block = block_text(body, m);

    let Some((s, e)) = find(content, m) else {
        return append(content, &block);
    };

    let mut out = String::with_capacity(content.len() + block.len());
    out.push_str(&content[..s]);
    out.push_str(&block);
    // Any further blocks are strays; drop them from the tail.
    out.push_str(&strip_all(&content[e..], m));
    out
}

/// Append `block` to `content` per the scheme documented at the top of this
/// module — one blank line of separation, and never two.
fn append(content: &str, block: &str) -> String {
    if content.is_empty() {
        return block.to_string();
    }
    let mut out = String::with_capacity(content.len() + block.len() + 2);
    out.push_str(content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(block);
    out
}

fn strip_all(content: &str, m: &Markers) -> String {
    let mut cur = content.to_string();
    while let Some((s, e)) = find(&cur, m) {
        cur = format!("{}{}", &cur[..s], &cur[e..]);
    }
    cur
}

/// Remove the managed block, inverting [`upsert`]'s insertion.
///
/// `None` when there was no block — the caller reports "nothing of ours here"
/// rather than rewriting a file it does not need to touch.
pub fn remove(content: &str, m: &Markers) -> Option<String> {
    let (s, e) = find(content, m)?;
    let head = &content[..s];
    let tail = strip_all(&content[e..], m);

    // Drop exactly the one separator newline `append` contributed.
    let head = head.strip_suffix('\n').unwrap_or(head);
    Some(format!("{head}{tail}"))
}

/// Upsert the block in the file at `path`, creating it if absent.
pub fn upsert_file(path: &Path, body: &str, m: &Markers) -> Result<(Action, String), InstallError> {
    let existing = read_opt(path)?;
    let had_block = existing.as_deref().is_some_and(|c| contains(c, m));
    let updated = upsert(existing.as_deref().unwrap_or(""), body, m);

    if existing.as_deref() == Some(updated.as_str()) {
        return Ok((Action::Unchanged, "already current".into()));
    }
    write_all(path, &updated)?;
    Ok(if had_block {
        (Action::Updated, "managed block refreshed".into())
    } else if existing.is_some() {
        (Action::Installed, "managed block appended".into())
    } else {
        (Action::Installed, "file created".into())
    })
}

/// Remove the block from the file at `path`. When nothing but our block was in
/// the file, the file itself goes — an empty `AGENTS.md` we created is not a
/// leftover the user asked for. `keep_if_empty` opts out for files where an
/// empty shell is meaningful.
pub fn remove_file_block(
    path: &Path,
    m: &Markers,
    empty_means_ours: impl Fn(&str) -> bool,
) -> Result<(Action, String), InstallError> {
    let Some(existing) = read_opt(path)? else {
        return Ok((Action::Absent, "no such file".into()));
    };
    let Some(cleaned) = remove(&existing, m) else {
        return Ok((Action::Absent, "no filigrio block in this file".into()));
    };
    if empty_means_ours(&cleaned) {
        return Ok((Action::Removed, remove_file(path)?.detail("block")));
    }
    write_all(path, &cleaned)?;
    Ok((
        Action::Removed,
        "managed block removed, rest preserved".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Markers = Markers::new("<!-- s -->", "<!-- e -->");

    /// **A delimiter must not embed anything that can change, because a
    /// delimiter that changes cannot find what it wrote.**
    ///
    /// Stated mechanically: strip the host language's comment syntax and what is
    /// left must be a single token. A command, a verb, a sentence of guidance —
    /// none of them survive that test, and every one of them is a thing this
    /// project has already renamed once. Anything a reader needs told belongs in
    /// the block *body*, which install regenerates.
    #[test]
    fn a_delimiter_is_a_single_token_because_one_that_can_change_cannot_find_what_it_wrote() {
        for m in [MARKDOWN, SHELL, YAML] {
            for marker in [m.start, m.end] {
                let name = marker
                    .trim()
                    .trim_start_matches("<!--")
                    .trim_end_matches("-->")
                    .trim_start_matches('#')
                    .trim();
                assert!(
                    !name.is_empty() && !name.contains(char::is_whitespace),
                    "`{marker}` is not a name, it is prose — and prose gets edited: {name:?}"
                );
                assert!(
                    !marker.contains('`'),
                    "`{marker}` quotes something; a delimiter that quotes a command moves with it"
                );
            }
        }
    }

    /// The measurement behind the rule above, kept executable: this is what a
    /// renamed delimiter did to blocks the previous build had written. `uninstall`
    /// reported nothing of ours while our block sat in the file, and `install`
    /// stacked a second one under the first.
    #[test]
    fn a_delimiter_that_changed_orphans_every_block_the_previous_build_wrote() {
        // The same block, delimited the way two consecutive builds spelled it.
        let old = Markers::new(
            "<!-- filigrio:start — `integration install` -->",
            "<!-- e -->",
        );
        let new = Markers::new("<!-- filigrio:start — `agent install` -->", "<!-- e -->");

        let installed = upsert("# doc\n", "body", &old);
        assert!(contains(&installed, &old));

        assert!(
            !contains(&installed, &new),
            "the new build cannot see the old build's block"
        );
        assert!(
            remove(&installed, &new).is_none(),
            "…so uninstall says `no filigrio block in this file` about a file that has one"
        );
        let twice = upsert(&installed, "body", &new);
        assert_eq!(
            twice.matches("<!-- filigrio:start").count(),
            2,
            "…and install stacks a second block beneath the first:\n{twice}"
        );
    }

    #[test]
    fn round_trip_is_byte_exact_for_a_file_with_a_final_newline() {
        let prior = "# My notes\n\nSome content the user wrote.\n";
        let installed = upsert(prior, "graph stuff", &M);
        assert!(installed.starts_with(prior));
        assert_eq!(remove(&installed, &M).unwrap(), prior);
    }

    #[test]
    fn round_trip_is_byte_exact_for_an_empty_file() {
        let installed = upsert("", "graph stuff", &M);
        assert_eq!(remove(&installed, &M).unwrap(), "");
    }

    /// The one documented normalization: a file with no trailing newline gets
    /// one. Pinned so it is a decision, not a surprise.
    #[test]
    fn round_trip_adds_a_final_newline_when_the_file_lacked_one() {
        let prior = "no trailing newline";
        let installed = upsert(prior, "b", &M);
        assert_eq!(remove(&installed, &M).unwrap(), "no trailing newline\n");
    }

    /// The headline non-negotiable: a user edits *around* the block — before it
    /// and after it — and uninstall must return exactly their text.
    #[test]
    fn user_edits_around_the_block_survive_uninstall_byte_exactly() {
        let prior = "# Project\n\nIntro paragraph.\n";
        let installed = upsert(prior, "OUR BODY", &M);

        // The user then edits above and below our block.
        let edited = installed.replace("Intro paragraph.", "Intro paragraph, revised.")
            + "\n## Their own section\n\nMore of their prose.\n";
        assert!(contains(&edited, &M));

        let expected =
            "# Project\n\nIntro paragraph, revised.\n\n## Their own section\n\nMore of their prose.\n";
        assert_eq!(remove(&edited, &M).unwrap(), expected);
    }

    #[test]
    fn installing_twice_does_not_duplicate_the_block() {
        let once = upsert("# doc\n", "body v1", &M);
        let twice = upsert(&once, "body v1", &M);
        assert_eq!(once, twice, "a second identical install is a no-op");
        assert_eq!(twice.matches(M.start).count(), 1);

        let upgraded = upsert(&twice, "body v2", &M);
        assert_eq!(upgraded.matches(M.start).count(), 1);
        assert!(upgraded.contains("body v2") && !upgraded.contains("body v1"));
        assert_eq!(remove(&upgraded, &M).unwrap(), "# doc\n");
    }

    /// The block moves nowhere on upgrade: content the user put *after* it stays
    /// after it. An implementation that removed-then-appended would silently
    /// reorder their file.
    #[test]
    fn upgrade_replaces_in_place_rather_than_moving_the_block_to_the_end() {
        let prior = "# doc\n";
        let installed = upsert(prior, "v1", &M);
        let with_tail = format!("{installed}\n## After\n");
        let upgraded = upsert(&with_tail, "v2", &M);
        assert!(
            upgraded.find("v2") < upgraded.find("## After"),
            "block must stay where it was:\n{upgraded}"
        );
    }

    /// A pre-existing duplicate (hand-edit, or a pre-marker install appended
    /// twice) collapses to one — installing twice must not *preserve* a
    /// duplicate either.
    #[test]
    fn duplicate_blocks_collapse_to_one_and_uninstall_clears_them_all() {
        let dup = format!(
            "head\n\n{}\n\n{}",
            block_text("a", &M).trim_end(),
            block_text("b", &M)
        );
        assert_eq!(dup.matches(M.start).count(), 2);

        let fixed = upsert(&dup, "c", &M);
        assert_eq!(fixed.matches(M.start).count(), 1);
        assert!(fixed.contains('c') && !fixed.contains("\nb\n"));

        assert!(!remove(&dup, &M).unwrap().contains(M.start));
    }

    /// A marker *mentioned* in prose is not a marker line — the oracle's #1688
    /// content-destroying bug class, made unrepresentable.
    #[test]
    fn a_marker_mentioned_inside_prose_is_not_matched() {
        let prior = "Docs say the block is delimited by `<!-- s -->` and `<!-- e -->`.\n";
        assert!(!contains(prior, &M));
        let installed = upsert(prior, "body", &M);
        assert_eq!(remove(&installed, &M).unwrap(), prior);
    }

    #[test]
    fn removing_from_a_file_without_our_block_is_none_not_an_empty_file() {
        assert!(remove("just the user's text\n", &M).is_none());
    }

    /// A truncated block (start, no end) is still removed — otherwise uninstall
    /// reports success while leaving our text behind, and the next install
    /// stacks a second block beneath it.
    #[test]
    fn a_block_missing_its_end_marker_still_uninstalls() {
        let mangled = "head\n\n<!-- s -->\nour body\n";
        assert!(contains(mangled, &M));
        assert_eq!(remove(mangled, &M).unwrap(), "head\n");
    }

    #[test]
    fn body_of_reads_back_what_upsert_wrote() {
        let c = upsert("x\n", "line one\nline two", &M);
        assert_eq!(body_of(&c, &M).as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn file_helpers_round_trip_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("AGENTS.md");
        std::fs::write(&p, "# Agents\n\nUser prose.\n").unwrap();
        let before = std::fs::read_to_string(&p).unwrap();

        let (a, _) = upsert_file(&p, "graph body", &M).unwrap();
        assert_eq!(a, Action::Installed);
        let (a, _) = upsert_file(&p, "graph body", &M).unwrap();
        assert_eq!(a, Action::Unchanged, "second install must not rewrite");

        let (a, _) = remove_file_block(&p, &M, |c| c.trim().is_empty()).unwrap();
        assert_eq!(a, Action::Removed);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
    }

    #[test]
    fn a_file_that_held_only_our_block_is_deleted_on_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("AGENTS.md");
        upsert_file(&p, "graph body", &M).unwrap();
        remove_file_block(&p, &M, |c| c.trim().is_empty()).unwrap();
        assert!(!p.exists(), "we created it and it held nothing else");
    }
}
