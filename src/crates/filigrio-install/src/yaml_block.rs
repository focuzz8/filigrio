//! A named entry inside a YAML mapping, spliced as **text** (ADR-0034 §15).
//!
//! This is the third config format, and deliberately **not** a third *editable*
//! value model. `json_entry` and `toml_entry` can parse-and-re-emit because
//! `serde_json` preserves key order and `toml_edit` preserves everything; YAML
//! has no such editor in Rust — `serde_yaml` is deprecated and round-trips
//! through a value model, which destroys comments and formatting. Shipping that
//! would make us the fourth tool in this ADR to reformat a user's config, and
//! the other three are all recorded as defects.
//!
//! There *is* a value model here ([`Node`]), and it is read-only: [`state`] has
//! to answer "is this our registration" by value, and nothing in this module
//! can write through it. The distinction is the whole design — a value model
//! you emit from reformats the user's file; a value model you only compare
//! against reformats nothing.
//!
//! So the mechanism is [`crate::block`]'s marker-delimited managed block, which
//! is byte-exact *by construction* because it never parses the file — it
//! splices a text region. What this module adds is only **where** the region
//! goes, which for YAML is indentation-sensitive:
//!
//! ```yaml
//! mcp_servers:
//!
//! # filigrio:start — …
//!   filigrio:
//!     command: "…"
//! # filigrio:end
//!   sentry:
//!     command: npx
//! ```
//!
//! Three shapes, all measured against the live parser rather than assumed:
//!
//! - **`mcp_servers:` absent** — the block is appended at EOF and carries the
//!   `mcp_servers:` line itself.
//! - **present with entries** — the block is spliced immediately after the
//!   `mcp_servers:` line, indented to match the indent those entries already
//!   use (so a 4-space config gets a 4-space entry, and ours never becomes a
//!   *child* of theirs).
//! - **present and empty** — same splice, at the default two-space indent.
//!
//! The markers stay at column 0. A comment is lexical in YAML and does not
//! terminate a block mapping at any column: `hermes mcp list` reads a config
//! with column-0 markers *inside* `mcp_servers:` and lists both servers. That
//! was verified before this module was written, not after.
//!
//! ## Locating with a parser, writing with a splice
//!
//! *Where* the region goes is answered by [`saphyr_parser`], whose `Marker`
//! carries a **byte index** per event. *How* it is written is unchanged: a text
//! splice that never re-emits, which is what makes the round trip byte-exact by
//! construction. Those two halves are separable, and keeping them separate is
//! the whole design — `saphyr` has no comment-preserving emitter, so parsing and
//! re-emitting would strip the user's comments and put us in the same table as
//! the three vendor CLIs below.
//!
//! This replaced a hand-rolled locator that matched the container key at column
//! zero by string prefix and found entries by counting indentation. That was
//! measurably wrong, not merely fragile:
//!
//! - `"mcp_servers":` is the **same key** as `mcp_servers:`, and `strip_prefix`
//!   did not think so — so an install against a quoted config appended a second
//!   top-level `mcp_servers:`, a duplicate key that YAML 1.2 makes an error.
//! - A `filigrio:` entry under somebody *else's* top-level mapping matched on
//!   indentation alone, because nothing tracked whose mapping it was inside.
//! - A multi-document file was spliced at whichever `mcp_servers:` came first,
//!   in whatever document; loaders disagree about which document they read, so
//!   that is now a refusal rather than a guess.
//!
//! A parser reports a scalar's **value**, not its spelling, and its nesting says
//! which mapping a key belongs to — so quoting, anchors, tags and flow style
//! stop being cases to enumerate and start being someone else's problem. Three
//! shapes are still refused by name and left untouched, because splicing into
//! them would mean re-emitting a line: an inline `mcp_servers: {}`, a
//! multi-document file, and YAML that does not parse.
//!
//! ## The failure this module exists to survive
//!
//! Hermes' own writer **strips every comment in the file** — measured on a plain
//! `hermes mcp remove`, which is not even the command that installs anything: a
//! 20-line hand-written config came back 48 lines with both user comments gone,
//! both of our markers gone, `_config_version: 33` injected, and pages of
//! commented-out template boilerplate appended. Our *entry* survived that; our
//! *markers* did not.
//!
//! A marker-only uninstall would therefore report "nothing of ours here" and
//! leave a live `filigrio:` entry pointing at a binary the user just deleted —
//! Hermes would go on trying to spawn it. So removal has a second, marker-
//! independent path: find the `filigrio:` entry by name and excise exactly its
//! lines. Install uses it too, so a re-install after a vendor rewrite replaces
//! the orphan instead of writing a duplicate key into the same mapping.
//!
//! Read paths do not rewrite: `hermes mcp list` left a hand-written config byte
//! for byte. So the markers survive ordinary use, and the fallback is for the
//! case where they did not.

use crate::block::{self, Markers};
use crate::{read_opt, remove_file, write_all, Action, EntryState, InstallError};
use std::collections::BTreeMap;
use std::path::Path;

/// The indent used for a mapping we create, and for one whose entries we cannot
/// see because it is empty. Two spaces is what Hermes' own writer emits.
const DEFAULT_INDENT: usize = 2;

/// A located block mapping: where its entries begin, and how far they are
/// indented.
#[derive(Debug, Clone, Copy)]
struct Mapping {
    /// Byte offset just past the `container:` line's newline.
    body_start: usize,
    /// Indent, in spaces, that this mapping's entries use.
    indent: usize,
}

/// Why a document could not be located in.
#[derive(Debug)]
enum LocateError {
    /// `mcp_servers: {…}` — legal YAML, but splicing into it would mean
    /// re-emitting the line.
    Inline,
    /// More than one YAML document in the file. Which one holds the config is
    /// not ours to guess.
    MultiDocument,
    /// The file is not YAML we can parse.
    Parse(String),
}

/// The byte offset of the start of the line containing `index`.
fn line_start(content: &str, index: usize) -> usize {
    content[..index].rfind('\n').map_or(0, |i| i + 1)
}

/// The byte offset just past the newline that ends the line containing `index`
/// (EOF if the line is unterminated).
fn line_end(content: &str, index: usize) -> usize {
    content[index..]
        .find('\n')
        .map_or(content.len(), |i| index + i + 1)
}

/// Where we are in the event stream: YAML mappings alternate key and value, and
/// only the parser's nesting tells us which a given scalar is.
enum Ctx {
    Map { expect_key: bool },
    Seq,
}

