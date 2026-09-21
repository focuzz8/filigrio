//! Type noise suppression system (ADR-0036 §R1.1).
//!
//! Filters noise edges that point at generic, primitive, or ubiquitous types
//! which don't represent meaningful structural relationships. Implements:
//!   * Per-language type denylists (primitives, std types, common containers)
//!   * Generic type parameter suppression (`T`, `U`, `V`, etc.)
//!   * Central filtering function for all type relation emissions
//!
//! This module ensures that edge emissions for `type/field`, `type/param`,
//! and `type/return` relationships exclude noise types, enabling meaningful
//! R3 gate assessments and clean baselines for Task 7 enum variant edges.

use std::collections::HashSet;

// ---- Denylist Rule ----------------------------------------------------------
//
// **Denylist Rule: "A type that can never be a project node"**
//
// This includes:
// - Standard library primitive types (String, bool, i32, etc.)
// - Standard library containers where payload is the interesting part (Vec, HashMap, etc.)
// - Standard library utility types that are never user-defined (Path, PathBuf, Duration, etc.)
//
// Future entries should follow this rule: types that can never appear as user-defined code definitions.
// This makes denylist entries decidable rather than ad hoc.

// ---- Rust type denylist -----------------------------------------------------

/// Rust standard library and primitive types that should be filtered as noise.
/// These are ubiquitous types that don't represent meaningful structural relationships.
fn get_rust_denylist() -> HashSet<&'static str> {
    let mut set = HashSet::new();

    // Standard library result/error types
    set.extend(["Result", "Option", "String"]);

    // Smart pointers
    set.extend(["Box", "Rc", "Arc", "Cow"]);

    // Primitive types
    set.extend(["bool"]);
    set.extend(["u8", "u16", "u32", "u64", "u128", "usize"]);
    set.extend(["i8", "i16", "i32", "i64", "i128", "isize"]);
    set.extend(["f32", "f64"]);
    set.extend(["char", "str"]);

    // Collection types (payload survives per R2 folding)
    set.extend(["Vec", "VecDeque", "LinkedList", "BinaryHeap"]);
    set.extend(["HashMap", "HashSet", "BTreeMap", "BTreeSet"]);

    // Smart pointer wrappers
    set.extend(["RefCell", "Mutex", "RwLock"]);

    // Async primitives
    set.extend(["Future", "Stream", "Pin", "Poll", "Waker"]);

    // std::path types (never project nodes)
    set.extend(["Path", "PathBuf"]);

    // std::time types (never project nodes)
    set.extend(["Duration", "Instant", "SystemTime"]);

    set
}

// ---- Python type denylist ---------------------------------------------------

/// Python built-in and standard library types that should be filtered as noise.
/// Matches graphify's _PYTHON_ANNOTATION_NOISE behavior for consistency.
fn get_python_denylist() -> HashSet<&'static str> {
    let mut set = HashSet::new();

    // Built-in types (scalar + complex)
    set.extend([
        "bool",
        "int",
        "float",
        "str",
        "list",
        "dict",
        "tuple",
        "set",
        "bytes",
        "bytearray",
        "complex",
        "object",
        "True",
        "False",
    ]);

    // Common generic aliases (from typing module)
    set.extend([
        "List", "Dict", "Tuple", "Set", "Optional", "Any", "Union", "Callable", "Iterable",
        "Iterator",
    ]);

    // Typing containers and utilities
    set.extend([
        "Sequence",
        "Mapping",
        "MutableMapping",
        "MutableSequence",
        "Awaitable",
        "AsyncIterable",
        "AsyncIterator",
        "Coroutine",
        "Generator",
        "AsyncGenerator",
        "ContextManager",
        "AsyncContextManager",
        "Annotated",
        "ClassVar",
        "Final",
        "Literal",
        "Concatenate",
        "ParamSpec",
        "TypeVar",
        "None",
        "Ellipsis",
        "Type",
    ]);

    // unittest.mock and test utilities
    set.extend([
        "MagicMock",
        "Mock",
        "AsyncMock",
        "NonCallableMock",
        "NonCallableMagicMock",
        "PropertyMock",
        "patch",
        "sentinel",
    ]);

    set
}

