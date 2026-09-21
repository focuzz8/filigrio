//! A local `read_file` tool given to the agent alongside the MCP graph tools. Two
//! reasons it exists: (1) an agent needs to confirm graph facts against source; (2)
//! *when* it reaches for source vs trusts the graph is itself an ergonomics signal.
//! Path-jailed to the repo root and byte-capped (ADR-0013: validate at the boundary).

import { tool } from "ai";
import { z } from "zod";
import { isAbsolute, relative, resolve } from "node:path";
import { cap } from "./logger.ts";

export const FILE_CAP = 8000;

/// Slice `text` to the 1-based inclusive line range [from, to], prefixing each
/// kept line with its number so the agent can map back to a graph node's `loc`
/// (e.g. `L29-L41`). Out-of-range bounds clamp; `to` past EOF just stops at EOF.
export function sliceLines(text: string, from: number, to: number): string {
  const lines = text.split("\n");
  const start = Math.max(1, Math.floor(from));
  const end = Math.min(lines.length, Math.floor(to));
  if (end < start) return `ERROR: empty line range L${from}-L${to}`;
  const width = String(end).length;
  return lines
    .slice(start - 1, end)
    .map((l, i) => `${String(start + i).padStart(width)}  ${l}`)
    .join("\n");
}

export function readFileTool(repoRoot: string) {
  const root = resolve(repoRoot);
  return tool({
    description:
      "Read a UTF-8 source file from the repository by repo-relative path " +
      "(e.g. the `src=` shown on a graph node). Use to confirm what the graph reports. " +
      "Pass `fromLine`/`toLine` (1-based, inclusive) to read just one function's body — " +
      "e.g. a node at `loc=L29-L41` → fromLine:29, toLine:41; ranged output is line-numbered.",
    inputSchema: z.object({
      path: z.string().describe("Repo-relative file path, e.g. apps/api/src/main.rs"),
      fromLine: z.number().int().positive().optional().describe(
        "First line to read (1-based, inclusive). Omit to read from the top.",
      ),
      toLine: z.number().int().positive().optional().describe(
        "Last line to read (1-based, inclusive). Omit to read to EOF.",
      ),
    }),
    execute: (
      { path, fromLine, toLine }: { path: string; fromLine?: number; toLine?: number },
    ): Promise<string> => {
      const abs = resolve(root, path);
      const rel = relative(root, abs);
      if (rel === "" || rel.startsWith("..") || isAbsolute(rel)) {
        return Promise.resolve(`ERROR: path '${path}' escapes the repository root`);
      }
      const ranged = fromLine !== undefined || toLine !== undefined;
      return Deno.readTextFile(abs)
        .then((data) =>
          ranged
            ? cap(sliceLines(data, fromLine ?? 1, toLine ?? Infinity), FILE_CAP)
            : cap(data, FILE_CAP)
        )
        .catch((e) => `ERROR: ${e instanceof Error ? e.message : String(e)}`);
    },
  });
}
