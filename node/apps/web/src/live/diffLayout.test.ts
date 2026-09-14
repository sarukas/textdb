import { describe, expect, it } from "vitest";
import { parseDiffLayout, readDiffLayout } from "./diffLayout";

describe("diff layout", () => {
  it("is side by side unless unified was chosen", () => {
    expect(parseDiffLayout(null)).toBe("split");
    expect(parseDiffLayout("split")).toBe("split");
    expect(parseDiffLayout("unified")).toBe("unified");
    expect(parseDiffLayout("garbage")).toBe("split");
  });

  it("falls back to side by side without storage", () => {
    expect(readDiffLayout()).toBe("split");
  });
});
