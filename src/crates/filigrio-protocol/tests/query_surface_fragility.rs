//! Query surface enhancement tests - ensure ADR-0025/27/29 fields work correctly.
//!
//! Tests that enhanced query parameters (mode/budget/relations/token_budget)
//! and typed NodeAddress/Direction are properly integrated into the protocol.

use filigrio_protocol::client::{DaemonClientTrait, InProcessClient};
use filigrio_protocol::contract::{
    Command, DataQuery, Direction, NodeAddress, Response, TraversalMode,
};
use filigrio_protocol::query_types::QueryParams;

/// Test that enhanced query parameters are properly accepted and serialized.
#[test]
fn test_enhanced_query_parameters_work() {
    // Test QueryParams with all ADR-0025/0029 fields
    let params = QueryParams {
        query: "find components".to_string(),
        mode: TraversalMode::Dfs,
        depth: 3,
        budget: 20,
        token_budget: 2000,
        relations: vec!["calls".to_string(), "imports".to_string()],
        include_unresolved: true,
    };

    // Serialize and deserialize to ensure round-trip works
    let serialized =
        serde_json::to_value(&params).expect("QueryParams should serialize successfully");

    let deserialized: QueryParams =
        serde_json::from_value(serialized).expect("QueryParams should deserialize successfully");

    assert_eq!(deserialized.query, "find components");
    assert_eq!(deserialized.mode, TraversalMode::Dfs);
    assert_eq!(deserialized.depth, 3);
    assert_eq!(deserialized.budget, 20);
    assert_eq!(deserialized.token_budget, 2000);
    assert_eq!(
        deserialized.relations,
        vec!["calls".to_string(), "imports".to_string()]
    );
    assert!(deserialized.include_unresolved);
}

/// Test that NodeAddress supports ADR-0027 addressing modes.
#[test]
fn test_node_address_modes_work() {
    // Test ID-based addressing (highest priority)
    let addr_id = NodeAddress::by_id("fn:src/component.rs:Component::new");
    assert!(addr_id.id.is_some());
    assert!(addr_id.label.is_none());
    assert!(addr_id.src.is_none());
    assert!(!addr_id.is_ambiguous());

    // Test label-based addressing (potentially ambiguous)
    let addr_label = NodeAddress::by_label("Component");
    assert!(addr_label.id.is_none());
    assert!(addr_label.label.is_some());
    assert!(addr_label.src.is_none());
    assert!(addr_label.is_ambiguous());

    // Test label+src addressing (disambiguated)
    let addr_label_src = NodeAddress::by_label_src("Component", "src/component.rs");
    assert!(addr_label_src.id.is_none());
    assert!(addr_label_src.label.is_some());
    assert!(addr_label_src.src.is_some());
    assert!(addr_label_src.is_ambiguous()); // Still ambiguous without ID
}

/// Test that TraversalMode and Direction types work correctly.
#[test]
fn test_enumerated_direction_modes_work() {
    // Test TraversalMode variations
    let modes = vec![
        TraversalMode::Bfs,
        TraversalMode::Dfs,
        TraversalMode::default(),
    ];
    assert_eq!(TraversalMode::default(), TraversalMode::Bfs);

    // Test Direction variations
    let directions = vec![
        Direction::In,
        Direction::Out,
        Direction::Both,
        Direction::default(),
    ];
    assert_eq!(Direction::default(), Direction::Both);

    // Test serialization/deserialization
    for mode in modes {
        let serialized = serde_json::to_value(mode).unwrap();
        let deserialized: TraversalMode = serde_json::from_value(serialized).unwrap();
        match mode {
            TraversalMode::Bfs => assert_eq!(deserialized, TraversalMode::Bfs),
            TraversalMode::Dfs => assert_eq!(deserialized, TraversalMode::Dfs),
        }
    }

    for direction in directions {
        let serialized = serde_json::to_value(direction).unwrap();
        let deserialized: Direction = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized, direction);
    }
}

