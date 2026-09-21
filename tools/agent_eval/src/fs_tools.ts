//! `ls` and `grep` — plain filesystem tools handed to the agent next to `read_file`
//! and the MCP graph tools. They exist to *remove a confound*, not to help: with only
//! `read_file` available, "the model used the graph" was forced rather than chosen, so
//! every score carried that bias. With a directory listing and a content search on the
//! table, which tool the model reaches for first becomes an observable — and a failure
//! that happened *while* `ls`/`grep` were available says something much stronger.
//!
//! Same boundary rules as `read_file` (ADR-0013: validate at the edge): path-jailed to
//! the repo root, output capped, and — the part this project keeps re-learning — a cut
//! is always *announced*. A silently truncated listing reads as an absent file.

import { tool } from "ai";
import { z } from "zod";
import { isAbsolute, relative, resolve } from "node:path";

/** Max entries one `ls` returns. */
export const LS_CAP = 200;
/** Hard stop on the `ls` walk itself, so a deep `depth` can't enumerate the world. */
export const LS_SCAN_MAX = 5000;
/** Max matches one `grep` returns (it keeps counting past this to report the total). */
export const GREP_CAP = 100;
/** Matched lines are trimmed and cut to this width — a minified bundle is one line. */
export const GREP_LINE_CHARS = 200;
/** Files larger than this are skipped: source isn't, logs and blobs are. */
export const GREP_MAX_FILE_BYTES = 256 * 1024;
/** Scan budget for one `grep`: stop after this many *read* files… */
export const GREP_MAX_FILES = 20_000;
/** …or this many bytes actually read, whichever comes first. */
export const GREP_MAX_BYTES = 128 * 1024 * 1024;

/** Directories neither tool descends into. Generated or vendored trees: they are not
 *  in the graph either (ADR-0022 ignores them), so descending would compare the file
 *  view against a graph view of different content. `ls` still *lists* them — nothing
 *  is hidden, it just doesn't recurse. */
export const SKIP_DIRS = new Set([
  ".git",
  "node_modules",
  "target",
  "dist",
  "build",
  ".filigrio-out",
  ".next",
  ".svelte-kit",
  ".turbo",
  ".venv",
  "__pycache__",
]);

/** Generated trees a *name* can't identify, matched on the path relative to the repo
 *  root. `.moon/` holds real workspace config; only its `cache/` is a build cache —
 *  and on this corpus that cache holds minified bundles that otherwise won 1 222 of
 *  the 1 322 hits for `grep import`. A real `rg` reads `.gitignore` and never sees
 *  them; a grep that does would be a worse tool than the one it stands in for, which
 *  would tilt the very comparison this exists to measure. */
export const SKIP_PATHS = new Set([".moon/cache"]);

function skipDir(name: string, repoRelPath: string): boolean {
  return SKIP_DIRS.has(name) || SKIP_PATHS.has(repoRelPath);
}

/** Repo-root jail, shared by both tools. Returns the absolute path, or an `ERROR:`
 *  string when the argument points outside the root (`..`, or an absolute path
 *  elsewhere). The repo root itself is allowed — unlike `read_file`, listing and
 *  searching the root is the normal case. */
export function jail(root: string, path: string): { abs: string } | { error: string } {
  const abs = resolve(root, path);
  const rel = relative(root, abs);
  if (rel.startsWith("..") || isAbsolute(rel)) {
    return { error: `ERROR: path '${path}' escapes the repository root` };
  }
  return { abs };
}

/** The jail above is lexical, so a *symlink* inside the repo that points out of it
 *  would still be followed by `stat`/`open`. Resolve the real path and re-check. The
 *  root is resolved too: if the checkout itself sits behind a symlink, comparing a
 *  resolved target against an unresolved root would reject everything. Missing paths
 *  fall through — the caller's `stat` reports them. Directory walks never hit this:
 *  `readDir` doesn't follow symlinks and neither walker descends into one. */
export function escapesViaSymlink(root: string, abs: string): boolean {
  let realRoot = root, realAbs = abs;
  try {
    realRoot = Deno.realPathSync(root);
    realAbs = Deno.realPathSync(abs);
  } catch {
    return false;
  }
  const rel = relative(realRoot, realAbs);
  return rel.startsWith("..") || isAbsolute(rel);
}

function joinRel(base: string, name: string): string {
  return base ? `${base}/${name}` : name;
}

export interface Listing {
  entries: string[];
  total: number;
  scanCapped: boolean;
}

/** Breadth-first listing of `dir` (absolute), to `depth` levels, paths relative to
 *  `dir`. Breadth-first on purpose: when the cap bites, the shallow entries — the ones
 *  that orient you — are the ones that survive. Directories end in `/`; symlinks are
 *  listed but never followed (cycles). `base` is `dir` relative to the repo root, and
 *  is only used to test the path-form skips. */
