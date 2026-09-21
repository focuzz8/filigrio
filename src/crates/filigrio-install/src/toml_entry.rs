//! Reversible edits to a **TOML** config the user owns (`~/.codex/config.toml`).
//!
//! [`crate::json_entry`]'s `serde_json` keyed merge does not apply here: TOML
//! carries comments, table headers, key order and quoting style, and a
//! parse-into-a-model / re-serialise round trip throws all four away. So this
//! module is built on **`toml_edit`**, which is format-preserving by design —
//! it keeps the untouched text as it was and only re-renders what changed.
//!
//! The reversibility unit is the same as JSON's: **one named sub-table**.
//! Install adds `[<container>.<key>]`, uninstall removes exactly that and
//! nothing else.
//!
//! ## What "byte-exact" means here, measured rather than claimed
//!
//! The OpenCode adapter shipped a byte-exactness claim that was wrong — a real
//! config ended *without* a final newline and our writer appended one, so the
//! round trip came back one byte longer (ADR-0034 §10 correction). Rather than
//! trust `toml_edit`'s docs, the same exercise was run against it first, and it
//! has **the same defect**: a document parsed from text with no final newline
//! renders back *with* one as soon as anything is inserted. So:
//!
//! - **The final newline is the file's property, not ours**
//!   ([`crate::had_trailing_newline`] + [`match_trailing_newline`]). A config
//!   that ended without one still does; a file we create gets one, POSIX-style.
//! - **Comments, key order, whitespace and quoting survive**, including a
//!   comment attached to a *sibling* `[mcp_servers.*]` header — which is more
//!   than `codex mcp add` itself manages (ADR-0034 §13).
//! - **The container table is only removed if it was implicit.** An implicit
//!   `[mcp_servers]` exists solely because a sub-table needed a parent, so
//!   dropping it when we drop our sub-table restores the prior bytes. A
//!   `[mcp_servers]` header the *user* wrote is theirs and stays, even empty.
//! - **A file left holding nothing at all is deleted**, so a `config.toml` we
//!   created does not survive as an empty husk. "Nothing at all" means the
//!   rendered text is blank — a file holding only the user's comments is not
//!   empty and is written back, not removed.
//!
//! Malformed TOML is **refused by name and position** ([`InstallError::BadToml`])
//! and left untouched, the same posture [`crate::json_entry`] takes for JSON it
//! cannot parse. So is a `mcp_servers` that is not a table header — a user may
//! legally write `mcp_servers = { … }` as an inline value, and half-handling
//! that shape is worse than naming it.

use crate::{
    had_trailing_newline, read_opt, remove_file, write_all, Action, EntryState, InstallError,
};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, Value};

