//! Tests for qualified path handling in Rust type extraction.
//!
//! These tests verify the correct logic for handling qualified paths:
//! 1. `crate::` / `self::` / `super::` / `Self::` → strip to last segment (definitionally local)
//! 2. Everything else → keep qualified (foreign until proven otherwise)

use filigrio_index::base_type_name;

#[test]
fn test_base_type_name_function() {
    // Local paths should be stripped
    assert_eq!(base_type_name("crate::LocalType"), "LocalType");
    assert_eq!(base_type_name("self::LocalType"), "LocalType");
    assert_eq!(base_type_name("super::LocalType"), "LocalType");
    assert_eq!(base_type_name("Self::LocalType"), "LocalType");

    // Foreign paths should be preserved (the fix!)
    assert_eq!(
        base_type_name("turbojpeg::PixelFormat"),
        "turbojpeg::PixelFormat"
    );
    assert_eq!(
        base_type_name("extern_crate::SomeType"),
        "extern_crate::SomeType"
    );
    assert_eq!(
        base_type_name("some::other::path::Type"),
        "some::other::path::Type"
    );

    // Std paths should be preserved
    assert_eq!(base_type_name("std::sync::Mutex"), "std::sync::Mutex");
    assert_eq!(
        base_type_name("core::option::Option"),
        "core::option::Option"
    );
    assert_eq!(base_type_name("alloc::vec::Vec"), "alloc::vec::Vec");

    // Generic types should have generics stripped, but path preserved
    assert_eq!(
        base_type_name("turbojpeg::PixelFormat<u8>"),
        "turbojpeg::PixelFormat"
    );
    assert_eq!(
        base_type_name("std::sync::Mutex<MyType>"),
        "std::sync::Mutex"
    );
}

#[test]
fn test_type_filter_with_qualified_paths() {
    use filigrio_index::type_filter::should_filter_type;
    use std::collections::HashSet;

    let empty_locals = HashSet::new();

    // Foreign qualified paths should not be filtered by denylist matching full path
    assert!(!should_filter_type(
        "rust",
        "turbojpeg::PixelFormat",
        &empty_locals
    ));

    // But std paths should be filtered by denylist matching last segment
    assert!(should_filter_type(
        "rust",
        "std::sync::Mutex",
        &empty_locals
    )); // "Mutex" is in denylist
    assert!(should_filter_type(
        "rust",
        "core::option::Option",
        &empty_locals
    )); // "Option" is in denylist
    assert!(should_filter_type("rust", "alloc::vec::Vec", &empty_locals)); // "Vec" is in denylist

    // HashMap is also in denylist
    assert!(should_filter_type(
        "rust",
        "std::collections::HashMap",
        &empty_locals
    )); // "HashMap" is in denylist
}

#[test]
fn test_end_to_end_rust_extraction() {
    use filigrio_core::{Artifact, ArtifactKind, EdgeTarget, Extractor};
    use filigrio_index::RustExtractor;

    let code = r#"
        // Test local path handling - should be stripped
        fn test_local() -> crate::LocalType {
            crate::LocalType::default()
        }
        
        // Test foreign path handling - should be preserved as qualified (THE FIX!)
        fn test_foreign() -> turbojpeg::PixelFormat {
            turbojpeg::PixelFormat::RGB
        }
        
        // Test another foreign path
        fn test_foreign_2() -> some::other::crate::ExternalType {
            some::other::crate::ExternalType::default()
        }
        
        struct LocalType;
        "#;

    let artifact = Artifact {
        path: "test.rs".to_string(),
        kind: ArtifactKind::Code,
        language: Some("rust".to_string()),
    };

    let extractor = RustExtractor::new();
    let extraction = extractor.extract(&artifact, code.as_bytes()).unwrap();

    // Check that we got the expected nodes and edges
    assert!(!extraction.nodes.is_empty(), "Should extract nodes");
    assert!(!extraction.edges.is_empty(), "Should extract edges");

    println!("=== All edges ===");
    for edge in &extraction.edges {
        println!("Relation: {}, Target: {:?}", edge.relation, edge.target);
    }

    // Check type/return edges specifically
    println!("\n=== Type/Return edges ===");
    for edge in &extraction.edges {
        if edge.relation == "type/return" {
            println!("Return type: {:?}", edge.target);
        }
    }

    // The key fix: turbojpeg::PixelFormat should appear as QUALIFIED (not stripped to "PixelFormat")
    let has_turbojpeg_qualified = extraction.edges.iter().any(|edge| {
        edge.relation == "type/return"
            && matches!(&edge.target, EdgeTarget::Symbol(s) if s.name == "turbojpeg::PixelFormat")
    });

    // Check the other foreign path is also preserved
    let _has_external_qualified = extraction.edges.iter().any(|edge| {
        edge.relation == "type/return"
            && matches!(
                &edge.target,
                EdgeTarget::Symbol(s) if s.name == "some::other::crate::ExternalType"
            )
    });

    // Local path should be stripped (crate::LocalType -> LocalType)
    let _has_local_stripped = extraction.edges.iter().any(|edge| {
        edge.relation == "type/return"
            && matches!(
                &edge.target,
                EdgeTarget::Symbol(s) if s.name == "LocalType" && !s.name.contains("crate::")
            )
    });

    // Check the other foreign path is also preserved
    let has_external_qualified = extraction.edges.iter().any(|edge| {
        edge.relation == "type/return" && 
        matches!(&edge.target, EdgeTarget::Symbol(s) if s.name == "some::other::crate::ExternalType")
    });

    // Local path should be stripped (crate::LocalType -> LocalType)
    let has_local_stripped = extraction.edges.iter().any(|edge| {
        edge.relation == "type/return"
            && matches!(
                &edge.target,
                EdgeTarget::Symbol(s) if s.name == "LocalType" && !s.name.contains("crate::")
            )
    });

    // Make sure the old broken behavior is NOT present
    let has_turbojpeg_broken = extraction.edges.iter().any(|edge| {
        edge.relation == "type/return" && 
        matches!(&edge.target, EdgeTarget::Symbol(s) if s.name == "PixelFormat" && !s.name.contains("turbojpeg"))
    });

    println!("\n=== Test Results ===");
    println!(
        "Has turbojpeg::PixelFormat QUALIFIED (fix working): {}",
        has_turbojpeg_qualified
    );
    println!(
        "Has some::other::crate::ExternalType QUALIFIED (fix working): {}",
        has_external_qualified
    );
    println!("Has LocalType STRIPPED (correct): {}", has_local_stripped);
    println!(
        "Has broken 'PixelFormat' without prefix (should be false): {}",
        has_turbojpeg_broken
    );

    // MAIN FIX: Third-party paths must be kept qualified
    assert!(
        has_turbojpeg_qualified,
        "Main fix: turbojpeg::PixelFormat should be kept QUALIFIED"
    );
    assert!(
        has_external_qualified,
        "Main fix: all foreign paths should be kept QUALIFIED"
    );

    // Local paths should still be stripped correctly
    assert!(
        has_local_stripped,
        "Local paths (crate::, self::, etc.) should be stripped"
    );

    // The old broken behavior should NOT be present
    assert!(
        !has_turbojpeg_broken,
        "Old broken behavior (stripping foreign paths) should NOT occur"
    );
}
