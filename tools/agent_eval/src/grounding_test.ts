import { assert, assertEquals } from "@std/assert";
import {
  checkGrounding,
  factsFromGraph,
  parseCitations,
  parseEdgeClaims,
  parseSections,
} from "./grounding.ts";

// The evidence grammar (ADR-0030): grounding reads the structured EVIDENCE block, so
// only the two shapes the tools emit and the skill prescribes are parsed —
// `label [src=FILE loc=LOC …]`, `… [id=ID …]`, and edges `A --rel--> B`. Freeform
// prose (`the method [io.rs:L1]`) is never grounded, so no stopword/tag heuristics.

const GRAPH = {
  nodes: [
    { id: "fn:io.rs:parse", label: "parse", source_file: "src/io.rs" },
    { id: "fn:io.rs:read", label: "read", source_file: "src/io.rs" },
    { id: "fn:core.rs:plan", label: "plan", source_file: "src/core.rs" },
  ],
  edges: [
    { source: "fn:io.rs:parse", target: "fn:io.rs:read", relation: "calls" },
    { source: "fn:io.rs:parse", target: "fn:core.rs:plan", relation: "calls" },
  ],
};
const facts = factsFromGraph(GRAPH);

// ---- section parsing (ADR-0030) --------------------------------------------

Deno.test("parseSections splits STATUS / EVIDENCE / WHY and grounds evidence only", () => {
  // The WHY prose ("...at [src/io.rs]", "the method [src/io.rs]") would trip a
  // whole-answer grounder; scoped to EVIDENCE it is never parsed — no stopword list.
  // WHY loosely names a helper in citation shape — the kind of aside grounding must
  // NOT judge (it is prose, not a claim the answer rests on).
  const answer = [
    "STATUS: OK",
    "RESULT: [FOUND]",
    "EVIDENCE:",
    "  parse [src=src/io.rs loc=L1]",
    "  read [src=src/io.rs loc=L9]",
    "  parse --calls--> read",
    "WHY: it delegates to some helper_fn [src=src/io.rs loc=L2] under the hood.",
  ].join("\n");
  const s = parseSections(answer);
  assertEquals(s.status, "OK"); // execution status
  assertEquals(s.result, "FOUND"); // outcome verdict
  assert(s.evidence.includes("parse") && !s.evidence.includes("WHY"));
  assert(s.why.includes("delegates"));
  const g = checkGrounding(s.evidence, facts);
  assert(g.ok, g.reasons.join("; "));
  assertEquals(g.edges.grounded.length, 1);
  assertEquals(g.citations.grounded.length, 2);
  // Whole-answer grounding WOULD flag the WHY aside (`helper_fn` is no node) — proof
  // that scoping to the EVIDENCE block, not the prose, is what keeps it honest.
  assert(!checkGrounding(answer, facts).ok, "whole-answer grounding trips on the WHY aside");
});

Deno.test("STATUS/RESULT parsing is lenient: markdown emphasis and missing tags", () => {
  assertEquals(parseSections("**RESULT:** [REACHES]\nEVIDENCE:").result, "REACHES");
  assertEquals(parseSections("RESULT:** FOUND").result, "FOUND");
  assertEquals(parseSections("STATUS: **INCOMPLETE**").status, "INCOMPLETE");
  // No RESULT → UNVERIFIED; no STATUS → UNKNOWN; no EVIDENCE header → nothing to ground.
  const s = parseSections("I could not determine the answer.");
  assertEquals(s.result, "UNVERIFIED");
  assertEquals(s.status, "UNKNOWN");
  assertEquals(s.evidence, "");
  assert(checkGrounding(s.evidence, facts).ok, "empty evidence never fabricates");
});

// ---- citation + edge parsing ------------------------------------------------

Deno.test("the tool-echoed `label [src=FILE loc=LOC]` form parses and grounds", () => {
  const cs = parseCitations("parse [src=src/io.rs loc=L1 community=parse]");
  assertEquals(cs.length, 1);
  assertEquals(cs[0], { label: "parse", file: "src/io.rs", loc: "L1" });
  assert(checkGrounding("parse [src=src/io.rs loc=L1 community=parse]", facts).ok);
});

Deno.test("an edge-tag bracket copied into evidence is not read as a citation", () => {
  // A model that pastes a get_neighbors line `--> read [calls] [UNRESOLVED]` must not
  // have `read [calls]` read as a `label [file=calls]` citation — the grammar only
  // recognizes `[src=…]`/`[id=…]`, so a bare `[calls]` is inert.
  assertEquals(parseCitations("read [calls] [UNRESOLVED]").length, 0);
});

