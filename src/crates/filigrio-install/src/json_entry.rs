//! Reversible edits to a JSON config the user owns (`.mcp.json`).
//!
//! JSON carries no comments, so a managed *block* is not available here. The
//! reversibility unit is instead **one named key**: install adds
//! `mcpServers.filigrio`, uninstall removes exactly that key and nothing else.
//!
//! ## The container is a *path*, not a key
//!
//! Four of the five JSON registrations nest one level (`mcpServers.<name>`,
//! `mcp.<name>`); OpenClaw nests two (`mcp.servers.<name>`). So `container` is a
//! slice of segments walked in order, created on the way down and — this is the
//! half that makes uninstall reversible — **pruned innermost-first on the way
//! back up**, so a `{"mcp": {"servers": {…}}}` we created leaves nothing behind.
//! One module with a path parameter, not a second module: the shape of the edit
//! is identical and only the depth differs (ADR-0034 §14).
//!
//! ## What "byte-exact" means for JSON, honestly
//!
//! A managed block round-trips byte for byte because the untouched text is
//! never reparsed. JSON is reparsed, so the guarantee is narrower and stated
//! rather than implied:
//!
//! - **Key order is preserved** (`serde_json/preserve_order`), so a rewritten
//!   file reads as the user left it.
//! - **Every other key survives**, at every depth.
//! - **Indentation is normalized** to 2-space pretty — the format Claude Code's
//!   own documentation shows. A file already in that form round-trips byte for
//!   byte; one indented differently comes back re-indented, and nothing else
//!   changes.
//! - **The final newline is the file's, not ours.** A config that ended without
//!   one still does. This was a real defect until a real
//!   `~/.config/opencode/opencode.json` — which ends without a newline — was run
//!   through the round trip and came back one byte longer.
//! - **A file that held only our entry is deleted**, so a `.mcp.json` we created
//!   does not survive as an empty husk. That needs no recorded state: `{}` after
//!   removal means there was nothing but us.
//!
//! The oracle rewrote a user's config wholesale on a JSON parse error
//! (`install.py:678`: `except json.JSONDecodeError: settings = {}`), silently
//! discarding it. We refuse instead — [`InstallError::BadJson`] names the file
//! and the parse position, and the run reports the failure.

use crate::{
    had_trailing_newline, read_opt, remove_file, write_all, Action, EntryState, InstallError,
};
use serde_json::{Map, Value};
use std::path::Path;

/// Load a JSON object from `path`. Absent → an empty object. Malformed →
/// an error, never a silent reset.
fn load_object(path: &Path) -> Result<Map<String, Value>, InstallError> {
    let Some(text) = read_opt(path)? else {
        return Ok(Map::new());
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(&text).map_err(|source| InstallError::BadJson {
        path: path.to_path_buf(),
        source,
    })?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(InstallError::NotJsonObject {
            path: path.to_path_buf(),
            found: match other {
                Value::Array(_) => "array",
                Value::String(_) => "string",
                Value::Number(_) => "number",
                Value::Bool(_) => "boolean",
                _ => "null",
            },
        }),
    }
}

fn serialize(map: &Map<String, Value>, trailing_newline: bool) -> Result<String, InstallError> {
    let mut s = serde_json::to_string_pretty(&Value::Object(map.clone()))
        .map_err(|e| InstallError::Template(format!("serializing JSON config: {e}")))?;
    if trailing_newline {
        s.push('\n');
    }
    Ok(s)
}

/// `["mcp", "servers"]` → `mcp.servers`, for the detail lines a user reads.
fn dotted(container: &[&str], key: &str) -> String {
    let mut s = String::new();
    for seg in container {
        s.push_str(seg);
        s.push('.');
    }
    s.push_str(key);
    s
}

/// Walk `container` immutably. `None` if any segment is missing or is not an
/// object.
fn resolve<'a>(root: &'a Map<String, Value>, container: &[&str]) -> Option<&'a Map<String, Value>> {
    let mut node = root;
    for seg in container {
        node = node.get(*seg)?.as_object()?;
    }
    Some(node)
}

/// Remove `key` at the end of `container`, then drop every container the
/// removal left empty, innermost first. Returns whether the key was there.
///
/// The prune is what makes a nested install reversible: a `{"mcp": {"servers":
/// {}}}` husk is not "the rest preserved", it is our footprint with the entry
/// filed off. The one case it gets wrong is named in ADR-0034 §14 — an *empty*
/// container the user wrote themselves is indistinguishable from one we made,
/// because JSON has no equivalent of `toml_edit`'s implicit-table flag.
fn remove_at(node: &mut Map<String, Value>, container: &[&str], key: &str) -> bool {
    let Some((head, rest)) = container.split_first() else {
        return node.remove(key).is_some();
    };
    let Some(child) = node.get_mut(*head).and_then(Value::as_object_mut) else {
        return false;
    };
    let removed = remove_at(child, rest, key);
    if removed && child.is_empty() {
        node.remove(*head);
    }
    removed
}

