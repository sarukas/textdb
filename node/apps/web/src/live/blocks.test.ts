import MarkdownIt from "markdown-it";
import { describe, expect, it } from "vitest";
import { blockAtLine, blocksTouching, renderBlocks } from "./blocks";
import { changedLineSpans, hunksToChangeSet } from "./changeset";
import { diffLines } from "./diff";
import { textOf } from "./tracker";

const md = new MarkdownIt({ html: false, linkify: true });

const doc = [
  "# Heading", // 0
  "", // 1
  "First paragraph", // 2
  "continues here.", // 3
  "", // 4
  "- one", // 5
  "- two", // 6
  "  - nested", // 7
  "", // 8
  "```js", // 9
  "code()", // 10
  "```", // 11
  "", // 12
  "> quote", // 13
  "", // 14
  "Last paragraph.", // 15
  "",
].join("\n");

describe("renderBlocks", () => {
  it("splits the document into top-level blocks with their source lines", () => {
    const blocks = renderBlocks(md, doc);
    expect(blocks.map((b) => [b.start, b.end])).toEqual([
      [0, 1],
      [2, 4],
      [5, 9], // markdown-it counts a list's trailing blank line as part of it
      [9, 12],
      [13, 14],
      [15, 16],
    ]);
    expect(blocks[0]!.html).toBe("<h1>Heading</h1>\n");
    expect(blocks[2]!.html).toContain("<li>two\n<ul>");
    // The blocks together render what the whole document renders.
    expect(blocks.map((b) => b.html).join("")).toBe(md.render(doc));
  });

  it("keeps keys of unchanged blocks when content shifts", () => {
    const before = renderBlocks(md, doc);
    const after = renderBlocks(md, "Intro line.\n\n" + doc.replace("- two", "- TWO"));
    const beforeKeys = new Set(before.map((b) => b.key));
    const kept = after.filter((b) => beforeKeys.has(b.key)).map((b) => b.html);
    expect(kept).toHaveLength(before.length - 1);
    expect(after.find((b) => b.html.includes("TWO"))!.key).not.toBe(before[2]!.key);
  });

  it("keeps YAML front matter as one literal block with its own lines", () => {
    const src = "---\ntitle: Route\nowner: carol\n---\n\n# Route\n\nBody.\n";
    const blocks = renderBlocks(md, src);
    expect(blocks.map((b) => [b.start, b.end])).toEqual([
      [0, 4],
      [5, 6],
      [7, 8],
    ]);
    expect(blocks[0]!.html).toBe('<pre class="front-matter"><code>---\ntitle: Route\nowner: carol\n---</code></pre>\n');
    expect(blocks[1]!.html).toBe("<h1>Route</h1>\n");
    // A change on line 3 (owner) touches only the front matter block.
    expect(blocksTouching(blocks, [{ from: 3, to: 4 }])).toEqual([0]);
  });

  it("gives identical blocks distinct keys", () => {
    const blocks = renderBlocks(md, "same\n\nsame\n");
    expect(blocks[0]!.key).not.toBe(blocks[1]!.key);
  });
});

describe("blocksTouching", () => {
  it("flags exactly the blocks that touch changed lines", () => {
    const next = doc.replace("continues here.", "continues, edited.").replace("code()", "code()\nmore()");
    const hunks = diffLines(doc, next);
    const cs = hunksToChangeSet(textOf(doc), hunks);
    const after = cs.apply(textOf(doc));
    const blocks = renderBlocks(md, next);
    const touched = blocksTouching(blocks, changedLineSpans(cs, after));
    expect(touched.map((i) => blocks[i]!.html.slice(0, 12))).toEqual(["<p>First par", "<pre><code c"]);
  });

  it("flags nothing for a change in blank lines between blocks", () => {
    const blocks = renderBlocks(md, doc);
    expect(blocksTouching(blocks, [{ from: 5, to: 6 }])).toEqual([]);
  });

  it("flags the neighbour of a deletion", () => {
    const blocks = renderBlocks(md, doc);
    // A deletion in front of line 3 (0-based 2) touches the paragraph that now starts there.
    expect(blocksTouching(blocks, [{ from: 3, to: 3 }])).toEqual([1]);
    // A deletion in front of line 5 (0-based 4, the blank after the paragraph) touches the paragraph end.
    expect(blocksTouching(blocks, [{ from: 5, to: 5 }])).toEqual([1]);
  });

  it("flags a spanning change once per block", () => {
    const blocks = renderBlocks(md, doc);
    expect(blocksTouching(blocks, [{ from: 4, to: 11 }])).toEqual([1, 2, 3]);
  });

  it("finds the block at a line", () => {
    const blocks = renderBlocks(md, doc);
    expect(blockAtLine(blocks, 1)).toBe(0);
    expect(blockAtLine(blocks, 4)).toBe(1);
    expect(blockAtLine(blocks, 5)).toBe(1);
    expect(blockAtLine(blocks, 16)).toBe(5);
  });
});