/// Parse the document at `path`. Absent → an empty document. Malformed → an
/// error naming the file and the position, never a silent reset.
fn load_document(path: &Path) -> Result<DocumentMut, InstallError> {
    let Some(text) = read_opt(path)? else {
        return Ok(DocumentMut::new());
    };
    text.parse::<DocumentMut>()
        .map_err(|source| InstallError::BadToml {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
}

/// Force the rendered document's final-newline state to match the file's.
///
/// `toml_edit` appends one to a document that had none — measured, not read.
/// When the original ended in a non-newline byte it had, by definition, no
/// trailing blank lines, so any trailing newline in the output is ours to
/// remove.
fn match_trailing_newline(mut text: String, want: bool) -> String {
    if want {
        if !text.ends_with('\n') {
            text.push('\n');
        }
    } else {
        while text.ends_with('\n') {
            text.pop();
        }
    }
    text
}

/// Borrow `container` as a table, or say why it is not one.
fn container_kind(item: &Item) -> &'static str {
    match item {
        Item::Value(v) if v.is_inline_table() => "an inline table",
        Item::Value(_) => "a value",
        Item::ArrayOfTables(_) => "an array of tables",
        _ => "not a table",
    }
}

/// Insert or refresh `[container.key]` in the TOML document at `path`.
pub fn upsert(
    path: &Path,
    container: &str,
    key: &str,
    entry: Table,
) -> Result<(Action, String), InstallError> {
    let before = read_opt(path)?;
    let mut doc = load_document(path)?;

    let had = doc
        .get(container)
        .and_then(Item::as_table)
        .is_some_and(|t| t.contains_key(key));

    // Created implicit so the rendered header is `[container.key]` and not a
    // bare `[container]` we invented above it. An *existing* table's implicit
    // flag is never touched: flipping a user's real `[mcp_servers]` header to
    // implicit would delete it, comment and all.
    let slot = doc.entry(container).or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let Some(table) = slot.as_table_mut() else {
        return Err(InstallError::NotTomlTable {
            path: path.to_path_buf(),
            key: container.to_string(),
            found: container_kind(slot),
        });
    };
    table.insert(key, Item::Table(entry));

    let text = match_trailing_newline(doc.to_string(), had_trailing_newline(before.as_deref()));
    if before.as_deref() == Some(text.as_str()) {
        return Ok((
            Action::Unchanged,
            format!("[{container}.{key}] already current"),
        ));
    }
    write_all(path, &text)?;
    Ok(if had {
        (Action::Updated, format!("[{container}.{key}] refreshed"))
    } else if before.is_some() {
        (Action::Installed, format!("[{container}.{key}] added"))
    } else {
        (Action::Installed, "file created".into())
    })
}

/// Remove `[container.key]`. Deletes the file when nothing at all is left.
pub fn remove(path: &Path, container: &str, key: &str) -> Result<(Action, String), InstallError> {
    let Some(before) = read_opt(path)? else {
        return Ok((Action::Absent, "no such file".into()));
    };
    let mut doc = load_document(path)?;

    let Some(table) = doc.get_mut(container).and_then(Item::as_table_mut) else {
        return Ok((Action::Absent, format!("no `[{container}]` in this file")));
    };
    if table.remove(key).is_none() {
        return Ok((Action::Absent, format!("no `[{container}.{key}]` entry")));
    }
    // Implicit = the table has no header of its own and existed only to parent
    // our entry. One the user wrote is theirs, empty or not.
    if table.is_empty() && table.is_implicit() {
        doc.remove(container);
    }

    let text = match_trailing_newline(doc.to_string(), had_trailing_newline(Some(&before)));
    // Blank, not "no keys": a config holding only the user's comments still has
    // content worth keeping, and deleting it would be the loss this module
    // exists to prevent.
    if text.trim().is_empty() {
        return Ok((Action::Removed, remove_file(path)?.detail("entry")));
    }
    write_all(path, &text)?;
    Ok((
        Action::Removed,
        format!("[{container}.{key}] removed, rest preserved"),
    ))
}

/// Anything with content in it, as a [`Value`] — so that the two spellings of
/// one table, `[mcp_servers.filigrio]` and an inline `filigrio = { … }`,
/// compare as the same content. They are the same content: Codex reads
/// identical fields out of either.
fn as_value(item: &Item) -> Option<Value> {
    match item {
        Item::Value(v) => Some(v.clone()),
        Item::Table(t) => Some(Value::InlineTable(t.clone().into_inline_table())),
        Item::ArrayOfTables(a) => Some(Value::Array(a.clone().into_array())),
        Item::None => None,
    }
}

/// Compare two TOML values **by content**, ignoring every decoration
/// `toml_edit` carries: surrounding whitespace, comments, quoting style, and
/// key order inside a table.
///
/// `toml_edit`'s types deliberately implement no `PartialEq` — two documents
/// that spell the same data differently are not the same *document*, which is
/// exactly the property that makes this crate's round trips byte-exact. It is
/// also why a rendered-text comparison cannot answer `status`: the entry we
/// installed picks up the surrounding file's style, so a text diff would report
/// the user's formatting as our staleness. So the walk is written out.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.value() == y.value(),
        (Value::Integer(x), Value::Integer(y)) => x.value() == y.value(),
        (Value::Float(x), Value::Float(y)) => x.value() == y.value(),
        (Value::Boolean(x), Value::Boolean(y)) => x.value() == y.value(),
        (Value::Datetime(x), Value::Datetime(y)) => x.value() == y.value(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(x, y)| same_value(x, y))
        }
        (Value::InlineTable(x), Value::InlineTable(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| same_value(v, w)))
        }
        _ => false,
    }
}