// ---- TypeScript type denylist ------------------------------------------------

/// TypeScript built-in and standard library types that should be filtered as noise.
/// These are ubiquitous types that don't represent meaningful structural relationships.
fn get_typescript_denylist() -> HashSet<&'static str> {
    let mut set = HashSet::new();

    // Primitive types (built-in keywords)
    set.extend(["string", "number", "boolean", "bigint", "symbol"]);
    set.extend(["any", "unknown", "never", "void"]);
    set.extend(["null", "undefined"]);

    // Collection types (payload survives per R2 folding)
    set.extend(["Array", "ReadonlyArray"]);
    set.extend(["Map", "ReadonlyMap", "Set", "ReadonlySet"]);
    set.extend(["WeakMap", "WeakSet"]);

    // Promise and async types
    set.extend(["Promise"]);

    // Object and utility types (never project nodes)
    set.extend(["Object", "Record"]);
    set.extend(["Partial", "Required", "Readonly"]);
    set.extend(["Pick", "Omit", "Exclude", "Extract"]);
    set.extend(["ReturnType", "Parameters", "InstanceType"]);

    // Common utility types from @types/node
    set.extend(["Buffer"]);

    set
}

// ---- Generic type parameters ------------------------------------------------

/// Type parameter names are collected syntactically and passed to `should_filter_type_with_context`.
/// No name-based pattern matching - use actual declared type parameters from AST.
// ---- Main filtering function ------------------------------------------------
/// Determine if a type should be filtered as noise based on language and type name.
///
/// This function implements the R1.1 noise suppression logic:
/// - Filters language-specific denylist types, unless they have local definitions
/// - Uses syntactically-collected type parameters instead of name patterns
/// - Returns `true` if the type should be filtered (not emitted), `false` otherwise
///
/// The `local_types` parameter contains names of types that are defined within the same file,
/// so they should not be filtered even if they match denylist patterns.
pub fn should_filter_type(language: &str, type_name: &str, local_types: &HashSet<String>) -> bool {
    should_filter_type_with_context(language, type_name, local_types, &HashSet::new())
}

/// Determine if a type should be filtered as noise with syntactic type parameter context.
///
/// This function implements the R1.1 noise suppression logic with actual syntactic
/// type parameter collection instead of name pattern matching:
/// - Filters actual declared type parameters (T, U, Item, Error, etc.) from the current context
/// - Filters language-specific denylist types, unless they have local definitions
/// - Returns `true` if the type should be filtered (not emitted), `false` otherwise
///
/// The `local_types` parameter contains names of types that are defined within the same file.
/// The `type_parameters` parameter contains actual declared type parameters from the current context.
pub fn should_filter_type_with_context(
    language: &str,
    type_name: &str,
    local_types: &HashSet<String>,
    type_parameters: &HashSet<String>,
) -> bool {
    let cleaned_type = type_name.trim();

    // Skip empty types
    if cleaned_type.is_empty() {
        return true;
    }

    // Extract the last segment for denylist matching
    // This handles qualified paths like `std::sync::Mutex` where we only check last segment
    let last_segment = cleaned_type.rsplit("::").next().unwrap_or(cleaned_type);

    // Check if it's an actual declared type parameter in the current context
    // Type parameters override local types (Rust scoping rule)
    if type_parameters.contains(cleaned_type) {
        return true;
    }

    // If the type is defined locally, don't filter it (it's a real user type)
    // This check comes SECOND because type parameters take precedence over local types
    if local_types.contains(cleaned_type) {
        return false;
    }

    // Check language-specific denylists using last segment for qualified paths
    match language.to_lowercase().as_str() {
        "rust" => get_rust_denylist().contains(last_segment),
        "python" => get_python_denylist().contains(last_segment),
        "typescript" | "javascript" => get_typescript_denylist().contains(last_segment),
        _ => false, // Don't filter for unknown languages
    }
}

// ---- Statistics tracking ----------------------------------------------------