export function listDir(dir: string, depth: number, base = ""): Listing {
  const entries: string[] = [];
  let total = 0;
  let scanCapped = false;
  let level: string[] = [""];
  for (let d = 1; d <= depth && level.length > 0 && !scanCapped; d++) {
    const next: string[] = [];
    for (const rel of level) {
      let kids: Deno.DirEntry[];
      try {
        kids = [...Deno.readDirSync(resolve(dir, rel))];
      } catch {
        continue; // unreadable directory: skip, don't fail the whole listing
      }
      kids.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
      for (const k of kids) {
        const path = joinRel(rel, k.name);
        total++;
        if (entries.length < LS_CAP) entries.push(k.isDirectory ? `${path}/` : path);
        if (total >= LS_SCAN_MAX) {
          scanCapped = true;
          break;
        }
        if (k.isDirectory && !k.isSymlink && !skipDir(k.name, joinRel(base, path))) next.push(path);
      }
      if (scanCapped) break;
    }
    level = next;
  }
  return { entries, total, scanCapped };
}

function renderListing(l: Listing): string {
  if (l.total === 0) return "(empty directory)";
  const shown = l.entries.join("\n");
  if (l.entries.length >= l.total && !l.scanCapped) return shown;
  const of = l.scanCapped ? `${l.total}+` : String(l.total);
  return `${shown}\n… TRUNCATED: showing ${l.entries.length} of ${of} entries — ` +
    `narrow \`path\` or lower \`depth\`.`;
}

export function lsTool(repoRoot: string) {
  const root = resolve(repoRoot);
  return tool({
    description:
      "List the files and directories at a repo-relative path (omit `path` for the " +
      "repository root). Directories end with `/`. `depth` 1 (default) lists just that " +
      "directory; 2 or 3 also list that many levels below it. Does not descend into " +
      "generated trees (.git, node_modules, target, dist, build, caches). Output is " +
      "capped and says so when cut.",
    inputSchema: z.object({
      path: z.string().optional().describe(
        "Repo-relative directory, e.g. apps/api/src. Omit for the repository root.",
      ),
      depth: z.literal([1, 2, 3]).optional().describe(
        "How many levels to list. 1 = this directory only (default).",
      ),
    }),
    execute: async ({ path, depth }: { path?: string; depth?: number }): Promise<string> => {
      const j = jail(root, path ?? "");
      if ("error" in j) return j.error;
      if (escapesViaSymlink(root, j.abs)) {
        return `ERROR: path '${path}' escapes the repository root (symlink)`;
      }
      try {
        const st = await Deno.stat(j.abs);
        if (!st.isDirectory) return `ERROR: '${path}' is a file, not a directory — use read_file`;
      } catch (e) {
        return `ERROR: ${e instanceof Error ? e.message : String(e)}`;
      }
      return renderListing(listDir(j.abs, depth ?? 1, relative(root, j.abs)));
    },
  });
}

export interface GrepResult {
  lines: string[];
  total: number;
  scanCapped: boolean;
}

/** Extensions never worth opening. This is a fast path, not the policy: anything not
 *  listed here still gets the NUL sniff below, so an unknown binary is still skipped —
 *  it just costs an open. It earns its keep on this corpus, where one service's data
 *  directory holds 22 462 JPEGs: sniffing them took 7.6 s of every whole-repo grep. */
const BINARY_EXTS = new Set([
  "jpg", "jpeg", "png", "gif", "webp", "bmp", "ico", "icns", "tiff", "psd",
  "mp3", "wav", "flac", "ogg", "mp4", "mov", "avi", "mkv", "webm",
  "zip", "gz", "tgz", "bz2", "xz", "zst", "tar", "7z", "rar", "pdf",
  "ttf", "otf", "woff", "woff2", "eot",
  "so", "dylib", "dll", "exe", "bin", "o", "a", "rlib", "rmeta", "wasm", "class", "jar", "pyc",
  "onnx", "engine", "pt", "pth", "safetensors", "npy", "npz", "pack", "idx",
  "db", "sqlite", "sqlite3", "parquet", "glb", "gltf", "blend", "fbx", "obj", "apk", "aab",
]);

function isBinaryName(name: string): boolean {
  const dot = name.lastIndexOf(".");
  return dot > 0 && BINARY_EXTS.has(name.slice(dot + 1).toLowerCase());
}

/** True when the first bytes hold a NUL — the cheap, extension-free binary test. Blobs
 *  are rejected after a 4 KiB sniff instead of a full read, which is what keeps a
 *  whole-repo grep affordable next to a directory of images. */
function looksBinary(head: Uint8Array): boolean {
  return head.includes(0);
}

async function* walkFiles(dir: string, rel: string): AsyncGenerator<string> {
  let kids: Deno.DirEntry[];
  try {
    kids = [...Deno.readDirSync(dir)];
  } catch {
    return;
  }
  kids.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  for (const k of kids) {
    const path = joinRel(rel, k.name);
    if (k.isDirectory) {
      if (k.isSymlink || skipDir(k.name, path)) continue;
      yield* walkFiles(resolve(dir, k.name), path);
    } else if (k.isFile && !isBinaryName(k.name)) {
      yield path;
    }
  }
}

/** Read at most `GREP_MAX_FILE_BYTES` of a file, returning null when it is too big,
 *  binary, or unreadable. */
