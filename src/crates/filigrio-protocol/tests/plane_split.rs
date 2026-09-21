//! ADR-0032f §2 — the type-level data/control-plane split, pinned at the wire.
//!
//! The contract is split into `DataQuery` (graph reads) and `ControlOp`
//! (mutations + daemon meta reads, behind the `control` feature). ADR-0042 F9
//! retired the third surface — the narrow `SliverOp` the MCP bridge used to
//! express register/index on — because the bridge is read-only now. These
//! tests pin:
//! - both planes round-trip through serde intact;
//! - the wire tags of the pre-split contract are preserved (`"Command"` /
//!   `"Query"` envelopes, per-op `kind` tags), so our own binaries can be
//!   updated one at a time;
//! - `Health`/`Progress` moved envelopes (Query → Command): they are control
//!   plane now, and the old `{"type":"Query","kind":"Health"}` frame is dead;
//! - the whole `"Sliver"` envelope is a **dead frame** — a stale bridge gets a
//!   parse error, never a silent reinterpretation as a command.

use filigrio_protocol::{DataQuery, Request};

#[cfg(feature = "control")]
use filigrio_protocol::{ChangeSet, Command, ControlOp, MetaQuery, Priority};

fn roundtrip(req: &Request) -> (serde_json::Value, Request) {
    let json = serde_json::to_value(req).expect("serialize");
    let back: Request = serde_json::from_value(json.clone()).expect("deserialize");
    (json, back)
}

/// Data plane keeps the `"Query"` envelope + `kind` tags of the old contract.
#[test]
fn data_plane_round_trips_with_preserved_wire_tags() {
    let req = Request::data(DataQuery::GodNodes {
        project: "p".into(),
        limit: 5,
    });
    let (json, back) = roundtrip(&req);
    assert_eq!(
        json["type"], "Query",
        "data plane must keep the Query envelope"
    );
    assert_eq!(json["kind"], "GodNodes");
    assert!(matches!(
        back,
        Request::Data(DataQuery::GodNodes { ref project, limit: 5 }) if project == "p"
    ));
}

/// Control-plane mutations keep the `"Command"` envelope + `kind` tags.
#[cfg(feature = "control")]
#[test]
fn control_command_round_trips_with_preserved_wire_tags() {
    let req = Request::command(Command::Submit {
        project: "p".into(),
        changeset: ChangeSet::default(),
        priority: Priority::Fs,
    });
    let (json, back) = roundtrip(&req);
    assert_eq!(
        json["type"], "Command",
        "control plane must keep the Command envelope"
    );
    assert_eq!(json["kind"], "Submit");
    assert!(matches!(
        back,
        Request::Control(ControlOp::Command(Command::Submit { ref project, .. })) if project == "p"
    ));
}

/// Health is a control-plane meta read now: it rides the `"Command"` envelope,
/// and the old `{"type":"Query","kind":"Health"}` frame no longer parses —
/// the plane move is real on the wire, not just in the Rust types.
#[cfg(feature = "control")]
#[test]
fn health_meta_read_lives_on_the_control_plane() {
    let (json, back) = roundtrip(&Request::meta(MetaQuery::Health));
    assert_eq!(json["type"], "Command");
    assert_eq!(json["kind"], "Health");
    assert!(matches!(
        back,
        Request::Control(ControlOp::Meta(MetaQuery::Health))
    ));

    let old_frame = serde_json::json!({"type": "Query", "kind": "Health"});
    assert!(
        serde_json::from_value::<Request>(old_frame).is_err(),
        "the pre-split Query::Health frame must be rejected, not silently read as data"
    );
}

/// Progress follows Health onto the control plane.
#[cfg(feature = "control")]
#[test]
fn progress_meta_read_lives_on_the_control_plane() {
    let (json, back) = roundtrip(&Request::meta(MetaQuery::Progress {
        project: "p".into(),
    }));
    assert_eq!(json["type"], "Command");
    assert_eq!(json["kind"], "Progress");
    assert!(matches!(
        back,
        Request::Control(ControlOp::Meta(MetaQuery::Progress { ref project })) if project == "p"
    ));
}

