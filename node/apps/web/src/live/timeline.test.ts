import { describe, expect, it } from "vitest";
import type { PathEvent } from "../api";
import { describePathEvent, timeline } from "./timeline";

const event = (ts: string, extra: Partial<PathEvent> = {}): PathEvent => ({
  id: 1,
  ts,
  op: "rename",
  old_path: "/a/old.md",
  new_path: "/a/new.md",
  via: null,
  version: 1,
  author: "human",
  ...extra,
});

describe("timeline", () => {
  it("orders versions and path events by time, versions first on a tie", () => {
    const versions = [
      { version: 1, ts: "2026-09-13T10:00:00.000Z" },
      { version: 2, ts: "2026-09-13T12:00:00.000Z" },
    ];
    const items = timeline(versions, [event("2026-09-13T11:00:00.000Z"), event("2026-09-13T12:00:00.000Z", { op: "delete" })]);
    expect(items.map((i) => (i.kind === "version" ? `v${i.entry.version}` : i.event.op))).toEqual(["v1", "rename", "v2", "delete"]);
  });
});

describe("describePathEvent", () => {
  it("says what happened and whether a folder took it along", () => {
    expect(describePathEvent(event("t"))).toBe("Renamed from /a/old.md");
    expect(describePathEvent(event("t", { op: "move", via: "/a" }))).toBe("Moved with folder /a, from /a/old.md");
    expect(describePathEvent(event("t", { op: "delete", new_path: null }))).toBe("Deleted");
    expect(describePathEvent(event("t", { op: "delete", new_path: null, via: "/a" }))).toBe("Deleted with folder /a");
  });
});
