//! `DispatchExtractor` — routes an artifact to the first registered extractor
//! that `handles` it. Order matters: specific real extractors first, the mock
//! fallback last. This is how the pipeline runs mixed-language repos while only
//! some languages have real extractors (Phase 1 → Phase 4).

#[cfg(not(feature = "ts-oxc"))]
use crate::TypeScriptExtractor;
#[cfg(feature = "ts-oxc")]
use crate::TypeScriptOxcExtractor;
use crate::{MockExtractor, PythonExtractor, RustExtractor};
use filigrio_core::{Artifact, Error, Extraction, Extractor, Result};

pub struct DispatchExtractor {
    extractors: Vec<Box<dyn Extractor>>,
}

impl DispatchExtractor {
    pub fn new(extractors: Vec<Box<dyn Extractor>>) -> Self {
        DispatchExtractor { extractors }
    }

    /// Real Rust + Python + TS/JS extractors first; mock fallback for the rest.
    ///
    /// The TS/JS slot is chosen at compile time (ADR-0040): the tree-sitter
    /// `TypeScriptExtractor` by default, or the oxc `TypeScriptOxcExtractor`
    /// under `--features ts-oxc`. The default (no-feature) build is unchanged.
    pub fn with_defaults() -> Self {
        Self::new(vec![
            Box::new(RustExtractor::new()),
            Box::new(PythonExtractor::new()),
            #[cfg(not(feature = "ts-oxc"))]
            Box::new(TypeScriptExtractor::new()),
            #[cfg(feature = "ts-oxc")]
            Box::new(TypeScriptOxcExtractor::new()),
            Box::new(MockExtractor::new()),
        ])
    }
}

impl Default for DispatchExtractor {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl Extractor for DispatchExtractor {
    fn handles(&self, artifact: &Artifact) -> bool {
        self.extractors.iter().any(|e| e.handles(artifact))
    }

    fn extract(&self, artifact: &Artifact, bytes: &[u8]) -> Result<Extraction> {
        for e in &self.extractors {
            if e.handles(artifact) {
                return e.extract(artifact, bytes);
            }
        }
        Err(Error::Parse(format!("no extractor for {}", artifact.path)))
    }
}

// ADR-0040: prove the default dispatcher routes `.ts` through the oxc extractor
// when built `--features ts-oxc`. The observable divergence used here: a TypeScript
// **overload** signature in a class body. oxc mints a `function` node per
// signature (3 `send` nodes for one method), tree-sitter only for the
// implementation — the `class_overload_signature` entry in the frontend parity
// register, which is where the direction is pinned and diagnosed.
//
// **The discriminator is deliberately taken from that register, and it has moved
// twice.** It was once the `class` node, until tree-sitter's
// `abstract_class_declaration` gap was fixed and the test went vacuous; then the
// bodiless `abstract go(): void`, until ADR-0036 §5 made *both* frontends mint
// it. Each time, the routing proof silently stopped proving anything while
// staying green. A discriminator is only sound while a divergence is registered
// and asserted to still hold, so read it from there — and when
// `class_overload_signature` is closed, this test must move again rather than be
// left passing on an observable both frontends now produce.
#[cfg(all(test, feature = "ts-oxc"))]
mod ts_oxc_routing {
    use super::*;
    use filigrio_core::ArtifactKind;

    // Empirically verified with both extractors: both mint the implementation
    // `send`; only oxc mints a node per overload signature as well.
    const OVERLOADED_METHOD: &str =
        "class Api { send(x: string): string; send(x: number): number; send(x: any): any { return x } }";

    fn ts_artifact() -> Artifact {
        Artifact {
            path: "src/demo.ts".into(),
            kind: ArtifactKind::Code,
            language: Some("typescript".into()),
        }
    }

    #[test]
    fn default_dispatch_routes_ts_through_oxc() {
        let dispatch = DispatchExtractor::with_defaults();
        let ex = dispatch
            .extract(&ts_artifact(), OVERLOADED_METHOD.as_bytes())
            .expect("dispatch extracts the .ts artifact");

        // Both frontends emit the class and the implementation, so this is a
        // sanity check on the extraction, not the routing proof.
        let class_node = ex
            .nodes
            .iter()
            .find(|n| n.label == "Api")
            .unwrap_or_else(|| panic!("no `Api` node at all; nodes={:?}", ex.nodes));
        assert_eq!(class_node.kind, "class");

        // The routing proof: only oxc mints a node per overload signature.
        let sends = ex
            .nodes
            .iter()
            .filter(|n| n.label == "send" && n.kind == "function")
            .count();
        assert!(
            sends > 1,
            "expected one `send` node per overload signature (oxc); got {sends} — \
             tree-sitter path? nodes={:?}",
            ex.nodes.iter().map(|n| &n.id.0).collect::<Vec<_>>()
        );
    }
}