/// ADR-0042 F9 — the **whole `"Sliver"` envelope is a dead frame.**
///
/// Replaces the two tests that pinned the sliver's round-trip and its widening
/// onto `Command`: with the MCP bridge read-only, nothing produces a sliver
/// frame, so the property worth pinning inverted. What matters now is that a
/// *stale* bridge — an old binary still on someone's PATH, exactly the caller
/// this envelope was built for — gets a parse error instead of having its
/// register/index quietly honored as a command. Least privilege at the parse
/// layer, the F5/F6 dead-frame pattern.
///
/// The two live ops are first in the list on purpose: `ProjectRegister` and
/// `ProjectIndex` name verbs the daemon still executes, so they are the frames
/// most at risk of being reinterpreted rather than rejected. The rest —
/// pre-F7 verb-object spellings, verbs removed by F5/F6, and control-only
/// verbs the sliver never granted — must fail for the same reason: the
/// envelope is gone, so its contents are irrelevant.
#[test]
fn the_sliver_envelope_is_dead() {
    for kind in [
        "ProjectRegister",
        "ProjectIndex",
        "RegisterProject",
        "IndexProject",
        "DaemonStop",
        "ProjectRemove",
        "ProjectFlush",
        "Submit",
        "Validate",
        "BuildProject",
        "ProjectWatch",
    ] {
        let frame =
            serde_json::json!({"type": "Sliver", "kind": kind, "project": "p", "path": "/w"});
        assert!(
            serde_json::from_value::<Request>(frame).is_err(),
            "the retired Sliver envelope must reject kind={kind}, never widen it into a command"
        );
    }
}

/// ADR-0042 F5 — `project build` collapsed into `project index`:
/// - a `ProjectIndex` frame **without** `clean` still parses (`clean: false`),
///   so every pre-F5 client keeps working on the wire;
/// - the removed `ProjectBuild` command frame is rejected, not silently
///   reinterpreted.
///
/// (The flag is `clean` since ADR-0042 F7; the pre-F7 `force` spelling is a
/// dead field, pinned in `tests/vocabulary.rs`.)
#[cfg(feature = "control")]
#[test]
fn project_index_defaults_clean_off_and_project_build_frames_are_dead() {
    let pre_f5 = serde_json::json!({"type": "Command", "kind": "ProjectIndex", "project": "p"});
    let req: Request = serde_json::from_value(pre_f5).expect("clean must be serde-default");
    assert!(matches!(
        req,
        Request::Control(ControlOp::Command(Command::ProjectIndex {
            ref project,
            clean: false,
        })) if project == "p"
    ));

    let cleaned = serde_json::json!(
        {"type": "Command", "kind": "ProjectIndex", "project": "p", "clean": true}
    );
    let req: Request = serde_json::from_value(cleaned).expect("explicit clean must parse");
    assert!(matches!(
        req,
        Request::Control(ControlOp::Command(Command::ProjectIndex {
            clean: true,
            ..
        }))
    ));

    let old_build = serde_json::json!(
        {"type": "Command", "kind": "ProjectBuild", "project": "p", "clean": true}
    );
    assert!(
        serde_json::from_value::<Request>(old_build).is_err(),
        "the removed ProjectBuild command must not parse"
    );
}

/// ADR-0042 F6 — the removed `Validate` command frame is rejected at
/// deserialization, for BOTH `fix` polarities. `fix: false` (read-only dry run)
/// is behavior that no longer exists, and `fix: true` is NOT tolerated as a
/// legacy no-op either: the whole verb is a dead frame, never silently
/// reinterpreted.
///
/// (F6's other half — the `deep` wire default — is gone with F6b: `deep` never
/// shipped on the wire, so there is no default to pin. See
/// `project_index_has_no_deep_field_on_the_wire` below.)
#[cfg(feature = "control")]
#[test]
fn validate_frames_are_dead() {
    for fix in [false, true] {
        let old_validate = serde_json::json!(
            {"type": "Command", "kind": "Validate", "project": "p", "deep": true, "fix": fix}
        );
        assert!(
            serde_json::from_value::<Request>(old_validate).is_err(),
            "the removed Validate command must not parse (fix={fix})"
        );
    }
}

/// ADR-0042 F6b — reconcile depth is **not a wire concept**: `ProjectIndex`
/// serializes without a `deep` field, and a frame carrying one is simply
/// ignored (serde drops unknown fields) rather than selecting a shallow index.
/// The shallow fast-path exists only on the daemon-internal `Op::Reconcile`,
/// which no client can address.
#[cfg(feature = "control")]
#[test]
fn project_index_has_no_deep_field_on_the_wire() {
    let json = serde_json::to_value(Request::command(Command::ProjectIndex {
        project: "p".into(),
        clean: false,
    }))
    .expect("serialize");
    assert!(
        json.get("deep").is_none(),
        "reconcile depth must not ride the wire (F6b), got: {json}"
    );

    // A stale client's `deep: false` must NOT produce a shallow index — there
    // is no shallow wire index anymore; the field is inert.
    let stale = serde_json::json!(
        {"type": "Command", "kind": "ProjectIndex", "project": "p", "deep": false}
    );
    let req: Request = serde_json::from_value(stale).expect("a stale deep field must be inert");
    assert!(matches!(
        req,
        Request::Control(ControlOp::Command(Command::ProjectIndex {
            clean: false,
            ..
        }))
    ));
}

