import { assert, assertEquals, assertStringIncludes } from "@std/assert";
import { GREP_CAP, GREP_LINE_CHARS, grepTool, jail, listDir, LS_CAP, lsTool } from "./fs_tools.ts";

// deno-lint-ignore no-explicit-any
const opts = { toolCallId: "t", messages: [] } as any;
// deno-lint-ignore no-explicit-any
const run = (t: unknown, args: Record<string, unknown>): Promise<string> => (t as any).execute(args, opts);

async function fixture(): Promise<string> {
  const root = await Deno.makeTempDir();
  await Deno.mkdir(`${root}/src/inner`, { recursive: true });
  await Deno.mkdir(`${root}/node_modules/pkg`, { recursive: true });
  await Deno.writeTextFile(`${root}/Cargo.toml`, "[package]\n");
  await Deno.writeTextFile(`${root}/src/main.rs`, "fn main() {\n    get_conn();\n}\n");
  await Deno.writeTextFile(`${root}/src/inner/db.rs`, "fn get_conn() -> Conn {\n    todo!()\n}\n");
  await Deno.writeTextFile(`${root}/node_modules/pkg/index.js`, "get_conn\n");
  return root;
}

// ── ls ──────────────────────────────────────────────────────────────────────

Deno.test("ls lists one directory, marking directories with a trailing slash", async () => {
  const root = await fixture();
  const out = await run(lsTool(root), {});
  assertEquals(out.split("\n").sort(), ["Cargo.toml", "node_modules/", "src/"]);
  await Deno.remove(root, { recursive: true });
});

Deno.test("ls depth reaches deeper levels; depth 1 stops at the directory", async () => {
  const root = await fixture();
  const shallow = await run(lsTool(root), { path: "src" });
  assertEquals(shallow.split("\n").sort(), ["inner/", "main.rs"]);
  const deep = await run(lsTool(root), { path: "src", depth: 2 });
  assertStringIncludes(deep, "inner/db.rs");
  await Deno.remove(root, { recursive: true });
});

Deno.test("ls lists a skipped directory but does not descend into it", async () => {
  const root = await fixture();
  const out = await run(lsTool(root), { depth: 3 });
  assertStringIncludes(out, "node_modules/");
  assert(!out.includes("node_modules/pkg"), `must not descend into node_modules:\n${out}`);
  await Deno.remove(root, { recursive: true });
});

Deno.test("ls rejects paths that escape the repo root", async () => {
  const root = await Deno.makeTempDir();
  const t = lsTool(root);
  for (const bad of ["..", "../secret", "../../etc", "/etc"]) {
    const out = await run(t, { path: bad });
    assert(out.startsWith("ERROR: path"), `expected jail rejection for ${bad}, got: ${out}`);
  }
  await Deno.remove(root, { recursive: true });
});

Deno.test("ls caps its output and says so", async () => {
  const root = await Deno.makeTempDir();
  const n = LS_CAP + 37;
  for (let i = 0; i < n; i++) {
    await Deno.writeTextFile(`${root}/f${String(i).padStart(4, "0")}.txt`, "x");
  }
  const out = await run(lsTool(root), {});
  const lines = out.split("\n");
  assertEquals(lines.length, LS_CAP + 1, "capped entries plus one truncation note");
  assertStringIncludes(out, `TRUNCATED: showing ${LS_CAP} of ${n} entries`);
  await Deno.remove(root, { recursive: true });
});

Deno.test("ls is breadth-first, so the shallow entries survive truncation", async () => {
  const root = await Deno.makeTempDir();
  // A first-alphabetically directory big enough to eat the whole cap on its own.
  await Deno.mkdir(`${root}/aaa`);
  for (let i = 0; i < LS_CAP + 50; i++) {
    await Deno.writeTextFile(`${root}/aaa/f${String(i).padStart(4, "0")}.txt`, "x");
  }
  await Deno.writeTextFile(`${root}/zzz.rs`, "fn z() {}");
  const out = await run(lsTool(root), { depth: 2 });
  assertStringIncludes(out, "zzz.rs"); // depth-1 sibling not swallowed by aaa/
  assertStringIncludes(out, "TRUNCATED");
  await Deno.remove(root, { recursive: true });
});

