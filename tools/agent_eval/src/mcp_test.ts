//! MCP-connection smoke test (ADR-0025) — the LLM-free ergonomics gate. It spawns
//! the real `filigrio-mcp --socket <socket>` through the Vercel AI SDK's *generic* MCP client and
//! asserts a standard client can list all 9 tools AND see `query_graph`'s `q`
//! parameter. This is the direct proof the conformance fixes (Phase 2) landed: a
//! stock client couldn't do this before `inputSchema`/`protocolVersion` were real.
//!
//! ADR-0032f update: Now uses the separate `filigrio-mcp` binary with daemon socket
//! communication instead of the old engine-linked `filigrio mcp serve --store`
//! (that CLI verb is gone — see `filigrio-classic` for the pre-0032f shape).
//!
//! ADR-0042 F9 update: the served list is **9 read-only tools** — the mutation
//! plane left the bridge. `register_project`/`index_project` are gone (the bridge
//! runs with the user's filesystem permissions, not the agent's, so every mutation
//! it exposed was a confused deputy; MCP roots, the agent-scoped containment, is
//! not built). This assertion is the end-to-end half of that invariant: the Rust
//! in-crate test `the_bridge_serves_only_read_only_tools` checks the same list
//! against the served value, this one checks what a real MCP client actually sees
//! over JSON-RPC. Skips cleanly when the debug binary isn't built (never fails off-box).

import { assert, assertEquals } from "@std/assert";
import { resolve } from "node:path";
import { connectFiligrio } from "./mcp.ts";

const HERE = import.meta.dirname!; // tools/agent_eval/src
const REPO = resolve(HERE, "../../.."); // repo root
const BIN = resolve(REPO, "src/target/debug/filigrio-mcp");

function binExists(): boolean {
  try {
    return Deno.statSync(BIN).isFile;
  } catch {
    return false;
  }
}

Deno.test({
  name: "a standard MCP client lists all 9 read-only tools with parameters",
  ignore: !binExists(),
  fn: async () => {
    // Use a temporary socket path for testing to avoid conflicts
    const tempDir = await Deno.makeTempDir({ prefix: "filigrio-test-" });
    const socketPath = resolve(tempDir, "filigrio-daemon.sock");

    // MCP server will connect to daemon via socket; projectDir also becomes
    // the bridge's cwd for its cwd→project resolution (ADR-0032f §6).
    const client = await connectFiligrio(BIN, socketPath, tempDir);
    try {
      const tools = await client.tools();
      const names = Object.keys(tools).sort();
      assertEquals(names, [
        "get_community",
        "get_neighbors",
        "get_node",
        "god_nodes",
        "graph_stats",
        "list_communities",
        "project_graph",
        "query_graph",
        "shortest_path",
      ]);

      // The crux: the model can see query_graph's `q` argument. Before the
      // conformance fix the schema was absent and this tool looked param-less.
      const schemaStr = JSON.stringify(
        // deno-lint-ignore no-explicit-any
        (tools["query_graph"] as any)?.inputSchema ?? tools["query_graph"],
      );
      assert(schemaStr.includes('"q"'), `query_graph must expose a q param: ${schemaStr}`);
    } finally {
      await client.close();
      await Deno.remove(tempDir, { recursive: true });
    }
  },
});
