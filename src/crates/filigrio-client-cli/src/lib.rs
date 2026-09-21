//! filigrio-client-cli — the engine-free human CLI (ADR-0032f §1).
//!
//! The binary (`src/main.rs`) is the mask; this library is the part of it that
//! has behaviour worth testing without a socket and without a process. Today
//! that is exactly one thing: [`hook`], the client-side half of ADR-0032b.
//!
//! Engine-free is a **link-time** property, not a habit: nothing here may reach
//! for `filigrio-index`/`-resolve`/`-pipeline`. The hook needs `ChangeSet` and
//! the wire contract, both of which come through `filigrio-protocol`.

// ADR-0041's gate, held rather than narrated.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod hook;