/// A YAML value reduced to what a comparison can honestly be about: content,
/// and nothing about how it was spelled.
///
/// This is the smallest thing that makes [`state`] answer the question
/// [`EntryState`] says it must — "is filigrio's registration current", by value
/// — for the one format with no value model in the crate. It is **not** a
/// document model and nothing writes through it: the splice still never
/// re-emits (see the module docs). It exists only to be compared.
///
/// Mappings are a `BTreeMap`, so key order is not part of the answer — `args`
/// before `command` is the same registration. Sequences are a `Vec`, because
/// the order of `args` is the difference between `--socket /run/x` and
/// something that will not start.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    /// The scalar's **content**, with quoting and escapes already resolved by
    /// the parser — `"--socket"`, `'--socket'` and `--socket` are one value.
    ///
    /// Deliberately not type-resolved: a plain `true` and a quoted `"true"`
    /// compare equal here, though YAML says one is a boolean and the other a
    /// string. Telling them apart would mean a YAML 1.2 type resolver — a
    /// second value model, which is the thing this module exists not to have —
    /// and the entries this crate writes are strings and nothing else.
    Scalar(String),
    Seq(Vec<Node>),
    Map(BTreeMap<Node, Node>),
    /// `*anchor`. We never write one, and this module does not follow one.
    /// Recording it as its own shape keeps a config that uses an alias honestly
    /// *different* from our body — `stale`, which sends the user to an install
    /// that replaces it — rather than accidentally equal to it.
    Alias(usize),
}

/// A collection that has been opened and is waiting for its children.
enum Building {
    Seq(Vec<Node>),
    /// The pending key, once a mapping has seen one and not yet its value.
    Map(BTreeMap<Node, Node>, Option<Node>),
}

/// Assembles the [`Node`] for one value out of the events that follow its key.
///
/// Fed every event after the key it belongs to, and says when it is finished —
/// which is what lets [`walk`] collect a value in the same pass that locates it,
/// rather than parsing the document a second time with a second set of rules to
/// keep in agreement with the first.
#[derive(Default)]
struct NodeBuilder {
    open: Vec<Building>,
}

impl NodeBuilder {
    /// Feed one event; `Some(node)` once the value is complete.
    fn feed(&mut self, event: &saphyr_parser::Event) -> Option<Node> {
        use saphyr_parser::Event as E;
        let finished = match event {
            E::Scalar(value, ..) => Node::Scalar(value.to_string()),
            E::Alias(id) => Node::Alias(*id),
            E::MappingStart(..) => {
                self.open.push(Building::Map(BTreeMap::new(), None));
                return None;
            }
            E::SequenceStart(..) => {
                self.open.push(Building::Seq(Vec::new()));
                return None;
            }
            E::MappingEnd | E::SequenceEnd => match self.open.pop() {
                Some(Building::Seq(items)) => Node::Seq(items),
                Some(Building::Map(entries, _)) => Node::Map(entries),
                // Our key's *parent* closed before the value opened, so the
                // value was never written: `filigrio:` and nothing under it.
                None => Node::Scalar(String::new()),
            },
            // Stream and document framing says nothing about a value.
            E::StreamStart | E::StreamEnd | E::DocumentStart(_) | E::DocumentEnd | E::Nothing => {
                return None
            }
        };
        self.place(finished)
    }

    /// File a completed node into whatever is open above it — or return it, if
    /// nothing is, because then it *is* the value we were sent to collect.
    fn place(&mut self, node: Node) -> Option<Node> {
        match self.open.last_mut() {
            None => Some(node),
            Some(Building::Seq(items)) => {
                items.push(node);
                None
            }
            Some(Building::Map(entries, pending)) => {
                match pending.take() {
                    None => *pending = Some(node),
                    Some(key) => {
                        entries.insert(key, node);
                    }
                }
                None
            }
        }
    }
}

/// What one walk of the document found.
struct Found {
    mapping: Option<Mapping>,
    /// Byte range of the `key` entry inside the container, if it is there.
    entry: Option<(usize, usize)>,
    /// The `key` entry's **value**, for [`state`] to compare. Collected in the
    /// same pass that finds the range, because two passes would be two places
    /// to disagree about which `filigrio:` is ours.
    value: Option<Node>,
}

/// Walk the document once, locating both the container mapping and our entry
/// inside it.
///
/// A parser and never string heuristics, because a parser reports a scalar's
/// **value** rather than its spelling — `"mcp_servers":` is the same key as
/// `mcp_servers:` — and its nesting says which mapping a key belongs to, so
/// quoting, anchors, tags and flow style are not cases to enumerate (the module
/// docs record the defects the heuristics shipped). Nothing here re-emits YAML:
/// the parser is asked *where*, the answer is a byte offset, and the write is
/// still a text splice — `saphyr` has no comment-preserving emitter, and using
/// one would be the `serde_yaml` mistake this module exists to avoid.
fn walk(content: &str, container: &str, key: &str) -> Result<Found, LocateError> {
    use saphyr_parser::Event as E;

    let mut parser = saphyr_parser::Parser::new_from_str(content);
    let mut stack: Vec<Ctx> = Vec::new();
    let mut docs = 0usize;

    let mut mapping = None;
    let mut entry = None;
    let mut entry_value = None;
    // Collecting our entry's value, from the event after its key until its
    // subtree closes. `None` before and after.
    let mut builder: Option<NodeBuilder> = None;

    // Byte offset just past the container key, once we have seen it.
    let mut container_key_end: Option<usize> = None;
    // Stack depth *inside* the container's own mapping. Set when we enter it,
    // cleared when we leave — without this, a `filigrio:` key under some other
    // top-level mapping would be mistaken for ours.
    let mut inside: Option<usize> = None;
    // (start of the entry's first line, depth it returns to, last text byte).
    let mut scan: Option<(usize, usize, usize)> = None;
    // A refusal is *recorded* rather than returned, so that a file which is also
    // malformed reports the parse error instead. "This config is broken" is the
    // more useful sentence, and it is the one the user can act on first.
    let mut refusal: Option<LocateError> = None;

    while let Some(next) = parser.next_event() {
        let (event, span) = next.map_err(|e| LocateError::Parse(e.to_string()))?;
        let (start, end) = (span.start.index(), span.end.index());
        // A zero-width event consumed no source — a block `MappingStart`, an
        // implicit null, every `*End`. Only text-bearing events may extend an
        // entry's range, which is what stops removal from swallowing the blank
        // line or the comment that follows it. The old code did that with a
        // "provisional blank line" rule; here it falls out of the spans.
        let bears_text = end > start;
        // Whether an entry was already open *before* this event. Without it the
        // scan would close on the very event that opened it, and every entry
        // would be one line long. The builder needs the same guard for the same
        // reason: the key's own event is not part of the key's value.
        let scanning = scan.is_some();
        let building = builder.is_some();
        if let Some((_, _, last)) = scan.as_mut() {
            if bears_text {
                *last = (*last).max(end);
            }
        }

        match &event {
            E::DocumentStart(_) => {
                docs += 1;
                if docs > 1 {
                    return Err(LocateError::MultiDocument);
                }
            }

            E::Scalar(value, ..) => {
                let is_key = matches!(stack.last(), Some(Ctx::Map { expect_key: true }));
                if let Some(Ctx::Map { expect_key }) = stack.last_mut() {
                    *expect_key = !*expect_key;
                }

                if is_key && stack.len() == 1 && value.as_ref() == container {
                    container_key_end = Some(end);
                } else if is_key && Some(stack.len()) == inside && value.as_ref() == key {
                    scan = Some((line_start(content, start), stack.len(), end));
                    builder = Some(NodeBuilder::default());
                } else if !is_key && stack.len() == 1 && mapping.is_none() {
                    // The container key's value is a scalar. An *implicit null*
                    // — `mcp_servers:` with nothing under it — is the empty
                    // mapping, spliced at the default indent. A scalar that
                    // actually spells something (`mcp_servers: off`) is not a
                    // mapping at all, and is refused rather than half-handled.
                    if let Some(key_end) = container_key_end {
                        if bears_text {
                            refusal = Some(LocateError::Inline);
                            container_key_end = None;
                        } else {
                            mapping = Some(Mapping {
                                body_start: line_end(content, key_end),
                                indent: DEFAULT_INDENT,
                            });
                        }
                    }
                }
            }

            E::MappingStart(..) | E::SequenceStart(..) => {
                let opening_value = matches!(stack.last(), Some(Ctx::Map { expect_key: false }));
                if let Some(Ctx::Map { expect_key }) = stack.last_mut() {
                    *expect_key = true;
                }
                let is_mapping = matches!(event, E::MappingStart(..));

                if opening_value && stack.len() == 1 && mapping.is_none() {
                    if let Some(key_end) = container_key_end {
                        // A *flow* collection consumed its `{` or `[`, so it
                        // bears text. That is the one shape this module will not
                        // splice into, because doing so means re-emitting a line.
                        if !is_mapping || bears_text {
                            refusal = Some(LocateError::Inline);
                            container_key_end = None;
                        } else {
                            mapping = Some(Mapping {
                                body_start: line_end(content, key_end),
                                // The first entry's own offset from its line
                                // start *is* the indent — no scanning for "the
                                // first line that says something about
                                // structure".
                                indent: start - line_start(content, start),
                            });
                            inside = Some(stack.len() + 1);
                        }
                    }
                }

                stack.push(if is_mapping {
                    Ctx::Map { expect_key: true }
                } else {
                    Ctx::Seq
                });
            }

            E::MappingEnd | E::SequenceEnd => {
                stack.pop();
                if inside.is_some_and(|d| stack.len() < d) {
                    inside = None;
                }
            }

            // An alias, or anything else, occupies whichever slot is next.
            E::Alias(_) => {
                if let Some(Ctx::Map { expect_key }) = stack.last_mut() {
                    *expect_key = !*expect_key;
                }
            }

            E::StreamStart | E::StreamEnd | E::DocumentEnd | E::Nothing => {}
        }

        // Everything after our key, until its value is whole. `building` was
        // read before the match, so the key's own event is not fed to it.
        if building {
            if let Some(b) = builder.as_mut() {
                if let Some(node) = b.feed(&event) {
                    entry_value = Some(node);
                    builder = None;
                }
            }
        }

        // The entry's subtree is finished once the stack is back at or below the
        // depth its key sat at. Its range ends at the end of the line holding
        // its last text-bearing byte.
        if let (true, Some((entry_start, depth, last))) = (scanning, scan) {
            let closed = stack.len() < depth
                || (stack.len() == depth
                    && matches!(event, E::Scalar(..) | E::MappingEnd | E::SequenceEnd));
            if closed {
                entry = Some((entry_start, line_end(content, last)));
                scan = None;
            }
        }
    }

    if let Some(r) = refusal {
        return Err(r);
    }

    // An entry whose mapping ran to the end of the stream.
    if let Some((entry_start, _, last)) = scan {
        entry = Some((entry_start, line_end(content, last)));
    }

    Ok(Found {
        mapping,
        entry,
        value: entry_value,
    })
}