/// Is `[container.key]` the table install would write?
///
/// See [`EntryState`] for why this compares the entry rather than the file, and
/// [`same_value`] for what "compare" means when the library under us refuses to
/// define equality.
pub fn state(
    path: &Path,
    container: &str,
    key: &str,
    expected: &Table,
) -> Result<EntryState, InstallError> {
    if read_opt(path)?.is_none() {
        return Ok(EntryState::Absent);
    }
    let doc = load_document(path)?;
    let Some(found) = doc
        .get(container)
        .and_then(Item::as_table)
        .and_then(|t| t.get(key))
        .and_then(as_value)
    else {
        return Ok(EntryState::Absent);
    };
    let want = Value::InlineTable(expected.clone().into_inline_table());
    Ok(if same_value(&found, &want) {
        EntryState::Current
    } else {
        EntryState::Stale
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml_edit::{value, Array};

    const CONTAINER: &str = "mcp_servers";

    fn entry() -> Table {
        let mut t = Table::new();
        t["command"] = value("/opt/g/bin/filigrio-mcp");
        let mut args = Array::new();
        args.push("--socket");
        args.push("/run/filigrio.sock");
        t["args"] = value(args);
        t
    }

    /// The fixture the brief asks for, and the one `codex mcp add` itself fails:
    /// a comment above, a pre-existing `[mcp_servers.*]` whose *header* carries
    /// a comment, a comment below, and **no trailing newline**.
    ///
    /// `codex mcp add filigrio … && codex mcp remove filigrio` (codex-cli
    /// 0.146.0) loses the header comment and appends a final newline. Ours must
    /// come back byte for byte.
    const REAL_SHAPED: &str = concat!(
        "# my codex config — do not let an installer eat this\n",
        "model = \"o3\"\n",
        "approval_policy = \"on-request\"\n",
        "\n",
        "# an existing server I care about\n",
        "[mcp_servers.sentry]\n",
        "command = \"npx\"\n",
        "args = [\"-y\", \"@sentry/mcp\"]\n",
        "\n",
        "# and a trailing note about the TUI\n",
        "[tui]\n",
        "notifications = true"
    );

    #[test]
    fn a_real_shaped_config_round_trips_byte_exactly() {
        assert!(!REAL_SHAPED.ends_with('\n'), "the fixture's whole point");
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, REAL_SHAPED).unwrap();

        let (a, _) = upsert(&p, CONTAINER, "filigrio", entry()).unwrap();
        assert_eq!(a, Action::Installed);
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("[mcp_servers.filigrio]"));
        assert!(
            after.contains("# an existing server I care about"),
            "the header comment `codex mcp add` deletes:\n{after}"
        );
        assert!(
            !after.ends_with('\n'),
            "install must not add a newline the user did not have:\n{after}"
        );

        let (a, _) = remove(&p, CONTAINER, "filigrio").unwrap();
        assert_eq!(a, Action::Removed);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            REAL_SHAPED,
            "byte-exact has no asterisk"
        );
    }

    /// The same file with a final newline: that one is the file's property too,
    /// in the other direction.
    #[test]
    fn a_config_that_ended_with_a_newline_still_does() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let prior = format!("{REAL_SHAPED}\n");
        std::fs::write(&p, &prior).unwrap();

        upsert(&p, CONTAINER, "filigrio", entry()).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().ends_with('\n'));
        remove(&p, CONTAINER, "filigrio").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    #[test]
    fn a_file_we_create_gets_a_trailing_newline_and_the_documented_header() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        upsert(&p, CONTAINER, "filigrio", entry()).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("[mcp_servers.filigrio]\n"), "got {text}");
        assert!(
            !text.contains("\n[mcp_servers]"),
            "no bare `[mcp_servers]` header we invented: {text}"
        );
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn a_file_we_created_is_deleted_on_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        upsert(&p, CONTAINER, "filigrio", entry()).unwrap();
        remove(&p, CONTAINER, "filigrio").unwrap();
        assert!(!p.exists());
    }

    /// A config holding nothing but the user's comments is not "empty".
    #[test]
    fn a_comments_only_config_is_written_back_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let prior = "# notes to self\n# nothing configured yet\n";
        std::fs::write(&p, prior).unwrap();

        upsert(&p, CONTAINER, "filigrio", entry()).unwrap();
        remove(&p, CONTAINER, "filigrio").unwrap();
        assert!(p.exists(), "their comments are content");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// A `[mcp_servers]` header the user wrote survives; one we implied does
    /// not. Both directions, because the difference is invisible in the value
    /// model and only exists in the text.
    #[test]
    fn an_explicit_container_header_survives_but_an_implied_one_does_not() {
        let dir = tempfile::tempdir().unwrap();

        let theirs = dir.path().join("theirs.toml");
        let prior = "model = \"o3\"\n\n# my servers live here\n[mcp_servers]\n";
        std::fs::write(&theirs, prior).unwrap();
        upsert(&theirs, CONTAINER, "filigrio", entry()).unwrap();
        remove(&theirs, CONTAINER, "filigrio").unwrap();
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), prior);

        let ours = dir.path().join("ours.toml");
        std::fs::write(&ours, "model = \"o3\"\n").unwrap();
        upsert(&ours, CONTAINER, "filigrio", entry()).unwrap();
        remove(&ours, CONTAINER, "filigrio").unwrap();
        assert_eq!(std::fs::read_to_string(&ours).unwrap(), "model = \"o3\"\n");
    }

    #[test]
    fn a_second_identical_install_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        upsert(&p, CONTAINER, "filigrio", entry()).unwrap();
        let once = std::fs::read_to_string(&p).unwrap();

        let (a, _) = upsert(&p, CONTAINER, "filigrio", entry()).unwrap();
        assert_eq!(a, Action::Unchanged);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), once);
    }

    #[test]
    fn a_changed_entry_is_an_update_not_an_install() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        upsert(&p, CONTAINER, "filigrio", entry()).unwrap();

        let mut changed = Table::new();
        changed["command"] = value("/elsewhere/filigrio-mcp");
        let (a, detail) = upsert(&p, CONTAINER, "filigrio", changed).unwrap();
        assert_eq!(a, Action::Updated, "{detail}");
        assert!(std::fs::read_to_string(&p).unwrap().contains("/elsewhere/"));
    }

    /// Malformed TOML is refused with the file and the position, and the user's
    /// bytes are untouched — `json_entry`'s posture, one format over.
    #[test]
    fn malformed_toml_is_refused_by_name_and_position_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let broken = "model = \"o3\"\n[mcp_servers\ncommand = \"x\"\n";
        std::fs::write(&p, broken).unwrap();

        let err = upsert(&p, CONTAINER, "filigrio", entry()).unwrap_err();
        assert!(matches!(err, InstallError::BadToml { .. }), "got {err}");
        let msg = err.to_string();
        assert!(msg.contains("config.toml"), "no file named: {msg}");
        assert!(msg.contains("line 2"), "no position: {msg}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken);
    }

    /// `mcp_servers = { … }` is legal TOML and not a shape we can extend
    /// without guessing at the user's style. Named, not half-handled.
    #[test]
    fn an_inline_table_container_is_refused_rather_than_half_handled() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let prior = "mcp_servers = { other = { command = \"npx\" } }\n";
        std::fs::write(&p, prior).unwrap();

        let err = upsert(&p, CONTAINER, "filigrio", entry()).unwrap_err();
        assert!(
            matches!(err, InstallError::NotTomlTable { .. }),
            "got {err}"
        );
        assert!(err.to_string().contains("inline table"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    #[test]
    fn removing_what_was_never_installed_is_absent_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        assert_eq!(remove(&p, CONTAINER, "filigrio").unwrap().0, Action::Absent);

        std::fs::write(&p, "model = \"o3\"\n").unwrap();
        assert_eq!(remove(&p, CONTAINER, "filigrio").unwrap().0, Action::Absent);

        std::fs::write(&p, "[mcp_servers.other]\ncommand = \"npx\"\n").unwrap();
        assert_eq!(remove(&p, CONTAINER, "filigrio").unwrap().0, Action::Absent);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[mcp_servers.other]\ncommand = \"npx\"\n"
        );
    }

    /// The entry as [`crate::clients::codex`] builds it, with the socket in it —
    /// the field a user actually changes, and the one an existence check cannot
    /// see move.
    fn entry_at(socket: &str) -> Table {
        let mut t = Table::new();
        t["command"] = value("/opt/g/bin/filigrio-mcp");
        let mut args = Array::new();
        args.push("--socket");
        args.push(socket);
        t["args"] = value(args);
        t
    }

    /// The three states. "Is the key there" answers `true` for a registration
    /// pointing at a socket that moved, which is the gap this closes.
    #[test]
    fn a_registration_that_no_longer_matches_reports_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let want = || entry_at("/run/filigrio.sock");

        assert_eq!(
            state(&p, CONTAINER, "filigrio", &want()).unwrap(),
            EntryState::Absent,
            "no file at all"
        );

        upsert(&p, CONTAINER, "filigrio", want()).unwrap();
        assert_eq!(
            state(&p, CONTAINER, "filigrio", &want()).unwrap(),
            EntryState::Current
        );
        assert_eq!(
            state(&p, CONTAINER, "filigrio", &want()).unwrap(),
            EntryState::Current,
            "asking twice must give the same answer"
        );

        upsert(
            &p,
            CONTAINER,
            "filigrio",
            entry_at("/run/user/1000/filigrio.sock"),
        )
        .unwrap();
        assert_eq!(
            state(&p, CONTAINER, "filigrio", &want()).unwrap(),
            EntryState::Stale
        );

        // A sibling server is not our registration.
        upsert(&p, CONTAINER, "filigrio", want()).unwrap();
        let mut theirs = Table::new();
        theirs["command"] = value("npx");
        upsert(&p, CONTAINER, "sentry", theirs).unwrap();
        assert_eq!(
            state(&p, CONTAINER, "filigrio", &want()).unwrap(),
            EntryState::Current
        );
    }

    /// **The false alarm this must never raise.** TOML has more spellings of one
    /// value than any other format here — literal vs basic strings, a multi-line
    /// array with a trailing comma, alignment whitespace, an inline table
    /// instead of a `[header]`, and a comment on the line. Our entry picks up
    /// whichever the surrounding document uses, so a rendered-text comparison
    /// would report the user's *style* as our staleness, on every run.
    ///
    /// Each fixture spells exactly what we would write, and each ends by proving
    /// it really is a file an install would have rewritten.
    #[test]
    fn an_entry_the_user_respelled_is_current_not_stale() {
        let want = || entry_at("/run/filigrio.sock");
        let cases = [
            (
                "literal strings, alignment and a comment",
                concat!(
                    "[mcp_servers.filigrio]\n",
                    "command   = '/opt/g/bin/filigrio-mcp'   # the stdio bridge\n",
                    "args      = ['--socket', '/run/filigrio.sock']\n"
                ),
            ),
            (
                "a multi-line array with a trailing comma",
                concat!(
                    "[mcp_servers.filigrio]\n",
                    "command = \"/opt/g/bin/filigrio-mcp\"\n",
                    "args = [\n",
                    "    \"--socket\",\n",
                    "    \"/run/filigrio.sock\",\n",
                    "]\n"
                ),
            ),
            (
                "our own keys in the other order",
                concat!(
                    "[mcp_servers.filigrio]\n",
                    "args = [\"--socket\", \"/run/filigrio.sock\"]\n",
                    "command = \"/opt/g/bin/filigrio-mcp\"\n"
                ),
            ),
            (
                "an inline table instead of a header",
                concat!(
                    "[mcp_servers]\n",
                    "filigrio = { command = \"/opt/g/bin/filigrio-mcp\", \
                     args = [\"--socket\", \"/run/filigrio.sock\"] }\n"
                ),
            ),
        ];

        for (name, prior) in cases {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("config.toml");
            std::fs::write(&p, prior).unwrap();

            assert_eq!(
                state(&p, CONTAINER, "filigrio", &want()).unwrap(),
                EntryState::Current,
                "{name}: the entry says exactly what we would write"
            );

            // The guard on the guard: a fixture an install leaves untouched
            // would pass the assertion above no matter how the comparison were
            // written.
            upsert(&p, CONTAINER, "filigrio", want()).unwrap();
            assert_ne!(
                std::fs::read_to_string(&p).unwrap(),
                prior,
                "{name}: an install left this file byte-identical, so the \
                 fixture no longer exercises the false alarm"
            );
        }
    }

    /// A registration that is *shaped* differently is stale, not merely
    /// respelled: a `command` that is an array (OpenCode's shape, one adapter
    /// over) is a different value, and so is a missing `args`.
    #[test]
    fn a_differently_shaped_entry_is_stale_rather_than_forgiven() {
        let want = || entry_at("/run/filigrio.sock");
        for prior in [
            "[mcp_servers.filigrio]\ncommand = [\"/opt/g/bin/filigrio-mcp\", \"--socket\", \"/run/filigrio.sock\"]\n",
            "[mcp_servers.filigrio]\ncommand = \"/opt/g/bin/filigrio-mcp\"\n",
            "[mcp_servers.filigrio]\ncommand = \"/opt/g/bin/filigrio-mcp\"\nargs = [\"--socket\", \"/run/filigrio.sock\"]\nenabled = true\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("config.toml");
            std::fs::write(&p, prior).unwrap();
            assert_eq!(
                state(&p, CONTAINER, "filigrio", &want()).unwrap(),
                EntryState::Stale,
                "got `current` for:\n{prior}"
            );
        }
    }
}