Deno.test("parseEdgeClaims reads the arrow form with/without a tag", () => {
  const es = parseEdgeClaims("parse --calls [EXTRACTED]--> read, and parse --calls--> plan");
  assertEquals(es, [
    { a: "parse", rel: "calls", b: "read" },
    { a: "parse", rel: "calls", b: "plan" },
  ]);
});

// ---- grounding verdicts -----------------------------------------------------

Deno.test("a fully grounded evidence block passes", () => {
  const g = checkGrounding(
    "parse [src=src/io.rs loc=L1]\nread [src=src/io.rs loc=L9]\nparse --calls--> read",
    facts,
  );
  assert(g.ok, g.reasons.join("; "));
  assertEquals(g.citations.grounded.length, 2);
  assertEquals(g.edges.grounded.length, 1);
});

Deno.test("a fabricated citation (unknown symbol) fails the gate", () => {
  const g = checkGrounding("frobnicate [src=src/io.rs loc=L9]", facts);
  assert(!g.ok);
  assertEquals(g.citations.unknownLabel[0].label, "frobnicate");
  assert(g.reasons.some((r) => r.includes("fabricated citation")));
});

Deno.test("a fabricated edge (no connection) fails the gate", () => {
  const g = checkGrounding("read --calls--> plan", facts);
  assert(!g.ok);
  assertEquals(g.edges.unknown[0], { a: "read", rel: "calls", b: "plan" });
  assert(g.reasons.some((r) => r.includes("fabricated edge")));
});

Deno.test("wrong relation/direction between connected nodes is flagged but not fatal", () => {
  const g = checkGrounding("parse --imports--> plan", facts); // parse→plan is `calls`
  assert(g.ok, "connected pair → not a hard fabrication");
  assertEquals(g.edges.mismatched.length, 1);
  assert(g.reasons.some((r) => r.includes("relation/direction off")));
});

Deno.test("citation to a real symbol at the wrong file is flagged (soft)", () => {
  const g = checkGrounding("plan [src=src/io.rs loc=L4]", facts); // plan is in core.rs
  assert(g.ok, "symbol exists → not a hard fabrication");
  assertEquals(g.citations.fileMismatch[0].label, "plan");
});

Deno.test("a basename src= still grounds (path alias)", () => {
  const g = checkGrounding("parse [src=io.rs loc=L1]", facts); // graph has src/io.rs
  assert(g.ok, g.reasons.join("; "));
  assertEquals(g.citations.grounded.length, 1);
});

// ---- unresolved edges (ADR-0029/0030) ---------------------------------------

Deno.test("an unresolved (opaque) hop marked in evidence grounds, not fabricates", () => {
  // parse has a real unresolved call to `dynres` (opaque) — not a resolved edge, so
  // the resolved-only checker would call it fabricated. With the unresolved index it
  // grounds; a genuinely invented edge still fails.
  const f = factsFromGraph(GRAPH, [], [
    { sourceLabel: "parse", relation: "calls", name: "dynres" },
  ]);
  const ok = checkGrounding("parse --calls [UNRESOLVED]--> dynres", f);
  assert(ok.ok, `unresolved hop must ground: ${ok.reasons.join("; ")}`);
  assertEquals(ok.edges.grounded.length, 1);
  assert(!checkGrounding("parse --calls--> nowhere", f).ok, "an invented edge still fails");
});

// ---- file/project/owner aliases (fact index) --------------------------------

Deno.test("a project name grounds (citable, not a graph node)", () => {
  const f = factsFromGraph(GRAPH, ["usls", "smart-camera-service"]);
  assert(checkGrounding("usls [src=apps/x loc=L1]", f).ok, "project ref grounds");
});

Deno.test("a project cited with a dropped @scope prefix grounds (real-run bug)", () => {
  // The model cites `edgez/hominid-frontend`; the real project name is
  // `@edgez/hominid-frontend`. Same package, dropped scope — not a fabrication. Match
  // by basename, like files. A genuinely unknown project still fails.
  const f = factsFromGraph(GRAPH, ["@edgez/hominid-frontend", "apps/hominid-frontend"]);
  assert(checkGrounding("edgez/hominid-frontend [src=apps/hominid-frontend loc=L1]", f).ok);
  assert(!checkGrounding("edgez/ghost-app [src=apps/x loc=L1]", f).ok, "unknown project still fails");
});

