//! Connect to the `filigrio-mcp` bridge binary as a **standard MCP client** over
//! stdio (ADR-0025). The bridge is its own binary — the CLI has no `mcp` verb
//! (it carried a `mcp serve` signpost that only ever errored; dropped 2026-07-28).
//! This is the whole point of the conformance work: no filigrio-specific glue — the
//! Vercel AI SDK's generic MCP client speaks to our server because the server now
//! emits real `inputSchema` and a real `protocolVersion`.

import { createMCPClient } from "@ai-sdk/mcp";
import { Experimental_StdioMCPTransport } from "@ai-sdk/mcp/mcp-stdio";

export type FiligrioClient = Awaited<ReturnType<typeof createMCPClient>>;

/** Spawn `filigrio-mcp --socket <socket>` and return a connected MCP client.
 *  The caller MUST `await client.close()` when done (it kills the child process).
 *  `projectDir` becomes the child process's cwd, so the bridge's cwd→project
 *  resolution (ADR-0032f §6) picks the right project without every tool call
 *  needing an explicit `project` argument. */
export function connectFiligrio(binPath: string, socketPath: string, projectDir: string): Promise<FiligrioClient> {
  const transport = new Experimental_StdioMCPTransport({
    command: binPath,
    args: ["--socket", socketPath],
    cwd: projectDir,
  });
  return createMCPClient({ transport });
}