/// Insert or refresh `container.key = entry` in the JSON object at `path`.
pub fn upsert(
    path: &Path,
    container: &[&str],
    key: &str,
    entry: Value,
) -> Result<(Action, String), InstallError> {
    let before = read_opt(path)?;
    let mut root = load_object(path)?;

    let had = resolve(&root, container).is_some_and(|m| m.contains_key(key));

    let mut node = &mut root;
    for seg in container {
        let slot = node
            .entry((*seg).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        node = match slot.as_object_mut() {
            Some(m) => m,
            None => {
                return Err(InstallError::NotJsonObject {
                    path: path.to_path_buf(),
                    found: "non-object",
                })
            }
        };
    }
    node.insert(key.to_string(), entry);

    let dotted = dotted(container, key);
    let text = serialize(&root, had_trailing_newline(before.as_deref()))?;
    if before.as_deref() == Some(text.as_str()) {
        return Ok((Action::Unchanged, format!("{dotted} already current")));
    }
    write_all(path, &text)?;
    Ok(if had {
        (Action::Updated, format!("{dotted} refreshed"))
    } else if before.is_some() {
        (Action::Installed, format!("{dotted} added"))
    } else {
        (Action::Installed, "file created".into())
    })
}

/// Remove `container.key`. Deletes the file when nothing else is left.
pub fn remove(
    path: &Path,
    container: &[&str],
    key: &str,
) -> Result<(Action, String), InstallError> {
    let Some(before) = read_opt(path)? else {
        return Ok((Action::Absent, "no such file".into()));
    };
    let mut root = load_object(path)?;
    let dotted = dotted(container, key);

    if !remove_at(&mut root, container, key) {
        return Ok((Action::Absent, format!("no `{dotted}` entry")));
    }

    if root.is_empty() {
        return Ok((Action::Removed, remove_file(path)?.detail("entry")));
    }
    write_all(
        path,
        &serialize(&root, had_trailing_newline(Some(&before)))?,
    )?;
    Ok((Action::Removed, format!("{dotted} removed, rest preserved")))
}

/// Is the entry at `container.key` the one install would write?
///
/// **The entry, compared as a value — never the file, compared as bytes.** The
/// whole-file version is one line shorter and permanently wrong here: this
/// module normalizes indentation to 2-space pretty (see the module docs), so
/// re-rendering a config the user indents with four spaces and diffing it
/// against the file reports `stale` on every run, for a difference our
/// registration had no part in. `serde_json::Value` compares structurally, and
/// with `preserve_order` its object comparison is still key-order independent —
/// so a user who reordered the keys inside our entry is told the truth, which is
/// that their client reads exactly what we would write.
///
/// See [`EntryState`] for the same argument in the other two formats.
pub fn state(
    path: &Path,
    container: &[&str],
    key: &str,
    expected: &Value,
) -> Result<EntryState, InstallError> {
    if read_opt(path)?.is_none() {
        return Ok(EntryState::Absent);
    }
    let root = load_object(path)?;
    let Some(found) = resolve(&root, container).and_then(|node| node.get(key)) else {
        return Ok(EntryState::Absent);
    };
    Ok(if found == expected {
        EntryState::Current
    } else {
        EntryState::Stale
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MCP_SERVERS: &[&str] = &["mcpServers"];

    fn entry() -> Value {
        json!({"type": "stdio", "command": "/bin/filigrio-mcp", "args": [], "env": {}})
    }

    /// The daemon socket a user passes `--socket`, spelled once so the fixtures
    /// below and the entry they compare against cannot drift apart.
    const SOCKET: &str = "/run/filigrio.sock";

    /// The entry as an adapter really builds it: the bridge, and the socket it
    /// is told to talk to. The socket is the field that moves — it is what
    /// `--socket` writes and what a `status` that only checks for the key's
    /// existence cannot see change.
    fn entry_at(socket: &str) -> Value {
        json!({
            "type": "stdio",
            "command": "/bin/filigrio-mcp",
            "args": ["--socket", socket],
            "env": {}
        })
    }

    #[test]
    fn upsert_then_remove_restores_a_users_file_byte_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        // Already in the canonical 2-space form Claude Code's docs show.
        let prior = "{\n  \"mcpServers\": {\n    \"theirs\": {\n      \"type\": \"stdio\",\n      \"command\": \"other\"\n    }\n  }\n}\n";
        std::fs::write(&p, prior).unwrap();

        let (a, _) = upsert(&p, MCP_SERVERS, "filigrio", entry()).unwrap();
        assert_eq!(a, Action::Installed);
        assert!(std::fs::read_to_string(&p).unwrap().contains("filigrio"));

        let (a, _) = remove(&p, MCP_SERVERS, "filigrio").unwrap();
        assert_eq!(a, Action::Removed);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// A real `opencode.json` ends without a newline. Adding one would make the
    /// round trip one byte long — "byte-exact" has no asterisk.
    #[test]
    fn a_config_that_ended_without_a_newline_still_does() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("opencode.json");
        let prior = "{\n  \"share\": \"disabled\",\n  \"mcp\": {\n    \"theirs\": {\n      \"type\": \"local\",\n      \"command\": [\n        \"npx\"\n      ]\n    }\n  }\n}";
        assert!(!prior.ends_with('\n'));
        std::fs::write(&p, prior).unwrap();

        upsert(&p, &["mcp"], "filigrio", entry()).unwrap();
        assert!(
            !std::fs::read_to_string(&p).unwrap().ends_with('\n'),
            "install must not add a newline the user did not have"
        );

        remove(&p, &["mcp"], "filigrio").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// The other direction: a file we create, and one that already ends in a
    /// newline, both keep the POSIX form.
    #[test]
    fn a_file_we_create_gets_a_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        upsert(&p, MCP_SERVERS, "filigrio", entry()).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().ends_with("}\n"));
    }

    #[test]
    fn a_second_identical_install_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        upsert(&p, MCP_SERVERS, "filigrio", entry()).unwrap();
        let once = std::fs::read_to_string(&p).unwrap();

        let (a, _) = upsert(&p, MCP_SERVERS, "filigrio", entry()).unwrap();
        assert_eq!(a, Action::Unchanged);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), once);
    }

    #[test]
    fn a_file_we_created_is_deleted_on_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        upsert(&p, MCP_SERVERS, "filigrio", entry()).unwrap();
        remove(&p, MCP_SERVERS, "filigrio").unwrap();
        assert!(!p.exists());
    }

    /// Unrelated top-level keys and sibling servers are the user's; they must
    /// survive both directions untouched, and in their original order.
    #[test]
    fn sibling_keys_and_their_order_survive() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        std::fs::write(
            &p,
            r#"{"zebra": 1, "mcpServers": {"alpha": {"command": "a"}}, "apple": {"deep": [1,2]}}"#,
        )
        .unwrap();

        upsert(&p, MCP_SERVERS, "filigrio", entry()).unwrap();
        remove(&p, MCP_SERVERS, "filigrio").unwrap();

        let after = std::fs::read_to_string(&p).unwrap();
        let keys: Vec<String> = serde_json::from_str::<Map<String, Value>>(&after)
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["zebra", "mcpServers", "apple"]);
        assert!(after.contains("\"alpha\""));
        assert!(after.contains("\"deep\""));
        assert!(!after.contains("filigrio"));
    }

    /// Malformed JSON is refused, not reset. The oracle's `except
    /// JSONDecodeError: settings = {}` silently discarded the user's file.
    #[test]
    fn malformed_json_is_refused_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        let broken = "{ this is not json";
        std::fs::write(&p, broken).unwrap();

        let err = upsert(&p, MCP_SERVERS, "filigrio", entry()).unwrap_err();
        assert!(matches!(err, InstallError::BadJson { .. }), "got {err}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            broken,
            "the user's file must be untouched"
        );
    }

    #[test]
    fn removing_what_was_never_installed_is_absent_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        assert_eq!(
            remove(&p, MCP_SERVERS, "filigrio").unwrap().0,
            Action::Absent
        );

        std::fs::write(&p, "{\n  \"other\": 1\n}\n").unwrap();
        assert_eq!(
            remove(&p, MCP_SERVERS, "filigrio").unwrap().0,
            Action::Absent
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "{\n  \"other\": 1\n}\n"
        );
    }

    /// The three states, and the middle one is why this function exists: "is
    /// the key there" is `Present` for a registration pointing at a socket that
    /// moved three releases ago.
    #[test]
    fn a_registration_that_no_longer_matches_reports_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        let want = entry_at(SOCKET);

        assert_eq!(
            state(&p, MCP_SERVERS, "filigrio", &want).unwrap(),
            EntryState::Absent,
            "no file at all"
        );

        upsert(&p, MCP_SERVERS, "filigrio", want.clone()).unwrap();
        assert_eq!(
            state(&p, MCP_SERVERS, "filigrio", &want).unwrap(),
            EntryState::Current
        );
        assert_eq!(
            state(&p, MCP_SERVERS, "filigrio", &want).unwrap(),
            EntryState::Current,
            "asking a second time must give the same answer — a `status` that \
             flips on its own is worse than one that never spoke"
        );

        // The realistic way this goes wrong: the socket moved, so the entry on
        // disk points at a path with nothing listening on it.
        upsert(
            &p,
            MCP_SERVERS,
            "filigrio",
            entry_at("/run/user/1000/filigrio.sock"),
        )
        .unwrap();
        assert_eq!(
            state(&p, MCP_SERVERS, "filigrio", &want).unwrap(),
            EntryState::Stale
        );

        // And a sibling server changing is not our registration changing.
        upsert(&p, MCP_SERVERS, "filigrio", want.clone()).unwrap();
        upsert(&p, MCP_SERVERS, "theirs", json!({"command": "npx"})).unwrap();
        assert_eq!(
            state(&p, MCP_SERVERS, "filigrio", &want).unwrap(),
            EntryState::Current
        );
    }

    /// **The false alarm this must never raise.**
    ///
    /// The tempting implementation is "re-render what `upsert` would write and
    /// diff it against the file" — `upsert` computes exactly that. It is wrong
    /// here, and permanently: this module re-emits at 2-space pretty (see the
    /// module docs), so a config the user indents with four spaces, or a tab, or
    /// keeps on one line *would* be rewritten by an install — for a reason our
    /// registration has no part in. That reports `stale` on every run and
    /// re-running install does not clear it; it only reformats their file.
    ///
    /// Each fixture holds exactly the entry we would write, spelled the user's
    /// way, and each ends by proving the file really is one a whole-file
    /// comparison would have called stale.
    #[test]
    fn a_config_the_user_formats_their_own_way_is_current_not_stale() {
        let want = entry_at(SOCKET);
        let cases = [
            (
                "four-space indentation",
                concat!(
                    "{\n",
                    "    \"mcpServers\": {\n",
                    "        \"filigrio\": {\n",
                    "            \"type\": \"stdio\",\n",
                    "            \"command\": \"/bin/filigrio-mcp\",\n",
                    "            \"args\": [\"--socket\", \"/run/filigrio.sock\"],\n",
                    "            \"env\": {}\n",
                    "        }\n",
                    "    }\n",
                    "}\n"
                ),
            ),
            (
                "tab indentation",
                concat!(
                    "{\n",
                    "\t\"mcpServers\": {\n",
                    "\t\t\"filigrio\": {\n",
                    "\t\t\t\"type\": \"stdio\",\n",
                    "\t\t\t\"command\": \"/bin/filigrio-mcp\",\n",
                    "\t\t\t\"args\": [\"--socket\", \"/run/filigrio.sock\"],\n",
                    "\t\t\t\"env\": {}\n",
                    "\t\t}\n",
                    "\t}\n",
                    "}\n"
                ),
            ),
            (
                "no whitespace at all",
                "{\"mcpServers\":{\"filigrio\":{\"type\":\"stdio\",\"command\":\"/bin/filigrio-mcp\",\"args\":[\"--socket\",\"/run/filigrio.sock\"],\"env\":{}}}}",
            ),
            (
                "our own keys in another order",
                concat!(
                    "{\n",
                    "  \"mcpServers\": {\n",
                    "    \"filigrio\": {\n",
                    "      \"env\": {},\n",
                    "      \"args\": [\"--socket\", \"/run/filigrio.sock\"],\n",
                    "      \"command\": \"/bin/filigrio-mcp\",\n",
                    "      \"type\": \"stdio\"\n",
                    "    }\n",
                    "  }\n",
                    "}\n"
                ),
            ),
        ];

        for (name, prior) in cases {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join(".mcp.json");
            std::fs::write(&p, prior).unwrap();

            assert_eq!(
                state(&p, MCP_SERVERS, "filigrio", &want).unwrap(),
                EntryState::Current,
                "{name}: the entry is exactly ours, so the registration is current"
            );

            // The guard on the guard: this fixture must really be one that a
            // whole-file comparison calls stale, or the assertion above proves
            // nothing about the trap it exists to hold shut.
            upsert(&p, MCP_SERVERS, "filigrio", want.clone()).unwrap();
            assert_ne!(
                std::fs::read_to_string(&p).unwrap(),
                prior,
                "{name}: an install left this file byte-identical, so the \
                 fixture no longer exercises the false alarm"
            );
        }
    }

    /// OpenClaw's depth (ADR-0034 §14): `mcp.servers.<name>`. A sibling server
    /// and unrelated top-level keys survive both directions, and the file comes
    /// back byte for byte.
    const NESTED: &[&str] = &["mcp", "servers"];

    #[test]
    fn a_two_level_container_round_trips_with_its_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("openclaw.json");
        let prior = concat!(
            "{\n",
            "  \"theme\": \"dark\",\n",
            "  \"mcp\": {\n",
            "    \"servers\": {\n",
            "      \"sentry\": {\n",
            "        \"command\": \"npx\",\n",
            "        \"args\": [\n",
            "          \"-y\",\n",
            "          \"@sentry/mcp\"\n",
            "        ]\n",
            "      }\n",
            "    }\n",
            "  }\n",
            "}\n"
        );
        std::fs::write(&p, prior).unwrap();

        let (a, detail) = upsert(&p, NESTED, "filigrio", entry()).unwrap();
        assert_eq!(a, Action::Installed);
        assert_eq!(detail, "mcp.servers.filigrio added", "the path, dotted");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["mcp"]["servers"]["filigrio"].is_object());
        assert!(v["mcp"]["servers"]["sentry"].is_object());
        assert!(
            v["mcpServers"].is_null(),
            "a one-level key here means a flat adapter's shape leaked in"
        );

        assert_eq!(
            state(&p, MCP_SERVERS, "filigrio", &entry()).unwrap(),
            EntryState::Absent,
            "the depth is part of the address: a one-level lookup must not find \
             the two-level entry"
        );

        let (a, _) = remove(&p, NESTED, "filigrio").unwrap();
        assert_eq!(a, Action::Removed);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// Both containers we invented are pruned, innermost first — a
    /// `{"mcp":{"servers":{}}}` husk is our footprint, not "the rest".
    #[test]
    fn containers_we_created_are_pruned_from_the_inside_out() {
        let dir = tempfile::tempdir().unwrap();

        // Nothing else in the file: it goes entirely.
        let ours = dir.path().join("ours.json");
        upsert(&ours, NESTED, "filigrio", entry()).unwrap();
        remove(&ours, NESTED, "filigrio").unwrap();
        assert!(!ours.exists());

        // A file with an unrelated key keeps that key and loses both of ours.
        let mixed = dir.path().join("mixed.json");
        std::fs::write(&mixed, "{\n  \"theme\": \"dark\"\n}\n").unwrap();
        upsert(&mixed, NESTED, "filigrio", entry()).unwrap();
        remove(&mixed, NESTED, "filigrio").unwrap();
        let after = std::fs::read_to_string(&mixed).unwrap();
        assert_eq!(after, "{\n  \"theme\": \"dark\"\n}\n", "no husk: {after}");
    }

    /// The prune stops at the first container that still holds something.
    #[test]
    fn a_container_with_a_sibling_server_is_not_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("openclaw.json");
        upsert(&p, NESTED, "theirs", json!({"command": "npx"})).unwrap();
        upsert(&p, NESTED, "filigrio", entry()).unwrap();
        remove(&p, NESTED, "filigrio").unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["mcp"]["servers"]["theirs"].is_object());
        assert!(v["mcp"]["servers"]["filigrio"].is_null());
    }

    /// The one case the prune gets wrong, pinned rather than left to be
    /// discovered: an *empty* container the user wrote is indistinguishable
    /// from one we created — JSON has no equivalent of `toml_edit`'s
    /// implicit-table flag — so it goes with ours. Named in ADR-0034 §14.
    #[test]
    fn an_empty_container_the_user_wrote_is_pruned_too_the_known_gap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("openclaw.json");
        let prior = "{\n  \"mcp\": {\n    \"servers\": {}\n  }\n}\n";
        std::fs::write(&p, prior).unwrap();

        upsert(&p, NESTED, "filigrio", entry()).unwrap();
        remove(&p, NESTED, "filigrio").unwrap();

        assert!(
            !p.exists(),
            "documented behaviour: an empty container carries no user content, \
             so it is pruned with ours and the emptied file is deleted"
        );
    }

    /// A segment that is not an object is refused, not overwritten.
    #[test]
    fn a_non_object_on_the_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("openclaw.json");
        let prior = "{\n  \"mcp\": \"disabled\"\n}\n";
        std::fs::write(&p, prior).unwrap();

        let err = upsert(&p, NESTED, "filigrio", entry()).unwrap_err();
        assert!(
            matches!(err, InstallError::NotJsonObject { .. }),
            "got {err}"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }
}