Deno.test("a file node cited by basename grounds; an edge with a basename endpoint grounds", () => {
  const withFile = factsFromGraph({
    nodes: [
      { id: "file:apps/svc/src/main.rs", label: "apps/svc/src/main.rs", source_file: "apps/svc/src/main.rs" },
      { id: "type:apps/svc/src/main.rs:AiAgent", label: "AiAgent", source_file: "apps/svc/src/main.rs" },
    ],
    edges: [{ source: "file:apps/svc/src/main.rs", target: "type:apps/svc/src/main.rs:AiAgent", relation: "contains" }],
  });
  assert(checkGrounding("main.rs [src=apps/svc/src/main.rs loc=L1]", withFile).ok);
  assert(checkGrounding("main.rs --contains--> AiAgent", withFile).ok);
});

// ADR-0028: owner-qualified names + the `[id=…]` handle.
const OWNED = {
  nodes: [
    { id: "fn:engine.rs:Engine::from_config", label: "from_config", source_file: "src/engine.rs", impl: "Engine" },
    { id: "fn:proc.rs:ImageProcessor::from_config", label: "from_config", source_file: "src/proc.rs", impl: "ImageProcessor" },
  ],
  edges: [] as Array<{ source: string; target: string; relation: string }>,
};
const owned = factsFromGraph(OWNED);

Deno.test("an owner-qualified citation grounds (Engine::from_config)", () => {
  assert(checkGrounding("Engine::from_config [src=src/engine.rs loc=L98]", owned).ok);
  assert(checkGrounding("from_config [src=src/proc.rs loc=L60]", owned).ok);
});

Deno.test("an owner-qualified EDGE endpoint grounds like a citation (real-run bug)", () => {
  // The model disambiguates a homonym source with its owner — `Engine::from_config
  // --calls--> from_config_with_session` — which is exactly right. Edge grounding must
  // alias `Owner::label`→`label` the same way citations do, or a correct answer fails.
  const g = factsFromGraph({
    nodes: [
      { id: "fn:e.rs:Engine::from_config", label: "from_config", source_file: "e.rs", impl: "Engine" },
      { id: "fn:e.rs:Engine::from_config_with_session", label: "from_config_with_session", source_file: "e.rs", impl: "Engine" },
    ],
    edges: [{ source: "fn:e.rs:Engine::from_config", target: "fn:e.rs:Engine::from_config_with_session", relation: "calls" }],
  });
  assert(checkGrounding("Engine::from_config --calls--> from_config_with_session", g).ok);
  // and both endpoints owner-qualified
  assert(checkGrounding("Engine::from_config --calls--> Engine::from_config_with_session", g).ok);
});

Deno.test("a citation whose label is a full node id grounds by id (real-run bug)", () => {
  // The model wrote `fn:…/cpu.rs:CpuTransformExecutor::execute_plan [src=… loc=…]` — it
  // put the whole node id (slashes and all) in the label slot instead of `id=`. The
  // label class allows `/` so the id is captured whole, and it's a real node → ground.
  const g = factsFromGraph({
    nodes: [{ id: "fn:libs/x/cpu.rs:Cpu::execute_plan", label: "execute_plan", source_file: "libs/x/cpu.rs", impl: "Cpu" }],
    edges: [],
  });
  assert(checkGrounding("fn:libs/x/cpu.rs:Cpu::execute_plan [src=libs/x/cpu.rs loc=L1]", g).ok);
});

Deno.test("a module/qualifier-prefixed citation of a real node grounds via the trailing label", () => {
  // The model over-qualifies a real type with a module path (`events::SystemEvent`) or
  // an enum (`Postprocess::Detection`); the trailing label IS a real node, so it grounds
  // (file-checked). A qualified *fake* trailing name is still a fabrication.
  const g = factsFromGraph({
    nodes: [{ id: "type:events.rs:SystemEvent", label: "SystemEvent", source_file: "src/events.rs" }],
    edges: [],
  });
  assert(checkGrounding("events::SystemEvent [src=src/events.rs loc=L1]", g).ok);
  assert(!checkGrounding("events::GhostType [src=src/events.rs loc=L1]", g).ok);
});

Deno.test("an [id=…] handle grounds by the id, and a fake id is flagged", () => {
  assert(checkGrounding("from_config [id=fn:engine.rs:Engine::from_config]", owned).ok);
  assert(!checkGrounding("bogus [id=fn:nope.rs:Ghost::x]", owned).ok);
});

Deno.test("copying a full node handle does not spawn a phantom citation", () => {
  const g = checkGrounding(
    "from_config [src=src/engine.rs loc=L98-L100 id=fn:engine.rs:Engine::from_config]",
    owned,
  );
  assert(g.ok, g.reasons.join("; "));
  assertEquals(g.citations.unknownLabel.length, 0);
});