/// Tracking statistics for noise filtering to measure impact on edge counts.
#[derive(Debug, Clone, Default)]
pub struct FilterStats {
    /// Total number of type edges considered for emission
    pub total_considered: usize,
    /// Number of edges filtered as noise
    pub filtered_count: usize,
    /// Number of edges actually emitted
    pub emitted_count: usize,
}

impl FilterStats {
    /// Create a new statistics tracker
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a type being evaluated for emission
    pub fn record_considered(&mut self) {
        self.total_considered += 1;
    }

    /// Record a type being filtered as noise
    pub fn record_filtered(&mut self) {
        self.filtered_count += 1;
    }

    /// Record a type being emitted (not filtered)
    pub fn record_emitted(&mut self) {
        self.emitted_count += 1;
    }

    /// Get the noise rate as a percentage (filtered / total_considered)
    pub fn noise_rate(&self) -> f64 {
        if self.total_considered == 0 {
            0.0
        } else {
            (self.filtered_count as f64 / self.total_considered as f64) * 100.0
        }
    }
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_denylist_filters_primitives() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("rust", "u32", &empty_locals));
        assert!(should_filter_type("rust", "i64", &empty_locals));
        assert!(should_filter_type("rust", "bool", &empty_locals));
        assert!(should_filter_type("rust", "f32", &empty_locals));
        assert!(should_filter_type("rust", "char", &empty_locals));
    }

    #[test]
    fn test_rust_denylist_filters_std_types() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("rust", "Result", &empty_locals));
        assert!(should_filter_type("rust", "Option", &empty_locals));
        assert!(should_filter_type("rust", "String", &empty_locals));
        assert!(should_filter_type("rust", "Vec", &empty_locals));
        assert!(should_filter_type("rust", "HashMap", &empty_locals));
    }

    #[test]
    fn test_rust_denylist_filters_std_path_types() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("rust", "Path", &empty_locals));
        assert!(should_filter_type("rust", "PathBuf", &empty_locals));
    }

    #[test]
    fn test_rust_denylist_filters_std_time_types() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("rust", "Duration", &empty_locals));
        assert!(should_filter_type("rust", "Instant", &empty_locals));
        assert!(should_filter_type("rust", "SystemTime", &empty_locals));
    }

    #[test]
    fn test_rust_denylist_preserves_app_types() {
        let empty_locals = HashSet::new();
        assert!(!should_filter_type("rust", "MyStruct", &empty_locals));
        assert!(!should_filter_type("rust", "CustomType", &empty_locals));
        assert!(!should_filter_type("rust", "Widget", &empty_locals));
        assert!(!should_filter_type("rust", "ProcessManager", &empty_locals));
    }

    #[test]
    fn test_generic_parameters_filtered_with_context() {
        let empty_locals = HashSet::new();
        let empty_params = HashSet::new();

        // Without type parameters context, these should not be filtered
        assert!(!should_filter_type_with_context(
            "rust",
            "T",
            &empty_locals,
            &empty_params
        ));
        assert!(!should_filter_type_with_context(
            "rust",
            "Item",
            &empty_locals,
            &empty_params
        ));

        // With type parameters context, actual parameters should be filtered
        let mut params = HashSet::new();
        params.insert("T".to_string());
        params.insert("Item".to_string());
        params.insert("Error".to_string());

        assert!(should_filter_type_with_context(
            "rust",
            "T",
            &empty_locals,
            &params
        ));
        assert!(should_filter_type_with_context(
            "rust",
            "Item",
            &empty_locals,
            &params
        ));
        assert!(should_filter_type_with_context(
            "rust",
            "Error",
            &empty_locals,
            &params
        ));

        // Types not in parameters should not be filtered
        assert!(!should_filter_type_with_context(
            "rust",
            "RealType",
            &empty_locals,
            &params
        ));

        // Type parameters should take precedence over local types
        // (e.g., struct T impl<T> -> T refers to the parameter, not the struct)
        let mut locals = HashSet::new();
        locals.insert("T".to_string());
        locals.insert("RealType".to_string());

        assert!(
            should_filter_type_with_context("rust", "T", &locals, &params),
            "Type T should be filtered when both in local_types and type_parameters (parameter precedence)"
        );
        assert!(
            !should_filter_type_with_context("rust", "RealType", &locals, &params),
            "Type RealType should NOT be filtered when only in local_types"
        );
    }

    #[test]
    fn test_generic_parameters_filtered_python_with_context() {
        let empty_locals = HashSet::new();
        let empty_params = HashSet::new();

        // Without context, should not filter
        assert!(!should_filter_type_with_context(
            "python",
            "T",
            &empty_locals,
            &empty_params
        ));
        assert!(!should_filter_type_with_context(
            "python",
            "Item",
            &empty_locals,
            &empty_params
        ));

        // With type parameters context
        let mut params = HashSet::new();
        params.insert("T".to_string());
        params.insert("Item".to_string());

        assert!(should_filter_type_with_context(
            "python",
            "T",
            &empty_locals,
            &params
        ));
        assert!(should_filter_type_with_context(
            "python",
            "Item",
            &empty_locals,
            &params
        ));
    }

    #[test]
    fn test_python_denylist_filters_builtins() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("python", "int", &empty_locals));
        assert!(should_filter_type("python", "float", &empty_locals));
        assert!(should_filter_type("python", "str", &empty_locals));
        assert!(should_filter_type("python", "list", &empty_locals));
        assert!(should_filter_type("python", "dict", &empty_locals));
    }

    #[test]
    fn test_typescript_denylist_filters_primitives() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("typescript", "string", &empty_locals));
        assert!(should_filter_type("typescript", "number", &empty_locals));
        assert!(should_filter_type("typescript", "boolean", &empty_locals));
        assert!(should_filter_type("typescript", "bigint", &empty_locals));
        assert!(should_filter_type("typescript", "symbol", &empty_locals));
    }

    #[test]
    fn test_typescript_denylist_filters_special_types() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("typescript", "any", &empty_locals));
        assert!(should_filter_type("typescript", "unknown", &empty_locals));
        assert!(should_filter_type("typescript", "never", &empty_locals));
        assert!(should_filter_type("typescript", "void", &empty_locals));
        assert!(should_filter_type("typescript", "null", &empty_locals));
        assert!(should_filter_type("typescript", "undefined", &empty_locals));
    }

    #[test]
    fn test_typescript_denylist_filters_collections() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("typescript", "Array", &empty_locals));
        assert!(should_filter_type(
            "typescript",
            "ReadonlyArray",
            &empty_locals
        ));
        assert!(should_filter_type("typescript", "Map", &empty_locals));
        assert!(should_filter_type("typescript", "Set", &empty_locals));
        assert!(should_filter_type("typescript", "WeakMap", &empty_locals));
        assert!(should_filter_type("typescript", "WeakSet", &empty_locals));
    }

    #[test]
    fn test_typescript_denylist_filters_object_types() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("typescript", "Object", &empty_locals));
        assert!(should_filter_type("typescript", "Record", &empty_locals));
        assert!(should_filter_type("typescript", "Promise", &empty_locals));
    }

    #[test]
    fn test_typescript_denylist_filters_utility_types() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("typescript", "Partial", &empty_locals));
        assert!(should_filter_type("typescript", "Required", &empty_locals));
        assert!(should_filter_type("typescript", "Readonly", &empty_locals));
        assert!(should_filter_type("typescript", "Pick", &empty_locals));
        assert!(should_filter_type("typescript", "Omit", &empty_locals));
        assert!(should_filter_type("typescript", "Exclude", &empty_locals));
        assert!(should_filter_type("typescript", "Extract", &empty_locals));
        assert!(should_filter_type(
            "typescript",
            "ReturnType",
            &empty_locals
        ));
        assert!(should_filter_type(
            "typescript",
            "Parameters",
            &empty_locals
        ));
    }

    #[test]
    fn test_typescript_denylist_preserves_app_types() {
        let empty_locals = HashSet::new();
        assert!(!should_filter_type("typescript", "MyClass", &empty_locals));
        assert!(!should_filter_type(
            "typescript",
            "CustomInterface",
            &empty_locals
        ));
        assert!(!should_filter_type("typescript", "Widget", &empty_locals));
        assert!(!should_filter_type(
            "typescript",
            "ProcessManager",
            &empty_locals
        ));
    }

    #[test]
    fn test_typescript_generic_patterns_filtered() {
        let empty_locals = HashSet::new();
        let empty_params = HashSet::new();

        // Without context, should not filter pattern-based names
        assert!(!should_filter_type_with_context(
            "typescript",
            "T",
            &empty_locals,
            &empty_params
        ));
        assert!(!should_filter_type_with_context(
            "typescript",
            "Item",
            &empty_locals,
            &empty_params
        ));

        // With type parameters context, actual parameters should be filtered
        let mut params = HashSet::new();
        params.insert("T".to_string());
        params.insert("U".to_string());
        params.insert("Item".to_string());
        params.insert("Value".to_string());

        assert!(should_filter_type_with_context(
            "typescript",
            "T",
            &empty_locals,
            &params
        ));
        assert!(should_filter_type_with_context(
            "typescript",
            "U",
            &empty_locals,
            &params
        ));
        assert!(should_filter_type_with_context(
            "typescript",
            "Item",
            &empty_locals,
            &params
        ));
        assert!(should_filter_type_with_context(
            "typescript",
            "Value",
            &empty_locals,
            &params
        ));
    }

    #[test]
    fn test_javascript_uses_typescript_denylist() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("javascript", "string", &empty_locals));
        assert!(should_filter_type("javascript", "number", &empty_locals));
        assert!(should_filter_type("javascript", "Array", &empty_locals));
        assert!(should_filter_type("javascript", "Object", &empty_locals));
    }

    #[test]
    fn test_empty_types_filtered() {
        let empty_locals = HashSet::new();
        assert!(should_filter_type("rust", "", &empty_locals));
        assert!(should_filter_type("rust", "  ", &empty_locals));
    }

    #[test]
    fn test_case_sensitive_matching() {
        let empty_locals = HashSet::new();
        assert!(!should_filter_type("rust", "result", &empty_locals)); // lowercase should NOT match
        assert!(should_filter_type("rust", "Result", &empty_locals)); // uppercase should match
        assert!(!should_filter_type("rust", "RESULT", &empty_locals)); // all caps should NOT match (case sensitive)
    }

    #[test]
    fn test_unknown_language_not_filtered() {
        let empty_locals = HashSet::new();
        let empty_params = HashSet::new();
        // Unknown languages don't apply language-specific filtering
        assert!(!should_filter_type_with_context(
            "unknown",
            "String",
            &empty_locals,
            &empty_params
        ));
        assert!(!should_filter_type_with_context(
            "unknown",
            "int",
            &empty_locals,
            &empty_params
        ));
    }

    #[test]
    fn test_filter_stats_tracking() {
        let mut stats = FilterStats::new();

        // Record some activity
        stats.record_considered();
        stats.record_emitted(); // Not filtered

        stats.record_considered();
        stats.record_filtered(); // Filtered

        stats.record_considered();
        stats.record_filtered(); // Filtered

        assert_eq!(stats.total_considered, 3);
        assert_eq!(stats.emitted_count, 1);
        assert_eq!(stats.filtered_count, 2);
        assert!((stats.noise_rate() - 66.66).abs() < 0.1); // ~66.66% noise rate
    }

    #[test]
    fn test_filter_stats_empty() {
        let stats = FilterStats::new();
        assert_eq!(stats.total_considered, 0);
        assert_eq!(stats.emitted_count, 0);
        assert_eq!(stats.filtered_count, 0);
        assert_eq!(stats.noise_rate(), 0.0);
    }

    #[test]
    fn test_complex_rust_types_not_denied() {
        let empty_locals = HashSet::new();
        // These are more complex types that should be kept
        assert!(!should_filter_type(
            "rust",
            "MyGenericStruct<u32>",
            &empty_locals
        ));
        assert!(!should_filter_type(
            "rust",
            "CustomResult<T, E>",
            &empty_locals
        ));
        assert!(!should_filter_type(
            "rust",
            "ApplicationError",
            &empty_locals
        ));
    }

    #[test]
    fn test_local_types_not_filtered() {
        // Local types should not be filtered even if they match denylist patterns
        let mut local_types = HashSet::new();
        local_types.insert("Option".to_string());
        local_types.insert("Result".to_string());
        local_types.insert("Vec".to_string());
        let empty_params = HashSet::new();

        assert!(
            !should_filter_type_with_context("rust", "Option", &local_types, &empty_params),
            "Should not filter local Option"
        );
        assert!(
            !should_filter_type_with_context("rust", "Result", &local_types, &empty_params),
            "Should not filter local Result"
        );
        assert!(
            !should_filter_type_with_context("rust", "Vec", &local_types, &empty_params),
            "Should not filter local Vec"
        );

        // But non-local types should still be filtered
        assert!(should_filter_type_with_context(
            "rust",
            "String",
            &local_types,
            &empty_params
        ));
        assert!(should_filter_type_with_context(
            "rust",
            "HashMap",
            &local_types,
            &empty_params
        ));

        // Type parameters are filtered using context, not local types
        let mut param_context = HashSet::new();
        param_context.insert("T".to_string());
        param_context.insert("Item".to_string());

        assert!(should_filter_type_with_context(
            "rust",
            "T",
            &local_types,
            &param_context
        ));
        assert!(should_filter_type_with_context(
            "rust",
            "Item",
            &local_types,
            &param_context
        ));

        // Types not in parameter context should not be filtered
        assert!(!should_filter_type_with_context(
            "rust",
            "T",
            &local_types,
            &empty_params
        ));
        assert!(!should_filter_type_with_context(
            "rust",
            "Item",
            &local_types,
            &empty_params
        ));
    }

    #[test]
    fn test_typescript_local_types_not_filtered() {
        let empty_params = HashSet::new();
        // Local types should not be filtered even if they match denylist patterns
        let mut local_types = HashSet::new();
        local_types.insert("String".to_string());
        local_types.insert("Array".to_string());
        local_types.insert("Promise".to_string());

        assert!(
            !should_filter_type_with_context("typescript", "String", &local_types, &empty_params),
            "Should not filter local String"
        );
        assert!(
            !should_filter_type_with_context("typescript", "Array", &local_types, &empty_params),
            "Should not filter local Array"
        );
        assert!(
            !should_filter_type_with_context("typescript", "Promise", &local_types, &empty_params),
            "Should not filter local Promise"
        );

        // But non-local types should still be filtered
        assert!(should_filter_type_with_context(
            "typescript",
            "number",
            &local_types,
            &empty_params
        ));
        assert!(should_filter_type_with_context(
            "typescript",
            "Object",
            &local_types,
            &empty_params
        ));

        // Type parameters are filtered using context
        let mut param_context = HashSet::new();
        param_context.insert("T".to_string());
        param_context.insert("Item".to_string());

        assert!(should_filter_type_with_context(
            "typescript",
            "T",
            &local_types,
            &param_context
        ));
        assert!(should_filter_type_with_context(
            "typescript",
            "Item",
            &local_types,
            &param_context
        ));
    }

    #[test]
    fn test_cross_file_error_type_not_filtered() {
        // Test that a type named "Error" defined in another file is not filtered
        // This was the original bug - name-based filtering incorrectly caught real types

        let empty_locals = HashSet::new(); // Error is not defined locally in this file
        let empty_params = HashSet::new(); // "Error" is not a type parameter here

        // "Error" should NOT be filtered when it's not in the type parameter context
        assert!(!should_filter_type_with_context(
            "rust",
            "Error",
            &empty_locals,
            &empty_params
        ));

        // But if "Error" IS a type parameter in the current context, it should be filtered
        let mut params = HashSet::new();
        params.insert("Error".to_string());
        assert!(should_filter_type_with_context(
            "rust",
            "Error",
            &empty_locals,
            &params
        ));
    }
}