/// The ADR-0025/0027/0029 parameters survive the trip through the client and
/// come back **identified**, field by field.
///
/// `InProcessClient::with_detailed_mode` echoes every field it received, which
/// is what makes this checkable: the assertions below fail if a field is
/// dropped, renamed, or silently defaulted anywhere between `DataQuery` and the
/// handler. Until 2026-07-28 this test asserted
/// `is_ok() || err.contains("not implemented")` against the plain
/// `InProcessClient`, which is satisfied by success, by the expected error, and
/// by nothing else panicking — a "does not panic" check with an integration
/// test's name (audit §K2a).
#[test]
fn test_enhanced_queries_integrated() {
    let client = InProcessClient::with_detailed_mode();

    // GetNode carries a typed NodeAddress (ADR-0027), not a bare string.
    let request = filigrio_protocol::contract::Request::Data(DataQuery::GetNode {
        project: "test".to_string(),
        node_address: NodeAddress::by_label_src("main", "src/main.rs"),
    });
    let data = match client
        .send(request)
        .expect("GetNode is answered in detailed mode")
    {
        Response::QueryResult { data } => data,
        other => panic!("expected a QueryResult, got {other:?}"),
    };
    assert_eq!(data["project"], "test");
    assert_eq!(
        data["node_address"]["label"], "main",
        "the label half of the address must reach the handler: {data}"
    );
    assert_eq!(
        data["node_address"]["src"], "src/main.rs",
        "the src half is what disambiguates a homonym — dropping it silently \
         re-ambiguates the query: {data}"
    );

    // Neighbors carries direction + relation filter + the ADR-0029 flag.
    let request = filigrio_protocol::contract::Request::Data(DataQuery::Neighbors {
        project: "test".to_string(),
        node: NodeAddress::by_id("fn:src/main.rs:main"),
        direction: Direction::In,
        relations: vec!["calls".to_string(), "imports".to_string()],
        include_unresolved: true,
    });
    let data = match client
        .send(request)
        .expect("Neighbors is answered in detailed mode")
    {
        Response::QueryResult { data } => data,
        other => panic!("expected a QueryResult, got {other:?}"),
    };
    assert_eq!(data["node"]["id"], "fn:src/main.rs:main");
    assert_eq!(
        data["direction"], "In",
        "`In` must not collapse to the `Both` default — the in/out distinction \
         is what ADR-0027 added: {data}"
    );
    assert_eq!(
        data["relations"],
        serde_json::json!(["calls", "imports"]),
        "the relation filter must arrive whole: {data}"
    );
    assert_eq!(
        data["include_unresolved"], true,
        "ADR-0029's flag is the one a grammar-constrained MCP client cannot \
         re-send if it is lost: {data}"
    );
}

/// Test that control-plane commands are available through the protocol.
#[test]
fn test_control_plane_commands_available() {
    let client = InProcessClient::new();

    // Test that ProjectRegister command exists and can be sent
    let add_cmd = Command::ProjectRegister {
        path: "/tmp/test_project".to_string(),
    };

    let request = filigrio_protocol::contract::Request::command(add_cmd);
    let response = client.send(request);

    // Should be accepted (may fail on non-existent path, but not on unknown command)
    assert!(
        response.is_ok()
            || response
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("project")
            || response.as_ref().unwrap_err().to_string().contains("path"),
        "ProjectRegister command should be recognized"
    );

    // Test daemon lifecycle commands. One verb today (F5/F6/F7 removed the rest);
    // the loop is over the *set*, which is what this test asserts about.
    #[allow(clippy::single_element_loop)]
    for cmd in [Command::DaemonStop] {
        let request = filigrio_protocol::contract::Request::command(cmd);
        let response = client.send(request);

        // Commands should be recognized, even if not fully implemented
        assert!(
            !response
                .as_ref()
                .unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("unknown command"),
            "Daemon commands should be recognized"
        );
    }
}