async function readTextCapped(abs: string): Promise<string | null> {
  let file: Deno.FsFile;
  let size: number;
  try {
    const st = await Deno.stat(abs);
    if (!st.isFile || st.size > GREP_MAX_FILE_BYTES) return null;
    if (st.size === 0) return "";
    size = st.size;
    file = await Deno.open(abs, { read: true });
  } catch {
    return null;
  }
  try {
    const buf = new Uint8Array(Math.min(size, GREP_MAX_FILE_BYTES));
    // Sniff first, read the rest only if it's text: a directory of images must cost
    // 4 KiB each, not their full size, or a whole-repo grep is unaffordable here.
    const sniffEnd = Math.min(buf.length, 4096);
    let n = 0;
    while (n < sniffEnd) {
      const got = await file.read(buf.subarray(n, sniffEnd));
      if (got === null) break;
      n += got;
    }
    if (looksBinary(buf.subarray(0, n))) return null;
    while (n < buf.length) {
      const got = await file.read(buf.subarray(n));
      if (got === null) break;
      n += got;
    }
    return new TextDecoder().decode(buf.subarray(0, n));
  } catch {
    return null;
  } finally {
    file.close();
  }
}

/** Fixed-string search under `abs` (a file or a directory), reporting `file:line: text`
 *  paths relative to `root`. Fixed-string, not regex, on purpose: a regex is a grammar
 *  the model has to get right before it learns anything, and `fn get_conn(` is a
 *  perfectly good literal. Scanning continues past `GREP_CAP` so the reported total is
 *  the real one — only the *listing* is capped. */
export async function grepUnder(root: string, abs: string, pattern: string): Promise<GrepResult> {
  const lines: string[] = [];
  let total = 0;
  let files = 0;
  let bytes = 0;
  let scanCapped = false;

  const scan = (rel: string, text: string) => {
    let no = 0;
    for (const line of text.split("\n")) {
      no++;
      if (!line.includes(pattern)) continue;
      total++;
      if (lines.length < GREP_CAP) {
        const t = line.trim();
        const cut = t.length > GREP_LINE_CHARS ? `${t.slice(0, GREP_LINE_CHARS)}…` : t;
        lines.push(`${rel}:${no}: ${cut}`);
      }
    }
  };

  const st = await Deno.stat(abs);
  if (st.isFile) {
    const text = await readTextCapped(abs);
    if (text !== null) scan(relative(root, abs) || abs, text);
    return { lines, total, scanCapped };
  }

  for await (const rel of walkFiles(abs, relative(root, abs))) {
    if (files >= GREP_MAX_FILES || bytes >= GREP_MAX_BYTES) {
      scanCapped = true;
      break;
    }
    const text = await readTextCapped(resolve(root, rel));
    if (text === null) continue; // too big, binary, or unreadable — costs no budget
    files++;
    bytes += text.length;
    scan(rel, text);
  }
  return { lines, total, scanCapped };
}

function renderGrep(g: GrepResult, pattern: string): string {
  if (g.total === 0) {
    return g.scanCapped
      ? `no match for '${pattern}'\n… TRUNCATED: the scan hit its file/byte budget ` +
        `before finishing — narrow \`path\`.`
      : `no match for '${pattern}'`;
  }
  const shown = g.lines.join("\n");
  const notes: string[] = [];
  if (g.lines.length < g.total) {
    notes.push(
      `… TRUNCATED: showing ${g.lines.length} of ${g.total} matches — narrow \`path\` ` +
        `or use a longer pattern.`,
    );
  }
  if (g.scanCapped) {
    notes.push(`… TRUNCATED: the scan hit its file/byte budget before finishing.`);
  }
  return [shown, ...notes].join("\n");
}

export function grepTool(repoRoot: string) {
  const root = resolve(repoRoot);
  return tool({
    description:
      "Search file contents for a plain text string (not a regular expression; " +
      "case-sensitive). Returns one line per match as `file:line: matched line`. Use " +
      "`path` to search one repo-relative directory or file; omit it to search the whole " +
      "repository. Skips generated trees (.git, node_modules, target, dist, build, " +
      "caches) and binary files. Output is capped and says so when cut.",
    inputSchema: z.object({
      pattern: z.string().min(1).describe("Exact text to find, e.g. fn get_conn"),
      path: z.string().optional().describe(
        "Repo-relative directory or file to search, e.g. libs/rust. Omit for the whole repository.",
      ),
    }),
    execute: async ({ pattern, path }: { pattern: string; path?: string }): Promise<string> => {
      const j = jail(root, path ?? "");
      if ("error" in j) return j.error;
      if (escapesViaSymlink(root, j.abs)) {
        return `ERROR: path '${path}' escapes the repository root (symlink)`;
      }
      try {
        await Deno.stat(j.abs);
      } catch (e) {
        return `ERROR: ${e instanceof Error ? e.message : String(e)}`;
      }
      try {
        return renderGrep(await grepUnder(root, j.abs, pattern), pattern);
      } catch (e) {
        return `ERROR: ${e instanceof Error ? e.message : String(e)}`;
      }
    },
  });
}