Deno.test("ls on a file, a missing path, and an empty directory each explain themselves", async () => {
  const root = await fixture();
  await Deno.mkdir(`${root}/empty`);
  assertStringIncludes(await run(lsTool(root), { path: "Cargo.toml" }), "is a file, not a directory");
  assert((await run(lsTool(root), { path: "nope" })).startsWith("ERROR:"));
  assertEquals(await run(lsTool(root), { path: "empty" }), "(empty directory)");
  await Deno.remove(root, { recursive: true });
});

Deno.test("listDir counts every entry it walked, not just the ones it kept", async () => {
  const root = await fixture();
  const l = listDir(root, 2);
  assertEquals(l.entries.length, l.total);
  assertEquals(l.scanCapped, false);
  await Deno.remove(root, { recursive: true });
});

// ── grep ────────────────────────────────────────────────────────────────────

Deno.test("grep returns file:line: text for each match", async () => {
  const root = await fixture();
  const out = await run(grepTool(root), { pattern: "fn get_conn" });
  assertEquals(out, "src/inner/db.rs:1: fn get_conn() -> Conn {");
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep scopes to a path prefix, and accepts a single file", async () => {
  const root = await fixture();
  const t = grepTool(root);
  assertEquals(await run(t, { pattern: "get_conn", path: "src/inner" }), "src/inner/db.rs:1: fn get_conn() -> Conn {");
  assertEquals(await run(t, { pattern: "get_conn", path: "src/main.rs" }), "src/main.rs:2: get_conn();");
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep does not search skipped directories", async () => {
  const root = await fixture();
  const out = await run(grepTool(root), { pattern: "get_conn" });
  assert(!out.includes("node_modules"), `node_modules must not be searched:\n${out}`);
  await Deno.remove(root, { recursive: true });
});

Deno.test("a path-form skip hits only that path, not every directory of that name", async () => {
  const root = await fixture();
  await Deno.mkdir(`${root}/.moon/cache/cas`, { recursive: true });
  await Deno.mkdir(`${root}/src/cache`, { recursive: true }); // a real source dir
  await Deno.writeTextFile(`${root}/.moon/workspace.yml`, "projects: get_conn\n");
  await Deno.writeTextFile(`${root}/.moon/cache/cas/blob.txt`, "get_conn\n");
  await Deno.writeTextFile(`${root}/src/cache/lru.rs`, "fn get_conn() {}\n");

  const hits = await run(grepTool(root), { pattern: "get_conn" });
  assertStringIncludes(hits, "src/cache/lru.rs"); // a `cache` dir with source is searched
  assertStringIncludes(hits, ".moon/workspace.yml"); // `.moon` itself is searched
  assert(!hits.includes(".moon/cache"), `the build cache must be skipped:\n${hits}`);

  const listed = await run(lsTool(root), { depth: 3 });
  assertStringIncludes(listed, ".moon/cache/");
  assert(!listed.includes(".moon/cache/cas"), `must not descend into the build cache:\n${listed}`);
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep says plainly when nothing matched", async () => {
  const root = await fixture();
  assertEquals(await run(grepTool(root), { pattern: "zzz_absent_zzz" }), "no match for 'zzz_absent_zzz'");
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep caps its matches and reports the true total", async () => {
  const root = await Deno.makeTempDir();
  const n = GREP_CAP + 23;
  await Deno.writeTextFile(`${root}/big.rs`, Array.from({ length: n }, (_, i) => `let x${i} = hit();`).join("\n"));
  const out = await run(grepTool(root), { pattern: "hit()" });
  const lines = out.split("\n");
  assertEquals(lines.length, GREP_CAP + 1, "capped matches plus one truncation note");
  assertStringIncludes(out, `TRUNCATED: showing ${GREP_CAP} of ${n} matches`);
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep trims and cuts an over-long matched line", async () => {
  const root = await Deno.makeTempDir();
  await Deno.writeTextFile(`${root}/min.js`, `   needle${"x".repeat(400)}`);
  const out = await run(grepTool(root), { pattern: "needle" });
  assertEquals(out.length, "min.js:1: ".length + GREP_LINE_CHARS + 1, "cut to the line cap plus the ellipsis");
  assert(out.endsWith("…"), out.slice(-20));
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep skips binary content, by extension and by NUL sniff", async () => {
  const root = await Deno.makeTempDir();
  const blob = new TextEncoder().encode("needle here");
  await Deno.writeFile(`${root}/image.jpg`, blob); // skipped on name alone
  await Deno.writeFile(`${root}/blob.dat`, new Uint8Array([...blob, 0, 1, 2])); // skipped on sniff
  await Deno.writeTextFile(`${root}/ok.rs`, "needle here");
  assertEquals(await run(grepTool(root), { pattern: "needle" }), "ok.rs:1: needle here");
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep skips a file past the per-file byte cap", async () => {
  const root = await Deno.makeTempDir();
  await Deno.writeTextFile(`${root}/huge.log`, `${"y".repeat(300 * 1024)}\nneedle\n`);
  await Deno.writeTextFile(`${root}/small.log`, "needle\n");
  assertEquals(await run(grepTool(root), { pattern: "needle" }), "small.log:1: needle");
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep rejects paths that escape the repo root", async () => {
  const root = await Deno.makeTempDir();
  const t = grepTool(root);
  for (const bad of ["../secret", "../../etc", "/etc"]) {
    const out = await run(t, { pattern: "root:", path: bad });
    assert(out.startsWith("ERROR: path"), `expected jail rejection for ${bad}, got: ${out}`);
  }
  await Deno.remove(root, { recursive: true });
});

