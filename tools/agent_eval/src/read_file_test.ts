import { assert, assertEquals } from "@std/assert";
import { readFileTool, sliceLines } from "./read_file.ts";

// deno-lint-ignore no-explicit-any
const opts = { toolCallId: "t", messages: [] } as any;

Deno.test("reads a file inside the jail", async () => {
  const root = await Deno.makeTempDir();
  await Deno.writeTextFile(`${root}/hello.txt`, "hi there");
  const t = readFileTool(root);
  // deno-lint-ignore no-explicit-any
  const out = await (t as any).execute({ path: "hello.txt" }, opts);
  assertEquals(out, "hi there");
  await Deno.remove(root, { recursive: true });
});

Deno.test("rejects paths that escape the repo root", async () => {
  const root = await Deno.makeTempDir();
  const t = readFileTool(root);
  for (const bad of ["../secret", "../../etc/passwd", "/etc/passwd"]) {
    // deno-lint-ignore no-explicit-any
    const out = await (t as any).execute({ path: bad }, opts);
    assert(out.startsWith("ERROR: path"), `expected jail rejection for ${bad}, got: ${out}`);
  }
  await Deno.remove(root, { recursive: true });
});

Deno.test("missing file returns an ERROR string, not a throw", async () => {
  const root = await Deno.makeTempDir();
  const t = readFileTool(root);
  // deno-lint-ignore no-explicit-any
  const out = await (t as any).execute({ path: "nope.txt" }, opts);
  assert(out.startsWith("ERROR:"), out);
  await Deno.remove(root, { recursive: true });
});

Deno.test("sliceLines returns the inclusive 1-based range, line-numbered", () => {
  const text = "a\nb\nc\nd\ne";
  assertEquals(sliceLines(text, 2, 4), "2  b\n3  c\n4  d");
});

Deno.test("sliceLines clamps out-of-range bounds to the file", () => {
  const text = "a\nb\nc";
  assertEquals(sliceLines(text, 0, 99), "1  a\n2  b\n3  c");
  assert(sliceLines(text, 3, 1).startsWith("ERROR:"), "reversed range is an error");
});

Deno.test("read_file with fromLine/toLine reads just that span", async () => {
  const root = await Deno.makeTempDir();
  await Deno.writeTextFile(`${root}/f.rs`, "fn a() {}\nfn make() {\n    body\n}\nfn z() {}");
  const t = readFileTool(root);
  // deno-lint-ignore no-explicit-any
  const out = await (t as any).execute({ path: "f.rs", fromLine: 2, toLine: 4 }, opts);
  assertEquals(out, "2  fn make() {\n3      body\n4  }");
  await Deno.remove(root, { recursive: true });
});
