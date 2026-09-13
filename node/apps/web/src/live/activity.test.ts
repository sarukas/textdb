import { describe, expect, it } from "vitest";
import type { ChangeEvent, ChangeOp } from "../api";
import { addToFeed, commonFolder } from "./activity";

let seq = 0;
function change(op: ChangeOp, path: string, extra: Partial<ChangeEvent> = {}): ChangeEvent {
  seq++;
  const folder = op === "mkdir";
  return {
    seq,
    ts: `2026-09-13T10:00:${String(seq % 60).padStart(2, "0")}.000Z`,
    op,
    path,
    old_path: null,
    node_kind: folder ? "folder" : "file",
    version: folder ? null : 1,
    base_version: null,
    commit_kind: folder ? null : "direct",
    author: folder ? null : "human",
    message: null,
    ...extra,
  };
}
const imported = (path: string, author = "human") => change("create", path, { author, message: "import" });

describe("addToFeed", () => {
  it("keeps ordinary changes as their own rows, newest first", () => {
    const feed = addToFeed([], [change("create", "/a.md"), change("commit", "/a.md", { author: "agent-7" })], 50);
    expect(feed.map((i) => i.kind)).toEqual(["event", "event"]);
    expect(feed[0]).toMatchObject({ event: { author: "agent-7" } });
  });

  it("collapses an import and the folders it created into one row", () => {
    const events = [change("mkdir", "/docs"), imported("/docs/a.md"), change("mkdir", "/docs/sub"), imported("/docs/sub/b.md"), imported("/docs/c.md")];
    const feed = addToFeed([], events, 50);
    expect(feed).toHaveLength(1);
    expect(feed[0]).toMatchObject({ kind: "import", files: 3, folders: 2, prefix: "/docs", author: "human", first: { path: "/docs/a.md" } });
  });

  it("keeps counting past the feed cap, across flushes", () => {
    let feed = addToFeed([], [change("commit", "/other.md", { author: "agent-7" })], 5);
    for (let i = 0; i < 10; i++) {
      feed = addToFeed(feed, Array.from({ length: 100 }, (_, j) => imported(`/big/${i}/${j}.md`)), 5);
    }
    expect(feed).toHaveLength(2);
    expect(feed[0]).toMatchObject({ kind: "import", files: 1000, prefix: "/big" });
    expect(feed[1]).toMatchObject({ kind: "event", event: { path: "/other.md" } });
  });

  it("starts a new row when someone else writes in between, or another author imports", () => {
    const feed = addToFeed(
      [],
      [imported("/docs/a.md"), change("commit", "/docs/a.md", { author: "agent-7" }), imported("/docs/b.md"), imported("/docs/c.md", "agent-9")],
      50,
    );
    expect(feed.map((i) => (i.kind === "import" ? `import:${i.author}:${i.files}` : "event"))).toEqual([
      "import:agent-9:1",
      "import:human:1",
      "event",
      "import:human:1",
    ]);
  });
});

describe("commonFolder", () => {
  it("finds the deepest shared folder", () => {
    expect(commonFolder("/docs/a", "/docs/b/c")).toBe("/docs");
    expect(commonFolder("/docs", "/docs/sub")).toBe("/docs");
    expect(commonFolder("/x", "/y")).toBe("/");
  });
});
