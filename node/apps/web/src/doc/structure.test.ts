import { describe, expect, it } from "vitest";
import type { Link, OutlineEntry } from "../api";
import { byDocument, linkStatusLabel, linkText, outlineTree, statusCounts } from "./structure";

function heading(path: string, headingPath: string, level: number, lineFrom: number): OutlineEntry {
  const parts = headingPath.split(" / ");
  return {
    path,
    heading: parts[parts.length - 1]!,
    headingPath,
    level,
    lineFrom,
    lineTo: lineFrom + 1,
    nwords: 1,
    nwordsTotal: 1,
    nbytes: 10,
    nlines: 20,
    fileNwords: 5,
    version: 1,
    updated_at: "2026-09-18T10:00:00.000Z",
    updatedBy: "human",
  };
}

const paths = (nodes: ReturnType<typeof outlineTree>): unknown =>
  nodes.map((n) => (n.children.length ? { [n.entry.heading]: paths(n.children) } : n.entry.heading));

describe("outlineTree", () => {
  it("nests each heading under the nearest shallower one above it", () => {
    const tree = outlineTree([
      heading("/a.md", "Review", 1, 1),
      heading("/a.md", "Review / Goals", 2, 5),
      heading("/a.md", "Review / Goals / Detail", 3, 7),
      heading("/a.md", "Review / Next steps", 2, 12),
    ]);
    expect(paths(tree)).toEqual([{ Review: [{ Goals: ["Detail"] }, "Next steps"] }]);
  });

  it("keeps a document whose headings skip levels or never start at one", () => {
    // `##` then `####` is what a writer typed, and the deeper one still belongs under it.
    const tree = outlineTree([heading("/a.md", "Body", 2, 1), heading("/a.md", "Body / Aside", 4, 3), heading("/a.md", "After", 2, 9)]);
    expect(paths(tree)).toEqual([{ Body: ["Aside"] }, "After"]);
  });

  it("loses no heading, whatever the levels do", () => {
    const rows = [heading("/a.md", "A", 3, 1), heading("/a.md", "B", 1, 2), heading("/a.md", "C", 6, 3), heading("/a.md", "D", 2, 4)];
    const count = (nodes: ReturnType<typeof outlineTree>): number => nodes.reduce((n, node) => n + 1 + count(node.children), 0);
    expect(count(outlineTree(rows))).toBe(rows.length);
    expect(outlineTree([])).toEqual([]);
  });
});

describe("byDocument", () => {
  it("groups rows by their file, in the order the files first appear", () => {
    const rows = [heading("/b.md", "One", 1, 1), heading("/a.md", "Two", 1, 1), heading("/b.md", "Three", 2, 4)];
    expect(byDocument(rows).map((d) => [d.path, d.rows.length])).toEqual([
      ["/b.md", 2],
      ["/a.md", 1],
    ]);
  });
});

function link(partial: Partial<Link>): Link {
  return {
    path: "/a.md",
    version: 3,
    line: 12,
    kind: "wiki",
    target: "notes/plan",
    anchor: null,
    alias: null,
    status: "ok",
    resolved: "/notes/plan.md",
    asset: false,
    ...partial,
  };
}

describe("linkText", () => {
  it("writes the link back the way it is written in the document", () => {
    expect(linkText(link({}))).toBe("[[notes/plan]]");
    expect(linkText(link({ anchor: "Goals", alias: "the plan" }))).toBe("[[notes/plan#Goals|the plan]]");
    expect(linkText(link({ kind: "embed" }))).toBe("![[notes/plan]]");
    expect(linkText(link({ kind: "md", target: "./plan.md", alias: "the plan" }))).toBe("[the plan](./plan.md)");
    expect(linkText(link({ kind: "image", target: "img/a.png", alias: null }))).toBe("![](img/a.png)");
  });

  it("writes a bare URL or an autolink as what was typed, not as an empty markdown link", () => {
    // `<me@e.com>` and a linkified `https://e.com` come back as `md` links with no text of their
    // own; `[](e.com)` would be a line nobody can find, and these are most of the `external` rows.
    expect(linkText(link({ kind: "md", target: "me@e.com", alias: null, status: "external" }))).toBe("me@e.com");
    expect(linkText(link({ kind: "md", target: "https://e.com", alias: null, status: "external" }))).toBe("https://e.com");
    // An anchor still belongs to it, and a markdown link that does have text keeps its shape.
    expect(linkText(link({ kind: "md", target: "plan.md", anchor: "Goals", alias: null }))).toBe("plan.md#Goals");
    expect(linkText(link({ kind: "md", target: "plan.md", anchor: "Goals", alias: "the plan" }))).toBe("[the plan](plan.md#Goals)");
  });
});

describe("statusCounts", () => {
  it("counts the statuses present, worst first", () => {
    const links = [
      link({ status: "ok" }),
      link({ status: "external" }),
      link({ status: "broken" }),
      link({ status: "ok" }),
      link({ status: "ambiguous" }),
    ];
    expect(statusCounts(links)).toEqual([
      { status: "broken", n: 1 },
      { status: "ambiguous", n: 1 },
      { status: "ok", n: 2 },
      { status: "external", n: 1 },
    ]);
    expect(statusCounts([])).toEqual([]);
  });

  it("puts a status it has never heard of after the ones it can rank, not above broken", () => {
    const rows = [link({ status: "folder" as "broken" }), link({ status: "broken" }), link({ status: null })];
    expect(statusCounts(rows).map((c) => c.status)).toEqual(["broken", "folder", null]);
  });
});

describe("linkStatusLabel", () => {
  it("says what each status means, and says something for one it has never heard of", () => {
    expect(linkStatusLabel("broken").tone).toBe("problem");
    expect(linkStatusLabel("external").tone).toBe("ok");
    expect(linkStatusLabel(null).label).toBe("unresolved");
    // A status a newer store answers with is shown as it arrived, not dropped.
    expect(linkStatusLabel("folder" as "broken").label).toBe("folder");
  });
});
