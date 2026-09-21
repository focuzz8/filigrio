**Addressing.** Many symbols share a name (`from`, `new`, `build`). Every node line
ends with `(impl Owner) [src=… loc=… id=…]`. To target one, copy its `id=` verbatim
into `{id: "…"}` — do not construct an id and do not retry the bare label.

**Search.** `query_graph` matches literal 3-character runs of a label — no synonyms,
no stemming. An empty result means the wrong words, not absent code: retry a shorter
root ("authentication" → "auth") or a name you have already seen. Do not invent
symbol names.

**Relations.** Edge kinds are flat (`calls`, `imports`, `contains`, `implements`) or
hierarchical (`type/param`, `type/return`, `type/field`, `type/bound`). The
`relations` argument OR's the kinds you name, and a family prefix selects all its
members: `["type"]` matches every `type/…`, `["type/field"]` only that one. Two
values cover the whole graph: `["semantic"]` — code meaning only, the default — and
`["any"]` — every kind, including the `contains`/`imports` scaffolding. Leaving
`relations` out means `semantic`, so structure you were counting on can be missing
without saying so.

On a type or trait node the useful direction is `in`: `type/param` = what **takes**
it, `type/return` = what **produces** it, `type/field` = what **stores** it, and on a
trait, `type/bound` = what is **bounded** by it. A type inside a container still
counts — `fn f(x: Vec<Config>)` is `type/param` on `Config`.

**Unresolved edges.** Some calls cannot be bound to a node — a method on an opaque
receiver (builder chains, trait objects, external types). The graph keeps these
*honestly unresolved* rather than guessing, so `get_neighbors` ends with a line like
`unresolved: 70 by-name caller(s)`. **A symbol with 0 resolved but 70 unresolved
callers is heavily used, not dead.** Pass `include_unresolved: true` to list them,
then read the body to confirm a specific call before asserting it. A chain can cross
several unresolved hops: walk it hop-by-hop (do not loop on `shortest_path` — it
stops at the first unresolved hop) and say plainly which hops are unresolved. A
truthful partial answer beats a fabricated path.

**Confidence.** Every node and edge is tagged EXTRACTED (certain), INFERRED (likely),
or AMBIGUOUS (a guess). Lean on EXTRACTED, flag INFERRED/AMBIGUOUS, and report
numbers as the tools give them.

**Honesty.** Answer only from what the tools returned. If the graph lacks something,
say so — never invent a symbol, an edge, or a relationship.