/// The container mapping, if this document has one.
fn locate_mapping(content: &str, container: &str) -> Result<Option<Mapping>, LocateError> {
    Ok(walk(content, container, "")?.mapping)
}

/// Byte range of the `name:` entry inside the container mapping.
fn locate_entry(
    content: &str,
    container: &str,
    name: &str,
) -> Result<Option<(usize, usize)>, LocateError> {
    Ok(walk(content, container, name)?.entry)
}

/// Prefix every non-blank line of `body` with `indent` spaces.
fn indent_lines(body: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    body.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{pad}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Name a [`LocateError`] as the refusal the user reads. Every variant is a
/// *refusal*, never a rewrite: a file we cannot read confidently is one we do
/// not touch, which is the same posture [`crate::json_entry`] and
/// [`crate::toml_entry`] take for input they cannot parse.
fn refuse(e: LocateError, container: &str, path: &Path) -> InstallError {
    match e {
        LocateError::Inline => InstallError::YamlInlineMapping {
            path: path.to_path_buf(),
            key: container.to_string(),
        },
        LocateError::MultiDocument => InstallError::YamlMultiDocument {
            path: path.to_path_buf(),
        },
        LocateError::Parse(why) => InstallError::BadYaml {
            path: path.to_path_buf(),
            why,
        },
    }
}

fn mapping_of(
    content: &str,
    container: &str,
    path: &Path,
) -> Result<Option<Mapping>, InstallError> {
    locate_mapping(content, container).map_err(|e| refuse(e, container, path))
}

fn entry_of(
    content: &str,
    container: &str,
    key: &str,
    path: &Path,
) -> Result<Option<(usize, usize)>, InstallError> {
    locate_entry(content, container, key).map_err(|e| refuse(e, container, path))
}

/// Is our entry present in `content` — inside our markers, or bare after a
/// vendor rewrite stripped them?
///
/// Both [`contains`] and [`upsert`] ask this, and the whole point of the
/// function is that they cannot answer it differently. `upsert` used to ask
/// only `block::contains`, so a re-install over the marker-less orphan a Hermes
/// write leaves behind *replaced* a live entry and called it
/// `Action::Installed` — "added" — which is the one sentence this module's
/// fallback exists to stop us saying. The predicate is content-level rather
/// than path-level because `upsert` already holds the bytes and must not read
/// the file a second time to decide what to call what it just did.
fn present_in(
    content: &str,
    container: &str,
    key: &str,
    markers: &Markers,
    path: &Path,
) -> Result<bool, InstallError> {
    if block::contains(content, markers) {
        return Ok(true);
    }
    Ok(entry_of(content, container, key, path)?.is_some())
}

/// Take `content` back to what it looked like before we ever touched it: drop
/// our managed block, and drop an unmarked entry a vendor rewrite left behind.
fn without_ours(
    content: &str,
    container: &str,
    key: &str,
    markers: &Markers,
    path: &Path,
) -> Result<String, InstallError> {
    let mut base = block::remove(content, markers).unwrap_or_else(|| content.to_string());
    if let Some((s, e)) = entry_of(&base, container, key, path)? {
        base = format!("{}{}", &base[..s], &base[e..]);
    }
    Ok(base)
}

/// Insert or refresh the `key` entry under `container` in the YAML file at
/// `path`. `entry_body` is the entry written at column 0; this module indents it
/// to wherever it lands.
pub fn upsert(
    path: &Path,
    container: &str,
    key: &str,
    entry_body: &str,
    markers: &Markers,
) -> Result<(Action, String), InstallError> {
    let before = read_opt(path)?;
    let content = before.clone().unwrap_or_default();

    // Normalise first, then install once. Doing it in that order means a
    // refresh, a re-install over a vendor-stripped orphan and a first install
    // are all the same code path — and `block::remove` inverts `block::upsert`
    // exactly, so an unchanged install still produces identical bytes.
    let base = without_ours(&content, container, key, markers, path)?;
    let mapping = mapping_of(&base, container, path)?;

    let updated = match mapping {
        Some(m) => {
            let body = indent_lines(entry_body, m.indent);
            let (head, tail) = base.split_at(m.body_start);
            format!("{}{}", block::upsert(head, &body, markers), tail)
        }
        None => {
            let body = format!("{container}:\n{}", indent_lines(entry_body, DEFAULT_INDENT));
            block::upsert(&base, &body, markers)
        }
    };

    if before.as_deref() == Some(updated.as_str()) {
        return Ok((
            Action::Unchanged,
            format!("{container}.{key} already current"),
        ));
    }
    // "Was ours already here?" — the same question [`contains`] answers, and it
    // has to be, because a marker-less orphan is still our registration and
    // replacing one is an update, not an install.
    //
    // The error arm is unreachable rather than merely unlikely: with markers
    // present `block::contains` short-circuits, and without them `without_ours`
    // above already walked these exact bytes and returned early on any refusal.
    // It is still not a `?`, because a question we could not answer must never
    // fail a write that is otherwise fine — and of the two labels, "refreshed"
    // only claims we rewrote something, where "added" asserts nothing of ours
    // was there. That assertion is precisely what this line got wrong before.
    let had = present_in(&content, container, key, markers, path).unwrap_or(true);
    write_all(path, &updated)?;
    Ok(if had {
        (Action::Updated, format!("{container}.{key} refreshed"))
    } else if before.is_some() {
        (Action::Installed, format!("{container}.{key} added"))
    } else {
        (Action::Installed, "file created".into())
    })
}

/// Remove the `key` entry under `container`. Deletes the file when nothing but
/// our entry was in it.
pub fn remove(
    path: &Path,
    container: &str,
    key: &str,
    markers: &Markers,
) -> Result<(Action, String), InstallError> {
    let Some(content) = read_opt(path)? else {
        return Ok((Action::Absent, "no such file".into()));
    };

    let (cleaned, detail) = match block::remove(&content, markers) {
        Some(cleaned) => (
            cleaned,
            format!("{container}.{key} removed, rest preserved"),
        ),
        None => {
            // The markers are gone. Hermes strips every comment in the file on
            // any write of its own, so this is the expected state after the user
            // has run `hermes mcp add`/`remove` — not a corruption. Our entry is
            // still live, so it still has to go.
            if mapping_of(&content, container, path)?.is_none() {
                return Ok((Action::Absent, format!("no `{container}:` in this file")));
            }
            let Some((s, e)) = entry_of(&content, container, key, path)? else {
                return Ok((Action::Absent, format!("no `{container}.{key}` entry")));
            };
            (
                format!("{}{}", &content[..s], &content[e..]),
                format!(
                    "{container}.{key} removed by name — our marker comments were gone (any \
                     Hermes write strips every comment in the file)"
                ),
            )
        }
    };

    if cleaned.trim().is_empty() {
        return Ok((Action::Removed, remove_file(path)?.detail("entry")));
    }
    write_all(path, &cleaned)?;
    Ok((Action::Removed, detail))
}

/// Is our entry present — either inside our markers, or bare after a vendor
/// rewrite stripped them?
///
/// **Kept as an oracle, not as a caller's API.** The question production asks is
/// [`state`]'s — "is the entry there *and* does it say what we would write" —
/// and it has no use for a bare yes/no; `json_entry` and `toml_entry` each had
/// the same function and both were deleted when `state` took their callers. This
/// one stays because
/// [`tests::upsert_labels_an_install_with_the_same_answer_contains_would_give`]
/// is the test that fixed a re-install over a vendor-stripped orphan being
/// labelled `Installed`, and it fixed it by asserting that `upsert`'s notion of
/// "was it there" agrees with *this* function's across every marker state. Two
/// answers to one question disagreeing was the defect; deleting one of them
/// deletes the test's premise rather than the redundancy.
///
/// `pub` and not `pub(crate)` for a mechanical reason: its only callers are
/// under `#[cfg(test)]`, so a crate-visible version is dead code in a normal
/// build and the crate denies warnings.
pub fn contains(
    path: &Path,
    container: &str,
    key: &str,
    markers: &Markers,
) -> Result<bool, InstallError> {
    let Some(content) = read_opt(path)? else {
        return Ok(false);
    };
    present_in(&content, container, key, markers, path)
}

/// What install would write, as a [`Node`].
///
/// An adapter's `entry_body` is a *fragment* — one key at column 0 — so it is
/// read back in exactly the shape [`upsert`] gives it on disk: wrapped in its
/// container and indented. One parser, one set of rules, and the expected value
/// is produced by the same code that reads the file it will be compared against.
/// A second, nearly-identical "parse a fragment" path would be a second place
/// for the two sides to disagree.
fn expected_value(
    container: &str,
    key: &str,
    entry_body: &str,
) -> Result<Option<Node>, LocateError> {
    let document = format!(
        "{container}:\n{}\n",
        indent_lines(entry_body, DEFAULT_INDENT)
    );
    Ok(walk(&document, container, key)?.value)
}

/// Is the `key` entry under `container` the one install would write?
///
/// The comparison is on the entry's **value** — scalar contents, sequence order,
/// mapping contents regardless of key order — and never on its text. See
/// [`EntryState`] for why the file is the wrong unit; the entry's *spelling* is
/// the wrong unit for a reason particular to YAML and to this vendor:
///
/// - the body an adapter hands [`upsert`] is written at column 0 and lands
///   indented to whatever the container mapping uses, so the two are never equal
///   as text to begin with;
/// - and Hermes re-emits the whole document from a template on every write of
///   its own (ADR-0034 §15 measured a `hermes mcp remove` turning 20 lines into
///   48). A re-spelling of our entry is therefore not a hypothetical a user
///   might produce by hand — it is *what that vendor does*. A flow sequence for
///   `args`, sequence items at the canonical indent, single quotes, `args`
///   before `command`: every one of those is the same registration, and calling
///   any of them `stale` is a permanent alarm on a 0600 credentials file that
///   re-running install cannot clear, because install would write bytes the
///   vendor then re-spells again.
///
/// The block is not the unit either, for the same measurement: a Hermes write
/// strips every comment in the file and takes our markers with it, while leaving
/// the entry live. Locating by name rather than by marker means that case needs
/// no second code path.
pub fn state(
    path: &Path,
    container: &str,
    key: &str,
    entry_body: &str,
) -> Result<EntryState, InstallError> {
    let Some(content) = read_opt(path)? else {
        return Ok(EntryState::Absent);
    };
    // The same walk that `upsert` and `remove` use, so a file all three refuse —
    // an inline container, more than one document, YAML that does not parse —
    // is refused here too. A `status` that answered "not registered" where
    // `install` fails would be the three-verbs-one-file disagreement again.
    let found = walk(&content, container, key).map_err(|e| refuse(e, container, path))?;
    if found.entry.is_none() {
        return Ok(EntryState::Absent);
    }
    // A body *we* rendered that will not parse is our bug, not the user's, and
    // it must not be reported against their file: `stale` would send them to an
    // install that writes the same unreadable thing again.
    let want = expected_value(container, key, entry_body).map_err(|e| {
        InstallError::Template(format!(
            "the `{container}.{key}` entry filigrio renders is not YAML it can read back ({e:?}); \
             this is a defect in filigrio, not in {}",
            path.display()
        ))
    })?;
    Ok(match (found.value, want) {
        (Some(have), Some(want)) if have == want => EntryState::Current,
        _ => EntryState::Stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::YAML;

    const CONTAINER: &str = "mcp_servers";
    const KEY: &str = "filigrio";

    fn body() -> String {
        "filigrio:\n  command: \"/opt/g/bin/filigrio-mcp\"\n  args:\n    - \"--socket\"\n    - \"/run/filigrio.sock\"".into()
    }

    fn write(dir: &std::path::Path, text: &str) -> std::path::PathBuf {
        let p = dir.join("config.yaml");
        std::fs::write(&p, text).unwrap();
        p
    }

    /// A user's config with comments above and below, a sibling server and a
    /// flow sequence — every one of which Hermes' own writer destroys.
    const USERS: &str = concat!(
        "# my hermes config\n",
        "model: hermes-4-405b\n",
        "\n",
        "mcp_servers:\n",
        "  sentry:\n",
        "    command: npx\n",
        "    args: [\"-y\", \"@sentry/mcp\"]\n",
        "    enabled: true\n",
        "\n",
        "# a trailing note\n",
        "temperature: 0.7\n"
    );

    #[test]
    fn a_users_config_round_trips_byte_exactly() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), USERS);

        let (a, _) = upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        assert_eq!(a, Action::Installed);
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("# my hermes config"), "their comment stays");
        assert!(
            after.contains("args: [\"-y\", \"@sentry/mcp\"]"),
            "flow kept"
        );
        assert!(!after.contains("_config_version"), "we inject nothing");

        let (a, _) = remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(a, Action::Removed);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), USERS);
    }

    /// The lines of the `filigrio:` entry we wrote, as (indent, text) pairs:
    /// its own line, then every following line indented deeper than it.
    ///
    /// Assertions go through this rather than `after.contains("…")` because the
    /// fixtures deliberately contain a *sibling* entry with the same child keys
    /// (`command:`), so a bare substring search is answered by **their** lines
    /// and passes whatever we wrote. This function cannot see their entry at
    /// all — see [`tests::the_indent_assertions_cannot_be_satisfied_by_the_neighbouring_entry`].
    fn our_entry_lines(after: &str) -> Vec<(usize, String)> {
        let indent_of = |l: &str| l.len() - l.trim_start().len();
        let mut out: Vec<(usize, String)> = Vec::new();
        for line in after.lines() {
            match out.first() {
                None => {
                    if line.trim_start().starts_with("filigrio:") {
                        out.push((indent_of(line), line.to_string()));
                    }
                }
                Some((key_indent, _)) => {
                    if line.trim().is_empty() || indent_of(line) > *key_indent {
                        out.push((indent_of(line), line.to_string()));
                    } else {
                        break;
                    }
                }
            }
        }
        out
    }

    /// The entry must land at the indent the mapping already uses, or it becomes
    /// a *child* of the entry above it — valid YAML, wrong structure, no error.
    ///
    /// Its own children are then written at the body's step (two) *relative to*
    /// that indent, so a 4-space mapping gets a `4 → 6` entry rather than the
    /// file's own `4 → 8`. That is deliberate and it is what the assertions
    /// below pin: the load-bearing property is "a sibling of theirs, with
    /// children deeper than itself", which is what decides whether Hermes reads
    /// two servers or one nested inside the other. Matching the file's *step* as
    /// well would be cosmetic.
    #[test]
    fn the_entry_matches_the_indent_the_mapping_already_uses() {
        let d = tempfile::tempdir().unwrap();
        let four = "mcp_servers:\n    sentry:\n        command: npx\n";
        let p = write(d.path(), four);
        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();

        let after = std::fs::read_to_string(&p).unwrap();
        let ours = our_entry_lines(&after);
        assert_eq!(
            ours.first().map(|(i, _)| *i),
            Some(4),
            "a sibling of `sentry`, not a child of it:\n{after}"
        );
        assert!(ours.len() > 1, "the entry has no body at all:\n{after}");
        for (indent, line) in ours.iter().skip(1).filter(|(_, l)| !l.trim().is_empty()) {
            assert!(
                *indent > 4,
                "`{line}` is at {indent}, which puts it outside our entry:\n{after}"
            );
        }
        assert_eq!(
            ours[1].0, 6,
            "children sit at the body's own step above our key:\n{after}"
        );

        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), four);
    }

    /// The guard on the guard.
    ///
    /// `the_entry_matches_the_indent_the_mapping_already_uses` used to assert
    /// `after.contains("\n        command:")` and call it "4-space children".
    /// Our entry never had 8-space children — the fixture's own `sentry:` did,
    /// so the assertion was answered by the neighbour and would have passed for
    /// *any* indent we wrote, including one that nested our server inside
    /// theirs. A test that passes for the wrong reason is worse than no test,
    /// because it is counted as coverage.
    ///
    /// This pins the property that made the old assertion vacuous — the fixture
    /// really does contain a same-named child key at the depth we were claiming
    /// — and that the helper reading our entry cannot see it.
    #[test]
    fn the_indent_assertions_cannot_be_satisfied_by_the_neighbouring_entry() {
        let d = tempfile::tempdir().unwrap();
        let four = "mcp_servers:\n    sentry:\n        command: npx\n";
        assert!(
            four.contains("\n        command:"),
            "the fixture must still be able to answer the old, vacuous assertion"
        );

        let p = write(d.path(), four);
        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();

        let ours = our_entry_lines(&after);
        assert!(
            ours.iter().all(|(_, l)| !l.contains("npx")),
            "the helper leaked the neighbour's lines into ours: {ours:?}"
        );
        assert!(
            after.contains("\n        command: npx"),
            "and the neighbour is still there, untouched:\n{after}"
        );
    }

    #[test]
    fn an_absent_mapping_is_created_and_removed_with_the_block() {
        let d = tempfile::tempdir().unwrap();
        let prior = "model: hermes-4-405b\n";
        let p = write(d.path(), prior);

        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(
            after.contains("mcp_servers:\n"),
            "we bring the key:\n{after}"
        );
        assert!(after.contains("\n  filigrio:\n"));

        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    #[test]
    fn an_empty_mapping_gets_the_default_indent() {
        let d = tempfile::tempdir().unwrap();
        let prior = "mcp_servers:\ntemperature: 0.7\n";
        let p = write(d.path(), prior);

        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("\n  filigrio:\n"), "two spaces:\n{after}");
        assert!(
            after.find("filigrio").unwrap() < after.find("temperature").unwrap(),
            "the entry must land inside the mapping, not after the next key:\n{after}"
        );

        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    #[test]
    fn a_file_we_created_is_deleted_on_uninstall() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        assert!(p.exists());
        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn a_second_identical_install_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), USERS);
        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        let once = std::fs::read_to_string(&p).unwrap();

        let (a, _) = upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        assert_eq!(a, Action::Unchanged);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), once);
    }

    #[test]
    fn a_changed_entry_is_an_update_in_place() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), USERS);
        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();

        let moved = body().replace("/opt/g/bin", "/elsewhere");
        let (a, _) = upsert(&p, CONTAINER, KEY, &moved, &YAML).unwrap();
        assert_eq!(a, Action::Updated);
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("/elsewhere"));
        // Counting `filigrio:` would also count the markers, which spell
        // `filigrio:start` / `filigrio:end` — count the entry *line*.
        assert_eq!(
            after.matches("\n  filigrio:\n").count(),
            1,
            "one entry:\n{after}"
        );

        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), USERS);
    }

    /// **The measured failure mode.** Hermes does not edit the file, it
    /// **re-emits the whole document from a template** — ADR-0034 §15 measured a
    /// plain `hermes mcp remove` turning a 20-line hand-written config into 48
    /// lines. So the shape here is what a template emitter produces, not what an
    /// editor leaves: the user's comments gone, our markers gone,
    /// `_config_version` injected, boilerplate appended — *and our entry
    /// re-spelled*, at the emitter's indent, with `args` as a flow sequence and
    /// single-quoted scalars.
    ///
    /// That last clause is the point. This fixture used to strip the comments
    /// while preserving our exact indentation, spelling and key order, which is
    /// the one thing a re-emitter cannot do: it was calibrated to what an
    /// *editor* would leave, and so could be passed by a `state` that compared
    /// text. The entry below is the same registration by value and shares almost
    /// no bytes with the one we wrote.
    fn vendor_rewritten() -> String {
        concat!(
            "model: hermes-4-405b\n",
            "_config_version: 33\n",
            "mcp_servers:\n",
            "    filigrio:\n",
            "        args: ['--socket', '/run/filigrio.sock']\n",
            "        command: '/opt/g/bin/filigrio-mcp'\n",
            "    sentry:\n",
            "        command: npx\n",
            "temperature: 0.7\n",
            "\n",
            "# ── Security ─────────────────────\n",
            "# security:\n",
            "#   redact_secrets: true\n"
        )
        .into()
    }

    /// How many `filigrio:` **entry** lines a file holds, at any indent.
    ///
    /// Not `matches("\n  filigrio:\n")`: the fixtures now come at more than one
    /// indent, and a literal that only matches two spaces would silently count
    /// zero — an assertion satisfied by the wrong thing rather than a failing
    /// one. The markers spell `filigrio:start` / `filigrio:end`, so an exact
    /// trimmed match excludes them.
    fn our_key_lines(after: &str) -> usize {
        after.lines().filter(|l| l.trim() == "filigrio:").count()
    }

    #[test]
    fn an_entry_whose_markers_a_vendor_write_stripped_is_still_removed() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), &vendor_rewritten());

        assert!(
            contains(&p, CONTAINER, KEY, &YAML).unwrap(),
            "status must see a marker-less entry, or it reports a lie"
        );

        let (a, detail) = remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(a, Action::Removed);
        assert!(detail.contains("by name"), "the reason is told: {detail}");

        let after = std::fs::read_to_string(&p).unwrap();
        assert!(!after.contains("filigrio"), "no orphan left:\n{after}");
        assert!(after.contains("  sentry:"), "their server survives");
        assert!(after.contains("# ── Security"), "the tail survives");
        assert!(
            after.contains("_config_version: 33"),
            "not our key to remove"
        );
        assert!(
            after.contains("temperature: 0.7\n"),
            "the following key must not be swallowed:\n{after}"
        );
    }

    /// Re-installing after a vendor rewrite must replace the orphan, not add a
    /// second `filigrio:` to the same mapping — a duplicate key.
    ///
    /// And it must *say* it replaced one. This test used to check only the
    /// bytes, which is why `upsert` could report `Installed` — "added" — over a
    /// live entry for as long as it did: the marker-only test it was asking
    /// answers "no" in exactly the state this module was written for.
    #[test]
    fn reinstalling_over_a_stripped_entry_does_not_duplicate_the_key() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), &vendor_rewritten());

        let (a, detail) = upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        assert_eq!(
            a,
            Action::Updated,
            "the entry was already live — only our markers were gone: {detail}"
        );
        assert!(detail.contains("refreshed"), "got: {detail}");
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            our_key_lines(&after),
            1,
            "exactly one entry, and it is inside our markers:\n{after}"
        );
        assert!(block::contains(&after, &YAML), "the markers are back");
        assert!(after.contains("  sentry:"));
    }

    /// An entry that is the *last* thing in the mapping, with a top-level key
    /// following: the region must stop at the column-0 line.
    #[test]
    fn removal_stops_at_the_next_top_level_key() {
        let d = tempfile::tempdir().unwrap();
        let prior = concat!(
            "mcp_servers:\n",
            "  filigrio:\n",
            "    command: x\n",
            "\n",
            "temperature: 0.7\n"
        );
        let p = write(d.path(), prior);
        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "mcp_servers:\n\ntemperature: 0.7\n",
            "the blank line and the next key are the user's"
        );
    }

    /// `mcp_servers: {}` is legal YAML we will not rewrite. Refused by name, and
    /// the file is untouched — a refusal is better than a reformat.
    #[test]
    fn an_inline_mapping_is_refused_rather_than_rewritten() {
        let d = tempfile::tempdir().unwrap();
        let prior = "mcp_servers: {}\nmodel: hermes-4-405b\n";
        let p = write(d.path(), prior);

        let err = upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap_err();
        assert!(
            matches!(err, InstallError::YamlInlineMapping { .. }),
            "got {err}"
        );
        assert!(err.to_string().contains("mcp_servers"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// A `mcp_servers:` under `profiles.work` is a **different key**, and this
    /// file's registration goes in a top-level one it has to add.
    ///
    /// The property outlived the reason it was first written down. The old
    /// locator matched the container key at column zero by string prefix, and
    /// this pinned that a nested one did not match; the module now locates with
    /// [`saphyr_parser`], which reports a key's *nesting* rather than its
    /// indentation, so the whole class — a nested key, a list item, a line
    /// inside a block scalar — stops being cases to enumerate. What is worth
    /// keeping is the answer, not the argument: "which mapping does this key
    /// belong to" is the question both implementations had to get right, and a
    /// future one that reads the file as text again fails here.
    #[test]
    fn a_nested_key_of_the_same_name_is_not_the_container() {
        let d = tempfile::tempdir().unwrap();
        let prior = "profiles:\n  work:\n    mcp_servers:\n      theirs:\n        command: x\n";
        let p = write(d.path(), prior);

        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(
            after.contains("\nmcp_servers:\n"),
            "a new top-level mapping, not a splice into the nested one:\n{after}"
        );
        assert!(after.contains("      theirs:"), "theirs is untouched");

        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// **The bug the parser was adopted to kill.** `"mcp_servers":` and
    /// `mcp_servers:` are the same key; the string matcher this module used
    /// before did not think so, and appended a *second* top-level
    /// `mcp_servers:` — a duplicate key, which YAML 1.2 makes an error and
    /// which Hermes would either refuse or silently half-load. A parser reports
    /// a scalar's value, not its spelling, so the whole class goes with it.
    #[test]
    fn a_quoted_container_key_is_the_same_key_and_is_not_duplicated() {
        for prior in [
            "\"mcp_servers\":\n  sentry:\n    command: npx\n",
            "'mcp_servers':\n  sentry:\n    command: npx\n",
        ] {
            let d = tempfile::tempdir().unwrap();
            let p = write(d.path(), prior);

            upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
            let after = std::fs::read_to_string(&p).unwrap();
            assert!(
                !after.contains("\nmcp_servers:"),
                "a second, unquoted container key was invented:\n{after}"
            );
            assert!(after.contains("  filigrio:"), "and ours went inside it");
            assert!(after.contains("  sentry:"), "theirs survives");

            remove(&p, CONTAINER, KEY, &YAML).unwrap();
            assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
        }
    }

    /// An entry named like ours, but under somebody else's top-level mapping,
    /// is not ours. The old locator matched on indentation alone and had no way
    /// to know whose mapping it was inside.
    #[test]
    fn an_entry_of_the_same_name_under_another_mapping_is_not_ours() {
        let d = tempfile::tempdir().unwrap();
        let prior = "other:\n  filigrio: not ours\nmcp_servers:\n  sentry:\n    command: npx\n";
        let p = write(d.path(), prior);

        assert!(
            !contains(&p, CONTAINER, KEY, &YAML).unwrap(),
            "someone else's `filigrio:` is not our registration"
        );

        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(
            after.contains("  filigrio: not ours"),
            "their entry must be untouched:\n{after}"
        );

        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }

    /// Loaders differ on which document of a multi-document file they take, so
    /// choosing one is guessing — and guessing wrong writes a registration into
    /// a document nothing reads. The old locator matched the first column-0
    /// `mcp_servers:` in the file regardless of which document it was in.
    #[test]
    fn a_multi_document_file_is_refused_rather_than_guessed_at() {
        let d = tempfile::tempdir().unwrap();
        let prior = "model: a\n---\nmcp_servers:\n  sentry:\n    command: npx\n";
        let p = write(d.path(), prior);

        let err = upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap_err();
        assert!(
            matches!(err, InstallError::YamlMultiDocument { .. }),
            "got {err}"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior, "untouched");
    }

    /// YAML we cannot parse is refused by name, the posture `json_entry` and
    /// `toml_entry` already take. The refusal is *recorded* rather than returned
    /// as soon as it is known, so a file that is both malformed and oddly shaped
    /// reports the parse error — that is the sentence the user can act on first.
    #[test]
    fn malformed_yaml_is_refused_and_named_rather_than_spliced() {
        let d = tempfile::tempdir().unwrap();
        let prior = "mcp_servers:\n  filigrio:\n   command: x\n  \tbad: [\n";
        let p = write(d.path(), prior);

        let err = upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap_err();
        assert!(matches!(err, InstallError::BadYaml { .. }), "got {err}");
        assert!(err.to_string().contains("config.yaml"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior, "untouched");
    }

    #[test]
    fn removing_what_was_never_installed_is_absent_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        assert_eq!(remove(&p, CONTAINER, KEY, &YAML).unwrap().0, Action::Absent);

        let p = write(d.path(), "model: x\n");
        assert_eq!(remove(&p, CONTAINER, KEY, &YAML).unwrap().0, Action::Absent);

        let p = write(d.path(), "mcp_servers:\n  sentry:\n    command: npx\n");
        assert_eq!(remove(&p, CONTAINER, KEY, &YAML).unwrap().0, Action::Absent);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "mcp_servers:\n  sentry:\n    command: npx\n"
        );
    }

    #[test]
    fn contains_reports_our_entry_in_both_states() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), USERS);
        assert!(!contains(&p, CONTAINER, KEY, &YAML).unwrap());
        upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
        assert!(contains(&p, CONTAINER, KEY, &YAML).unwrap());
        remove(&p, CONTAINER, KEY, &YAML).unwrap();
        assert!(!contains(&p, CONTAINER, KEY, &YAML).unwrap());
    }

    /// The two questions are one question.
    ///
    /// `contains` has always counted a marker-less entry as ours; `upsert`
    /// asked only whether the markers were there, and so labelled a re-install
    /// over a vendor-stripped orphan `Installed`. Two functions in one file
    /// disagreeing about "present" is the defect — the wrong label was only the
    /// symptom — so this asserts the agreement itself, across every marker
    /// state, rather than one label in one fixture.
    #[test]
    fn upsert_labels_an_install_with_the_same_answer_contains_would_give() {
        let d = tempfile::tempdir().unwrap();
        let installed = {
            let p = write(d.path(), USERS);
            upsert(&p, CONTAINER, KEY, &body(), &YAML).unwrap();
            std::fs::read_to_string(&p).unwrap()
        };
        // A body that differs from what each fixture already holds, so the
        // interesting answer is `Installed` vs `Updated` and never `Unchanged`.
        let moved = body().replace("/opt/g/bin", "/elsewhere");

        for (state, prior) in [
            ("nothing of ours", USERS.to_string()),
            ("our markers intact", installed),
            ("our markers stripped by a vendor write", vendor_rewritten()),
        ] {
            let d = tempfile::tempdir().unwrap();
            let p = write(d.path(), &prior);

            let was_there = contains(&p, CONTAINER, KEY, &YAML).unwrap();
            let (a, detail) = upsert(&p, CONTAINER, KEY, &moved, &YAML).unwrap();
            assert_eq!(
                a,
                if was_there {
                    Action::Updated
                } else {
                    Action::Installed
                },
                "with {state}, `contains` said {was_there} and `upsert` said {a:?} ({detail})"
            );
        }
    }

    /// The entry as [`crate::clients::hermes`] builds it, at column 0, with the
    /// socket in it — the field a user actually changes.
    fn body_at(socket: &str) -> String {
        format!(
            "filigrio:\n  command: \"/opt/g/bin/filigrio-mcp\"\n  args:\n    - \"--socket\"\n    - \"{socket}\""
        )
    }

    /// The three states. `contains` answers `true` for a registration pointing
    /// at a socket that moved, which is the gap this closes.
    #[test]
    fn a_registration_that_no_longer_matches_reports_stale() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        let want = body_at("/run/filigrio.sock");

        assert_eq!(
            state(&p, CONTAINER, KEY, &want).unwrap(),
            EntryState::Absent,
            "no file at all"
        );
        let p = write(d.path(), USERS);
        assert_eq!(
            state(&p, CONTAINER, KEY, &want).unwrap(),
            EntryState::Absent,
            "their mapping, none of our entries in it"
        );

        upsert(&p, CONTAINER, KEY, &want, &YAML).unwrap();
        assert_eq!(
            state(&p, CONTAINER, KEY, &want).unwrap(),
            EntryState::Current
        );
        assert_eq!(
            state(&p, CONTAINER, KEY, &want).unwrap(),
            EntryState::Current,
            "asking twice must give the same answer"
        );

        upsert(
            &p,
            CONTAINER,
            KEY,
            &body_at("/run/user/1000/filigrio.sock"),
            &YAML,
        )
        .unwrap();
        assert_eq!(state(&p, CONTAINER, KEY, &want).unwrap(), EntryState::Stale);
    }

    /// **The false alarm this must never raise.** The body an adapter hands us
    /// is written at column 0; what lands on disk is indented to the container
    /// mapping — two spaces here, four in the fixture below. Comparing either
    /// the file or the block as *text* answers `stale` for every install this
    /// module has ever done, which is why [`dedent`] exists.
    #[test]
    fn an_entry_indented_to_its_mapping_is_current_at_any_indent() {
        let want = body_at("/run/filigrio.sock");
        for prior in [
            "mcp_servers:\n  sentry:\n    command: npx\n",
            "mcp_servers:\n    sentry:\n        command: npx\n",
            "mcp_servers:\n",
            "model: hermes-4-405b\n",
        ] {
            let d = tempfile::tempdir().unwrap();
            let p = write(d.path(), prior);
            upsert(&p, CONTAINER, KEY, &want, &YAML).unwrap();

            let after = std::fs::read_to_string(&p).unwrap();
            assert_eq!(
                state(&p, CONTAINER, KEY, &want).unwrap(),
                EntryState::Current,
                "we wrote this ourselves and then called it stale:\n{after}"
            );
            assert!(
                !after.contains(&want),
                "the fixture stopped exercising the indent: our body appears \
                 verbatim at column 0, so a text comparison would have passed:\n{after}"
            );
        }
    }

    /// The comparison [`state`] used to make, kept here as the thing every
    /// fixture below has to defeat: the entry's lines, dedented, compared as
    /// text.
    ///
    /// A guard whose fixtures the old implementation would also have passed is
    /// not a guard — it is a second copy of a test that already exists. So each
    /// case asserts *both* that it is `Current` by value and that it was `Stale`
    /// by shape, and the day someone writes a fixture that is merely a
    /// differently-worded socket path, this says so.
    fn same_by_text_shape(entry: &str, body: &str) -> bool {
        let dedent = |text: &str| {
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let base = lines.first().map_or(0, |l| l.len() - l.trim_start().len());
            lines
                .iter()
                .map(|l| {
                    let indent = l.len() - l.trim_start().len();
                    format!("{}{}", " ".repeat(indent.saturating_sub(base)), l.trim())
                })
                .collect::<Vec<_>>()
        };
        dedent(entry) == dedent(body)
    }

    /// **The false alarm this must never raise**, and the one adapter where it
    /// is not hypothetical.
    ///
    /// Hermes re-emits the whole document from a template on every write of its
    /// own — ADR-0034 §15 measured `hermes mcp remove` turning 20 lines into 48.
    /// So our entry coming back respelled is not something an unusual user does,
    /// it is what this vendor does to everybody, and each spelling below is one
    /// a YAML emitter really produces. Every one of them is the same
    /// registration: same command, same arguments, same order of arguments.
    ///
    /// Reporting `stale` for any of them is a permanent alarm on a 0600
    /// credentials file that the action it names cannot clear — install writes
    /// our spelling, the next `hermes mcp` write replaces it with theirs, and
    /// `status` says `stale` again. `json_entry` and `toml_entry` each hold this
    /// same property for their own format's respellings.
    #[test]
    fn an_entry_the_user_respelled_is_current_not_stale() {
        let want = body_at("/run/filigrio.sock");
        let cases = [
            (
                "args as a flow sequence",
                concat!(
                    "mcp_servers:\n",
                    "  filigrio:\n",
                    "    command: \"/opt/g/bin/filigrio-mcp\"\n",
                    "    args: [\"--socket\", \"/run/filigrio.sock\"]\n"
                ),
            ),
            (
                "sequence items indented level with their key",
                concat!(
                    "mcp_servers:\n",
                    "  filigrio:\n",
                    "    command: \"/opt/g/bin/filigrio-mcp\"\n",
                    "    args:\n",
                    "    - \"--socket\"\n",
                    "    - \"/run/filigrio.sock\"\n"
                ),
            ),
            (
                "single-quoted scalars",
                concat!(
                    "mcp_servers:\n",
                    "  filigrio:\n",
                    "    command: '/opt/g/bin/filigrio-mcp'\n",
                    "    args:\n",
                    "      - '--socket'\n",
                    "      - '/run/filigrio.sock'\n"
                ),
            ),
            (
                "our own keys in the other order",
                concat!(
                    "mcp_servers:\n",
                    "  filigrio:\n",
                    "    args:\n",
                    "      - \"--socket\"\n",
                    "      - \"/run/filigrio.sock\"\n",
                    "    command: \"/opt/g/bin/filigrio-mcp\"\n"
                ),
            ),
        ];

        for (name, prior) in cases {
            let d = tempfile::tempdir().unwrap();
            let p = write(d.path(), prior);

            assert_eq!(
                state(&p, CONTAINER, KEY, &want).unwrap(),
                EntryState::Current,
                "{name}: the entry says exactly what we would write"
            );

            // The guard on the guard: a fixture the *old* text comparison would
            // also have called current proves nothing about the trap this holds
            // shut.
            let (s, e) = entry_of(prior, CONTAINER, KEY, &p).unwrap().unwrap();
            assert!(
                !same_by_text_shape(&prior[s..e], &want),
                "{name}: this fixture is the same text as our body, so it no \
                 longer exercises the respelling:\n{}",
                &prior[s..e]
            );

            // And the control, in every spelling: it is value equality, not
            // "anything with our two keys in it". A socket that really moved is
            // stale however the entry around it is written.
            assert_eq!(
                state(&p, CONTAINER, KEY, &body_at("/run/user/1000/filigrio.sock")).unwrap(),
                EntryState::Stale,
                "{name}: a registration pointing somewhere else is not current"
            );
        }
    }

    /// A Hermes write of its own strips every comment in the file, our markers
    /// with them (see the module docs). The entry it leaves is still the live
    /// registration, so it is still **current** — reporting `stale` because our
    /// bookkeeping comments are gone would send the user to re-run an install
    /// that changes nothing they can see.
    #[test]
    fn an_entry_whose_markers_a_vendor_write_stripped_is_still_current() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), &vendor_rewritten());
        assert!(!block::contains(&vendor_rewritten(), &YAML), "no markers");

        assert_eq!(
            state(&p, CONTAINER, KEY, &body()).unwrap(),
            EntryState::Current
        );

        // And the same file with the socket moved is stale, marker-less or not.
        let moved = std::fs::read_to_string(&p)
            .unwrap()
            .replace("/run/filigrio.sock", "/tmp/other.sock");
        std::fs::write(&p, moved).unwrap();
        assert_eq!(
            state(&p, CONTAINER, KEY, &body()).unwrap(),
            EntryState::Stale
        );
    }

    /// A key that merely *starts with* our name is not our entry.
    #[test]
    fn a_similarly_named_entry_is_not_matched() {
        let d = tempfile::tempdir().unwrap();
        let prior = "mcp_servers:\n  filigrio-legacy:\n    command: old\n";
        let p = write(d.path(), prior);
        assert!(!contains(&p, CONTAINER, KEY, &YAML).unwrap());
        assert_eq!(remove(&p, CONTAINER, KEY, &YAML).unwrap().0, Action::Absent);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), prior);
    }
}