Deno.test("grep on a missing path returns an ERROR string, not a throw", async () => {
  const root = await Deno.makeTempDir();
  const out = await run(grepTool(root), { pattern: "x", path: "nope/" });
  assert(out.startsWith("ERROR:"), out);
  await Deno.remove(root, { recursive: true });
});

// ── the jail itself ─────────────────────────────────────────────────────────

Deno.test("a symlink out of the repo is rejected by both tools", async () => {
  const root = await Deno.makeTempDir();
  const outside = await Deno.makeTempDir();
  await Deno.writeTextFile(`${outside}/secret.txt`, "needle\n");
  await Deno.symlink(outside, `${root}/escape`);
  await Deno.symlink(`${outside}/secret.txt`, `${root}/secret-link.txt`);

  for (const bad of ["escape", "secret-link.txt"]) {
    const listed = await run(lsTool(root), { path: bad });
    assertStringIncludes(listed, "escapes the repository root");
    const grepped = await run(grepTool(root), { pattern: "needle", path: bad });
    assertStringIncludes(grepped, "escapes the repository root");
  }
  // …and the walk never follows one either.
  assertEquals(await run(grepTool(root), { pattern: "needle" }), "no match for 'needle'");
  await Deno.remove(root, { recursive: true });
  await Deno.remove(outside, { recursive: true });
});

Deno.test("a symlink that stays inside the repo still works", async () => {
  const root = await fixture();
  await Deno.symlink(`${root}/src`, `${root}/src-link`);
  assertStringIncludes(await run(lsTool(root), { path: "src-link" }), "main.rs");
  await Deno.remove(root, { recursive: true });
});

Deno.test("jail admits the root and everything under it, and nothing else", () => {
  const root = "/repo";
  assertEquals(jail(root, ""), { abs: "/repo" });
  assertEquals(jail(root, "src/a.rs"), { abs: "/repo/src/a.rs" });
  assertEquals(jail(root, "src/../src/a.rs"), { abs: "/repo/src/a.rs" });
  for (const bad of ["..", "../repo2", "/etc/passwd", "src/../../etc"]) {
    assert("error" in jail(root, bad), `expected rejection for ${bad}`);
  }
});
