//! The `module_of` bridge (ADR-0021) — the per-language rule that maps a
//! *physical* file to the *semantic* **module** (the namespace / import-export
//! unit) it belongs to.
//!
//! This is where language knowledge first enters the semantic layer: modules are
//! semantic, but discovered from physical structure. Today it is **identity** —
//! for TS/JS/Python **and Rust** a module *is* a file — so this is a no-op seam.
//! Its job is to give the concept a name in code: the export graph and import
//! scope key on a `ModuleId`, not "a file that we happen to treat as a module."
//!
//! **Grouping vs. resolution — the Rust lesson.** It is tempting to think Rust
//! forces a mod-tree `module_of` (`crate::a::b`). It does *not*: a Rust file is
//! still exactly one module, so grouping stays identity. Rust's mod-tree path is
//! a **resolution** concern — mapping a `use`/`pub use` *specifier* (`crate::a`)
//! to a target file — which lives in the `ModuleResolver` (`resolve_rust`), not
//! here. (Inline `mod foo { … }` is the sole exception where a file holds several
//! modules; not yet modeled.) So the `pub use` gap (ADR-0020) was closed without
//! touching this seam.
//!
//! The genuine forcing function for a non-identity `module_of` is a language
//! where **many files share one module**: **Go** (`module_of = dir(file)`,
//! package = directory) or **C#** (a namespace spans files). That is when this
//! upgrades to a context-carrying rule (likely a `ModuleGrouping` trait — Rule
//! of Three).

/// The identity of a semantic module. A file path today (identity grouping —
/// TS/JS/Python/Rust); a directory (Go) or namespace (C#) once a genuinely
/// many-files-to-one-module rule lands. Kept a `String` alias — a newtype earns
/// its keep only when that second rule forces distinct construction.
pub type ModuleId = String;

/// The module a physical `file` belongs to. Identity for the languages that ship
/// today (module = file); the single seam a new language's grouping rule edits.
pub fn module_of(file: &str) -> ModuleId {
    file.to_string()
}
