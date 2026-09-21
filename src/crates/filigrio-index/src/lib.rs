//! filigrio-index — the `Extractor` port (HLD §4, ADR-0003/0010).
//!
//! Real tree-sitter extractors: [`rust::RustExtractor`] (`.rs`) and
//! [`python::PythonExtractor`] (`.py`), each parsing source into
//! definition nodes (`file`/`function`/`class`/`struct`/…) plus in-file
//! `calls`/`imports` edges as unresolved `Symbol` targets for `resolve` to link,
//! with receiver-type hints for method-homonym disambiguation.
//! [`mock::MockExtractor`] is the fallback for languages without a real
//! extractor yet. [`DispatchExtractor`] routes an artifact to the first
//! extractor that handles it.

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]
mod capability;
mod dispatch;
mod driver;
mod idgen;
mod mock;
mod python;
mod rust;
pub mod type_filter;
mod typescript;

// ADR-0040 oxc TS/JS frontend (feature-gated, off by default). See
// src/typescript_oxc.rs. Wired into the default dispatcher as a compile-time
// swap: under `--features ts-oxc`, `DispatchExtractor::with_defaults()` registers
// `TypeScriptOxcExtractor` in place of the tree-sitter `TypeScriptExtractor`
// (see dispatch.rs); the default (no-feature) build keeps the tree-sitter TS path.
#[cfg(feature = "ts-oxc")]
mod typescript_oxc;

pub use dispatch::DispatchExtractor;
pub use mock::MockExtractor;
pub use python::PythonExtractor;
pub use rust::base_type_name;
pub use rust::RustExtractor;
pub use type_filter::{should_filter_type, FilterStats};
pub use typescript::TypeScriptExtractor;

#[cfg(feature = "ts-oxc")]
pub use typescript_oxc::TypeScriptOxcExtractor;