/// ADR-0042 F6b — `ProjectWatch` is a real control-plane verb on the `Command`
/// envelope, and both polarities round-trip.
#[cfg(feature = "control")]
#[test]
fn project_watch_round_trips_on_the_control_plane() {
    for on in [true, false] {
        let (json, back) = roundtrip(&Request::command(Command::ProjectWatch {
            project: "p".into(),
            on,
        }));
        assert_eq!(json["type"], "Command");
        assert_eq!(json["kind"], "ProjectWatch");
        assert_eq!(json["on"], on);
        assert!(matches!(
            back,
            Request::Control(ControlOp::Command(Command::ProjectWatch { ref project, on: got }))
                if project == "p" && got == on
        ));
    }
}

/// ADR-0042 F4 — `ProjectFlush` is a control-plane verb (persisting is an
/// operator/CI concern, not an agent one), and its outcome carries whether it
/// actually wrote. An MCP client reads the daemon's answers, never
/// `.filigrio-out` directly — and since F9 it cannot express *any* mutation,
/// flush included (see `the_sliver_envelope_is_dead`).
#[cfg(feature = "control")]
#[test]
fn project_flush_round_trips_on_the_control_plane() {
    use filigrio_protocol::{CommandOutcome, Response};

    let (json, back) = roundtrip(&Request::command(Command::ProjectFlush {
        project: "p".into(),
    }));
    assert_eq!(json["type"], "Command");
    assert_eq!(json["kind"], "ProjectFlush");
    assert!(matches!(
        back,
        Request::Control(ControlOp::Command(Command::ProjectFlush { ref project }))
            if project == "p"
    ));

    for wrote in [true, false] {
        let resp = Response::CommandCompleted {
            outcome: CommandOutcome::Flushed {
                project: "p".into(),
                wrote,
            },
        };
        let text = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&text).unwrap();
        assert!(matches!(
            back,
            Response::CommandCompleted {
                outcome: CommandOutcome::Flushed { wrote: got, .. }
            } if got == wrote
        ));
    }
}

/// ADR-0042 F6c — the response IS the outcome: `CommandCompleted` round-trips
/// with its typed payload, and the removed `CommandAccepted { job_id }` ack
/// frame no longer parses (a stale client's ack can't be silently accepted as
/// some other response).
#[test]
fn command_outcomes_round_trip_and_the_ack_frame_is_dead() {
    use filigrio_protocol::{CommandOutcome, Response};

    let outcomes = [
        CommandOutcome::Indexed {
            project: "p".into(),
            changed: 3,
            vanished: 0,
        },
        // ADR-0042 F8: the vanished count rides the same frame as `changed`.
        CommandOutcome::Indexed {
            project: "p".into(),
            changed: 3,
            vanished: 1,
        },
        CommandOutcome::Applied {
            project: "p".into(),
            changed: 0,
            vanished: 0,
        },
        CommandOutcome::Exported {
            project: "p".into(),
            path: "/tmp/graph.json".into(),
        },
        CommandOutcome::Watch {
            project: "p".into(),
            watching: true,
            changed: Some(2),
            vanished: Some(0),
            note: None,
        },
        CommandOutcome::Registered {
            project: "p".into(),
            path: "/w".into(),
        },
        CommandOutcome::Removed {
            project: "p".into(),
        },
        CommandOutcome::Stopping,
    ];
    for outcome in outcomes {
        let resp = Response::CommandCompleted { outcome };
        let json = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(json["type"], "CommandCompleted");
        let back: Response = serde_json::from_value(json.clone()).expect("deserialize");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            json,
            "outcome must round-trip unchanged"
        );
    }

    let old_ack = serde_json::json!({"type": "CommandAccepted", "job_id": "job-0"});
    assert!(
        serde_json::from_value::<Response>(old_ack).is_err(),
        "the removed CommandAccepted ack must not parse"
    );
}
